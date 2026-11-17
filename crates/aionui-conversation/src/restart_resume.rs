//! TASK-44: turns interrupted by an AionUi restart.
//!
//! Every deploy restarts AionUi; a turn that was running then is gone (the
//! CLI process dies with AionCore) and so is any approval it waited for. This
//! module makes that visible and lets the conversation carry on:
//!
//! 1. **Shutdown snapshot.** At the shutdown signal AionCore writes the turns
//!    that are running, with the approvals they wait for, to
//!    `<data_dir>/runtime/interrupted-turns.json` — first immediately (the web
//!    host hard-kills AionCore a few seconds after SIGTERM), then again after
//!    the drain with the turns that are still running.
//! 2. **Startup.** The snapshot and the stale runtime rows that
//!    `startup_recovery` settles (a tool row left "running" proves an
//!    interrupted turn even after a crash without a snapshot) become one
//!    [`RestartInterrupted`] record per conversation in
//!    `conversations.extra.restart_interrupted`.
//! 3. **Continue.** The owner's Continue button, or AionCore itself when the
//!    conversation's `auto_continue_after_restart` switch is on, sends ONE
//!    resume message that says what was interrupted and tells the agent to
//!    check the real state first. The resume goes through the idempotent send
//!    path keyed by the record's `at`, so one restart can never produce two.
//!
//! Safety rules of the automatic path (owner, 2026-11-17): an approval is
//! never given and its command never re-run by AionCore (the agent is told
//! the request was NOT approved); a step that writes to production, pays,
//! mails, pushes or deploys is never continued automatically; a step that was
//! already interrupted and continued once is not continued again; at most
//! [`AUTO_CONTINUE_PARALLEL`] conversations resume at a time.
//!
//! What each CLI records for the killed step is NOT relied on. Claude Code
//! writes its own "Tool call interrupted… outcome is unknown" result on
//! `--resume` (seen in 10 real sessions killed by deploys, 2026-11-15/16);
//! for Codex and Gemini there is no such evidence, so the resume message
//! always states the interruption itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    AutoContinueSetting, EXTRA_AUTO_CONTINUE_AFTER_RESTART, EXTRA_RESTART_INTERRUPTED, RestartAutoBlocked,
    RestartInterrupted, RestartInterruptedKind, RestartInterruptedState, RestartResumeAction, RestartResumeResponse,
    SendMessageRequest,
};
use aionui_common::{ErrorChain, now_ms};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::ConversationError;
use crate::runtime_state::ConversationRuntimeStateService;
use crate::service::ConversationService;

