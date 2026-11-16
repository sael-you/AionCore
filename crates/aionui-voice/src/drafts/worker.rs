//! Draft worker (§3.3.6–§3.3.7): one background loop that retries queued
//! drafts, decides started/not-started after a send, expires stale drafts and
//! recovers after a restart. The database is the store of work; events only
//! wake the loop early. Every action takes the draft's lease first.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use aionui_db::VoiceDraftRow;
use tokio::sync::Notify;

use super::engine::{DraftEngine, START_CHECK_MS};

/// Fallback tick (queued retries, start checks, expiry).
pub const WORKER_TICK: Duration = Duration::from_secs(2);
/// Recovery sweep period (§3.3.7: startup and every 30 s).
pub const RECOVERY_EVERY: Duration = Duration::from_secs(30);
/// Retention purge period (transcripts 7 days, draft texts 30 days, §3.3.8).
pub const PURGE_EVERY: Duration = Duration::from_secs(3600);
const DRAFT_TEXT_RETENTION_MS: i64 = 30 * 24 * 3600 * 1000;
const LEASE_MS: i64 = 15_000;

/// What the worker needs from the service: which sessions have a live
/// runner in this process (their holds are the runner's), and when this
/// process started (a hold that fell due before is `interrupted`).
pub trait LiveSessions: Send + Sync {
    fn is_live(&self, session_id: &str) -> bool;
}

pub struct DraftWorker {
    pub engine: Arc<DraftEngine>,
    pub live: Arc<dyn LiveSessions>,
    pub owner: String,
    pub started_at_ms: i64,
    pub wake: Arc<Notify>,
}

fn now_ms() -> i64 {
    aionui_common::now_ms()
}

impl DraftWorker {
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(self) {
        let mut last_recovery = tokio::time::Instant::now() - RECOVERY_EVERY;
        let mut last_purge = tokio::time::Instant::now() - PURGE_EVERY;
        loop {
            let recovery = last_recovery.elapsed() >= RECOVERY_EVERY;
            if recovery {
                last_recovery = tokio::time::Instant::now();
            }
            self.sweep(recovery).await;
            if last_purge.elapsed() >= PURGE_EVERY {
                last_purge = tokio::time::Instant::now();
                self.purge().await;
            }
            tokio::select! {
                _ = tokio::time::sleep(WORKER_TICK) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    async fn lease(&self, row: &VoiceDraftRow) -> bool {
        let now = now_ms();
        self.engine
            .repo
            .claim_draft_lease(
                &row.user_id,
                &row.id,
                row.version,
                &row.state,
                &self.owner,
                now + LEASE_MS,
                now,
            )
            .await
            .unwrap_or(false)
    }

    /// Delete expired transcripts and blank draft texts past retention.
    pub async fn purge(&self) {
        let now = now_ms();
        match self
            .engine
            .repo
            .purge_voice_data(now, now - DRAFT_TEXT_RETENTION_MS)
            .await
        {
            Ok(rows) if rows > 0 => tracing::info!(rows, "voice retention purge"),
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "voice retention purge failed"),
        }
    }

    /// One pass. `recovery` also handles `sending` drafts without an outcome
    /// and holds whose runner is gone.
    pub async fn sweep(&self, recovery: bool) {
        let states = [
            "queued",
            "sending",
            "drafted",
            "ambiguous",
            "pending_transcript",
            "committing",
        ];
        let mut rows = match self.engine.repo.drafts_in_states(&states).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "voice draft worker: listing drafts failed");
                return;
            }
        };
        // Conversation-mode messages go oldest first (FIFO by creation; a
        // re-queue bumps `updated_at`); C9 drafts keep their order.
        rows.sort_by_key(|r| {
            if super::engine::is_voice_mode(r) {
                (1, r.created_at)
            } else {
                (0, 0)
            }
        });
        let mut seen = HashSet::new();
        for row in rows {
            if !seen.insert((row.id.clone(), row.version)) {
                continue;
            }
            let now = now_ms();
            let live = row.session_id.as_deref().is_some_and(|s| self.live.is_live(s));
            match row.state.as_str() {
                "queued" if self.lease(&row).await => {
                    self.engine.retry_queued(&row).await;
                }
                "sending" => {
                    let started = row.send_started_at.unwrap_or(row.updated_at);
                    if row.outcome_msg_id.is_some() {
                        if now - started >= START_CHECK_MS && self.lease(&row).await {
                            self.engine.check_started(&row).await;
                        }
                    } else if (recovery || now - row.updated_at >= LEASE_MS) && self.lease(&row).await {
                        // Crashed between the claim and the send (or the send
                        // never answered): re-send with the same key — safe.
                        tracing::info!(draft_id = %row.id, "voice draft worker re-sending with the same key");
                        let fresh = self
                            .engine
                            .repo
                            .get_draft(&row.user_id, &row.id, row.version)
                            .await
                            .ok()
                            .flatten();
                        if let Some(fresh) = fresh {
                            self.engine.deliver(&fresh).await;
                        }
                    }
                }
                "committing" if !live => {
                    let deadline = row.hold_deadline_at.unwrap_or(row.updated_at);
                    if deadline < self.started_at_ms {
                        // The cancel window fell inside the downtime: the user
                        // lost it, so the draft is not sent (§3.3.7).
                        self.engine.interrupt(&row).await;
                    } else if deadline <= now && self.lease(&row).await {
                        self.engine.start_send(&row).await;
                    }
                }
                "drafted" | "ambiguous" | "pending_transcript" if row.expires_at.is_some_and(|t| t <= now) => {
                    self.engine.expire(&row).await;
                }
                _ => {}
            }
        }
    }
}