/// Conversations AionCore resumes by itself at the same time.
pub const AUTO_CONTINUE_PARALLEL: usize = 2;
/// Wait after startup before the first automatic resume (agents, MCP and the
/// Token Pool come up first).
pub const AUTO_CONTINUE_START_DELAY: Duration = Duration::from_secs(15);
const AUTO_CONTINUE_STAGGER: Duration = Duration::from_secs(3);
const AUTO_CONTINUE_POLL: Duration = Duration::from_secs(2);
/// An automatic resume that is still running after this no longer blocks the
/// next one (the limit is about the start burst, not about long turns).
const AUTO_CONTINUE_SLOT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SUMMARY_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Shutdown snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownSnapshot {
    pub at: i64,
    pub turns: Vec<SnapshotTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTurn {
    pub conversation_id: String,
    pub turn_id: String,
    /// Approvals the turn was waiting for (title + what it asked to run).
    #[serde(default)]
    pub approvals: Vec<SnapshotApproval>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotApproval {
    pub title: String,
    pub summary: String,
}

pub fn snapshot_path(data_dir: &Path) -> PathBuf {
    data_dir.join("runtime").join("interrupted-turns.json")
}

fn clip(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > SUMMARY_CHARS {
        let mut out: String = flat.chars().take(SUMMARY_CHARS - 1).collect();
        out.push('…');
        out
    } else {
        flat
    }
}

/// The running turns and the approvals they wait for, right now.
pub fn build_snapshot(
    runtime_state: &ConversationRuntimeStateService,
    task_manager: &Arc<dyn IWorkerTaskManager>,
) -> ShutdownSnapshot {
    let turns = runtime_state
        .active_turns()
        .into_iter()
        .map(|(conversation_id, turn_id)| {
            let approvals = task_manager
                .get_task(&conversation_id)
                .map(|agent| agent.get_confirmations())
                .unwrap_or_default()
                .into_iter()
                .map(|c| SnapshotApproval {
                    title: clip(c.title.as_deref().or(c.action.as_deref()).unwrap_or("approval")),
                    summary: clip(&c.description),
                })
                .collect();
            SnapshotTurn {
                conversation_id,
                turn_id,
                approvals,
            }
        })
        .collect();
    ShutdownSnapshot { at: now_ms(), turns }
}

/// Write the snapshot atomically (temp file + rename), owner-only.
pub fn write_snapshot(path: &Path, snapshot: &ShutdownSnapshot) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("snapshot path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".interrupted-turns.{}.tmp", std::process::id()));
    let body = serde_json::to_vec(snapshot).map_err(std::io::Error::other)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &body))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Read and remove the snapshot of the previous run (empty when there is none
/// or it does not parse).
pub fn take_snapshot(path: &Path) -> ShutdownSnapshot {
    let snapshot = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            warn!(error = %error, "restart resume: shutdown snapshot unreadable; ignored");
            ShutdownSnapshot::default()
        }),
        Err(_) => ShutdownSnapshot::default(),
    };
    let _ = std::fs::remove_file(path);
    snapshot
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// A runtime row a previous process left unfinished (from `startup_recovery`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleStep {
    pub user_id: String,
    pub conversation_id: String,
    pub row_type: String,
    pub content: String,
}

/// Tool name and what it was asked to do, from a stale tool row.
fn tool_of(step: &StaleStep) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(&step.content).ok()?;
    let entry = match step.row_type.as_str() {
        "tool_call" => &value,
        // The last entry of a group that is not finished.
        "tool_group" => value.as_array()?.iter().rev().find(|e| {
            e.get("status")
                .and_then(|s| s.as_str())
                .is_none_or(|s| !matches!(s, "Success" | "Error" | "Canceled"))
        })?,
        _ => return None,
    };
    let name = entry
        .get("name")
        .or_else(|| entry.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("tool");
    let args = entry.get("args").or_else(|| entry.get("input"));
    let summary = args
        .and_then(|a| a.get("command").and_then(|c| c.as_str()).map(str::to_owned))
        .or_else(|| args.filter(|a| !a.is_null()).map(|a| a.to_string()))
        .or_else(|| entry.get("description").and_then(|d| d.as_str()).map(str::to_owned))
        .unwrap_or_default();
    Some((clip(name), clip(&summary)))
}

/// Substrings that mark a step AionCore must not continue by itself: it
/// writes to production, pays, mails, runs ads, pushes, publishes or deploys.
const SENSITIVE_MARKERS: &[&str] = &[
    "git push",
    "ship.sh",
    "deploy",
    "publish",
    "production",
    "--prod",
    "kubectl",
    "terraform",
    "helm ",
    "stripe",
    "paypal",
    "payment",
    "refund",
    "invoice",
    "send_email",
    "send-email",
    "sendmail",
    "send_message",
    "gmail",
    "smtp",
    "mailgun",
    "google_ads",
    "googleads",
    "meta_ads",
    "ads_",
    "campaign",
    "drop table",
    "delete from",
    "truncate ",
    "execute_sql",
    "apply_migration",
    "rm -rf",
];

/// MCP tools: any verb that changes something on the other side.
const MCP_WRITE_VERBS: &[&str] = &[
    "create", "update", "delete", "insert", "write", "send", "post", "apply", "execute", "merge", "push", "remove",
    "set_", "upload", "pay",
];

/// Whether the interrupted step must never be continued automatically.
pub fn is_sensitive_step(tool: Option<&str>, summary: Option<&str>) -> bool {
    let tool = tool.unwrap_or_default().to_ascii_lowercase();
    let text = format!("{tool} {}", summary.unwrap_or_default().to_ascii_lowercase());
    if SENSITIVE_MARKERS.iter().any(|m| text.contains(m)) {
        return true;
    }
    tool.starts_with("mcp__") && MCP_WRITE_VERBS.iter().any(|v| tool.contains(v))
}

/// What one conversation was doing, from its snapshot entry and stale rows.
fn classify(
    snapshot: Option<&SnapshotTurn>,
    stale: &[&StaleStep],
) -> (RestartInterruptedKind, Option<String>, Option<String>) {
    if let Some(approval) = snapshot.and_then(|t| t.approvals.first()) {
        return (
            RestartInterruptedKind::Approval,
            Some(approval.title.clone()),
            Some(approval.summary.clone()).filter(|s| !s.is_empty()),
        );
    }
    if let Some((name, summary)) = stale.iter().rev().find_map(|s| tool_of(s)) {
        return (
            RestartInterruptedKind::Tool,
            Some(name),
            Some(summary).filter(|s| !s.is_empty()),
        );
    }
    (RestartInterruptedKind::Text, None, None)
}

/// The one message that resumes an interrupted conversation.
pub fn resume_message(record: &RestartInterrupted) -> String {
    let step = match (&record.tool, &record.summary) {
        (Some(tool), Some(summary)) => format!("`{tool}: {summary}`"),
        (Some(tool), None) => format!("`{tool}`"),
        _ => "your last step".to_owned(),
    };
    match record.kind {
        RestartInterruptedKind::Approval => format!(
            "AionUi restarted while you were waiting for an approval. Your request to run {step} was NOT approved \
             and did not run. Do not assume it happened. If it is still needed, ask for it again; otherwise continue \
             with the task."
        ),
        RestartInterruptedKind::Tool => format!(
            "AionUi restarted while {step} was running. It may have run fully, partly or not at all. Before anything \
             else, check the real state (files, git status, running processes, the remote system) and re-run only \
             what is missing. Do not blindly repeat a step that changes something outside this workspace; if you \
             cannot tell whether it took effect, ask me. Then continue with the task."
        ),
        RestartInterruptedKind::Text => {
            "AionUi restarted while you were answering. Nothing was running. Continue where you stopped.".to_owned()
        }
    }
}

fn parse_record(extra: &str) -> Option<RestartInterrupted> {
    let value: serde_json::Value = serde_json::from_str(extra).ok()?;
    serde_json::from_value(value.get(EXTRA_RESTART_INTERRUPTED)?.clone()).ok()
}

fn auto_enabled(extra: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(extra)
        .ok()
        .and_then(|v| v.get(EXTRA_AUTO_CONTINUE_AFTER_RESTART).and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// A conversation AionCore may continue by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoCandidate {
    pub user_id: String,
    pub conversation_id: String,
}

impl ConversationService {
    /// Turn the previous run's snapshot and stale rows into one
    /// `restart_interrupted` record per conversation. Returns the
    /// conversations whose switch allows an automatic continue.
    pub async fn record_restart_interruptions(
        &self,
        snapshot: ShutdownSnapshot,
        stale: Vec<StaleStep>,
    ) -> Vec<AutoCandidate> {
        let at = now_ms();
        let mut by_conversation: HashMap<&str, Vec<&StaleStep>> = HashMap::new();
        for step in &stale {
            by_conversation.entry(&step.conversation_id).or_default().push(step);
        }
        let mut ids: Vec<&str> = snapshot.turns.iter().map(|t| t.conversation_id.as_str()).collect();
        ids.extend(by_conversation.keys().copied());
        ids.sort_unstable();
        ids.dedup();

        let mut candidates = Vec::new();
        for id in ids {
            let empty = Vec::new();
            let steps = by_conversation.get(id).unwrap_or(&empty);
            let user_id = match steps.first() {
                Some(step) => Some(step.user_id.clone()),
                None => self.conversation_repo().owner_user_id(id).await.ok().flatten(),
            };
            let Some(user_id) = user_id else { continue };
            let row = match self.conversation_repo().get(&user_id, id).await {
                Ok(Some(row)) => row,
                _ => continue,
            };
            let turn = snapshot.turns.iter().find(|t| t.conversation_id == id);
            let (kind, tool, summary) = classify(turn, steps);
            // The same step again, after it was already continued once.
            let repeat = parse_record(&row.extra)
                .filter(|prev| {
                    prev.kind == kind
                        && prev.tool == tool
                        && prev.summary == summary
                        && matches!(
                            prev.state,
                            RestartInterruptedState::Continued | RestartInterruptedState::AutoContinued
                        )
                })
                .map_or(0, |prev| prev.repeat + 1);
            let auto = auto_enabled(&row.extra);
            let auto_blocked = if !auto {
                None
            } else if is_sensitive_step(tool.as_deref(), summary.as_deref()) {
                Some(RestartAutoBlocked::Sensitive)
            } else if repeat > 0 {
                Some(RestartAutoBlocked::Repeat)
            } else {
                None
            };
            let record = RestartInterrupted {
                at,
                kind,
                tool,
                summary,
                state: RestartInterruptedState::Waiting,
                repeat,
                auto_blocked,
            };
            if let Err(error) = self.write_restart_record(&user_id, id, &record).await {
                warn!(conversation_id = id, error = %ErrorChain(&error), "restart resume: record not saved");
                continue;
            }
            info!(
                conversation_id = id,
                kind = ?record.kind,
                repeat,
                auto,
                auto_blocked = ?record.auto_blocked,
                "restart resume: turn interrupted by the restart"
            );
            if auto && record.auto_blocked.is_none() {
                candidates.push(AutoCandidate {
                    user_id,
                    conversation_id: id.to_owned(),
                });
            }
        }
        candidates
    }

    async fn write_restart_record(
        &self,
        user_id: &str,
        conversation_id: &str,
        record: &RestartInterrupted,
    ) -> Result<(), ConversationError> {
        let value = serde_json::to_value(record).map_err(|e| ConversationError::internal(e.to_string()))?;
        let matched = self
            .conversation_repo()
            .set_extra_keys(
                user_id,
                conversation_id,
                &[(EXTRA_RESTART_INTERRUPTED, value)],
                None,
                None,
            )
            .await?;
        if !matched {
            return Err(ConversationError::NotFound {
                id: conversation_id.to_owned(),
            });
        }
        self.broadcast_list_changed(user_id, conversation_id, "updated", None);
        Ok(())
    }

    async fn restart_record(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<RestartInterrupted, ConversationError> {
        let row = self
            .conversation_repo()
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;
        parse_record(&row.extra).ok_or_else(|| ConversationError::BadRequest {
            reason: "this conversation was not interrupted by a restart".into(),
        })
    }

    /// The owner's Continue or Dismiss on the "Interrupted by a restart" notice.
    pub async fn restart_resume(
        &self,
        user_id: &str,
        conversation_id: &str,
        action: RestartResumeAction,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<RestartResumeResponse, ConversationError> {
        match action {
            RestartResumeAction::Dismiss => {
                let mut record = self.restart_record(user_id, conversation_id).await?;
                if record.state == RestartInterruptedState::Waiting {
                    record.state = RestartInterruptedState::Dismissed;
                    self.write_restart_record(user_id, conversation_id, &record).await?;
                }
                Ok(RestartResumeResponse {
                    restart_interrupted: record,
                    msg_id: None,
                    turn_id: None,
                })
            }
            RestartResumeAction::Continue => {
                self.continue_after_restart(
                    user_id,
                    conversation_id,
                    RestartInterruptedState::Continued,
                    task_manager,
                )
                .await
            }
        }
    }

    /// Send the resume message once (idempotent per restart) and mark the record.
    async fn continue_after_restart(
        &self,
        user_id: &str,
        conversation_id: &str,
        new_state: RestartInterruptedState,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<RestartResumeResponse, ConversationError> {
        let mut record = self.restart_record(user_id, conversation_id).await?;
        if record.state != RestartInterruptedState::Waiting {
            return Err(ConversationError::BadRequest {
                reason: "this interruption was already continued or dismissed".into(),
            });
        }
        let request = SendMessageRequest {
            content: resume_message(&record),
            files: Vec::new(),
            sessions: Vec::new(),
            inject_skills: Vec::new(),
            hidden: false,
        };
        // One resume per restart, whoever asks (button and automatic path).
        let key = format!("restart-resume:{conversation_id}:{}", record.at);
        let sent = self
            .send_message_idempotent(user_id, conversation_id, request, task_manager, &key)
            .await?;
        record.state = new_state;
        self.write_restart_record(user_id, conversation_id, &record).await?;
        info!(conversation_id, state = ?new_state, "restart resume: conversation continued");
        Ok(RestartResumeResponse {
            restart_interrupted: record,
            msg_id: Some(sent.msg_id),
            turn_id: Some(sent.turn_id),
        })
    }

    /// The per-conversation "continue by itself after a restart" switch.
    pub async fn set_auto_continue_after_restart(
        &self,
        user_id: &str,
        conversation_id: &str,
        enabled: bool,
    ) -> Result<AutoContinueSetting, ConversationError> {
        let matched = self
            .conversation_repo()
            .set_extra_keys(
                user_id,
                conversation_id,
                &[(EXTRA_AUTO_CONTINUE_AFTER_RESTART, serde_json::Value::Bool(enabled))],
                None,
                None,
            )
            .await?;
        if !matched {
            return Err(ConversationError::NotFound {
                id: conversation_id.to_owned(),
            });
        }
        self.broadcast_list_changed(user_id, conversation_id, "updated", None);
        Ok(AutoContinueSetting { enabled })
    }

    /// Continue the candidates by themselves: after a start delay, at most
    /// [`AUTO_CONTINUE_PARALLEL`] at a time, staggered. A failure leaves the
    /// record waiting for the owner.
    pub async fn auto_continue_after_restart(
        &self,
        candidates: Vec<AutoCandidate>,
        task_manager: Arc<dyn IWorkerTaskManager>,
        start_delay: Duration,
    ) {
        if candidates.is_empty() {
            return;
        }
        tokio::time::sleep(start_delay).await;
        let mut running: Vec<(String, tokio::time::Instant)> = Vec::new();
        let last = candidates.len().saturating_sub(1);
        for (index, candidate) in candidates.into_iter().enumerate() {
            loop {
                running.retain(|(id, started)| {
                    self.runtime_state().is_claimed(id) && started.elapsed() < AUTO_CONTINUE_SLOT_TIMEOUT
                });
                if running.len() < AUTO_CONTINUE_PARALLEL {
                    break;
                }
                tokio::time::sleep(AUTO_CONTINUE_POLL).await;
            }
            match self
                .continue_after_restart(
                    &candidate.user_id,
                    &candidate.conversation_id,
                    RestartInterruptedState::AutoContinued,
                    &task_manager,
                )
                .await
            {
                Ok(_) => running.push((candidate.conversation_id, tokio::time::Instant::now())),
                Err(error) => warn!(
                    conversation_id = %candidate.conversation_id,
                    error = %ErrorChain(&error),
                    "restart resume: automatic continue failed; waiting for the owner"
                ),
            }
            if index < last {
                tokio::time::sleep(AUTO_CONTINUE_STAGGER).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(row_type: &str, content: serde_json::Value) -> StaleStep {
        StaleStep {
            user_id: "u1".into(),
            conversation_id: "c1".into(),
            row_type: row_type.into(),
            content: content.to_string(),
        }
    }

    #[test]
    fn snapshot_round_trips_and_is_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = snapshot_path(dir.path());
        assert_eq!(take_snapshot(&path), ShutdownSnapshot::default());
        let snapshot = ShutdownSnapshot {
            at: 5,
            turns: vec![SnapshotTurn {
                conversation_id: "c1".into(),
                turn_id: "t1".into(),
                approvals: vec![SnapshotApproval {
                    title: "Bash".into(),
                    summary: "rm build".into(),
                }],
            }],
        };
        write_snapshot(&path, &snapshot).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(take_snapshot(&path), snapshot);
        assert_eq!(
            take_snapshot(&path),
            ShutdownSnapshot::default(),
            "removed after the first read"
        );
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(take_snapshot(&path), ShutdownSnapshot::default());
    }

    #[test]
    fn classification_prefers_approval_then_tool_then_text() {
        let tool = step(
            "tool_call",
            serde_json::json!({"name": "Bash", "args": {"command": "cargo   test\n-p x"}, "status": "running"}),
        );
        let turn = SnapshotTurn {
            conversation_id: "c1".into(),
            turn_id: "t".into(),
            approvals: vec![SnapshotApproval {
                title: "Write".into(),
                summary: "/etc/hosts".into(),
            }],
        };
        assert_eq!(
            classify(Some(&turn), &[&tool]),
            (
                RestartInterruptedKind::Approval,
                Some("Write".into()),
                Some("/etc/hosts".into())
            )
        );
        assert_eq!(
            classify(None, &[&tool]),
            (
                RestartInterruptedKind::Tool,
                Some("Bash".into()),
                Some("cargo test -p x".into())
            )
        );
        let group = step(
            "tool_group",
            serde_json::json!([
                {"name": "Read", "status": "Success"},
                {"name": "Edit", "args": {"file_path": "a.rs"}, "status": "Executing"}
            ]),
        );
        let (kind, name, _) = classify(None, &[&group]);
        assert_eq!((kind, name.as_deref()), (RestartInterruptedKind::Tool, Some("Edit")));
        let text = step("text", serde_json::json!({"content": "half"}));
        assert_eq!(classify(None, &[&text]), (RestartInterruptedKind::Text, None, None));
        let long = clip(&"x".repeat(500));
        assert_eq!(long.chars().count(), SUMMARY_CHARS);
    }

    #[test]
    fn sensitive_steps_are_never_automatic() {
        for (tool, summary) in [
            ("Bash", "git push origin youssef"),
            ("Bash", "tools/ship.sh train9"),
            ("Bash", "vercel deploy --prod"),
            ("Bash", "psql -c 'DELETE FROM users'"),
            ("mcp__supabase__execute_sql", "select 1"),
            ("mcp__stripe__create_refund", "re_1"),
            ("mcp__gmail__send_email", "to: a@b.c"),
            ("mcp__github__create_pull_request", "title"),
            ("mcp__google_ads__mutate", "pause campaign"),
        ] {
            assert!(is_sensitive_step(Some(tool), Some(summary)), "{tool} {summary}");
        }
        for (tool, summary) in [
            ("Bash", "cargo test -p aionui-conversation"),
            ("Read", "src/main.rs"),
            ("mcp__context7__get_library_docs", "tokio"),
            ("Bash", "git status"),
        ] {
            assert!(!is_sensitive_step(Some(tool), Some(summary)), "{tool} {summary}");
        }
    }

    #[test]
    fn resume_messages_say_what_happened_and_never_approve() {
        let mut record = RestartInterrupted {
            at: 1,
            kind: RestartInterruptedKind::Approval,
            tool: Some("Bash".into()),
            summary: Some("rm -r build".into()),
            state: RestartInterruptedState::Waiting,
            repeat: 0,
            auto_blocked: None,
        };
        let approval = resume_message(&record);
        assert!(approval.contains("NOT approved"), "{approval}");
        assert!(approval.contains("`Bash: rm -r build`"));
        assert!(approval.contains("ask for it again"));
        record.kind = RestartInterruptedKind::Tool;
        let tool = resume_message(&record);
        assert!(tool.contains("fully, partly or not at all"), "{tool}");
        assert!(tool.contains("check the real state"));
        assert!(tool.contains("re-run only what is missing"));
        record.kind = RestartInterruptedKind::Text;
        record.tool = None;
        assert!(resume_message(&record).contains("Continue where you stopped"));
    }
}
