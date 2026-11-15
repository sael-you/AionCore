//! Per-call draft/tool handling for [`CallRunner`]: tool calls (executed once
//! per provider call id), drafts, confirmation evidence, the 4 s hold, and
//! `ask` answers. While a draft awaits confirmation or an `ask` is pending,
//! provider-initiated responses are off (`create_response: false`, §3.1) so
//! "send" never triggers a model turn.

use std::collections::VecDeque;
use std::time::Instant as StdInstant;

use aionui_db::{NewVoiceTranscript, VoiceDraftRow};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::drafts::evidence::{HOLD, is_cancel_phrase};
use crate::drafts::{ConfirmTracker, DraftRequest, DraftState, HoldClock, HoldEvent, OnsetVerdict, TranscriptVerdict};
use crate::readback::ClipVoice;
use crate::runner::CallRunner;
use crate::tools::ToolEffect;

/// Transcripts are kept 7 days (§3.3.8).
const TRANSCRIPT_TTL_MS: i64 = 7 * 24 * 3600 * 1000;
const MAX_FINALS: usize = 4;

/// Events the client reports on `/api/voice/ws` for a live call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ClientEvent {
    PlaybackFinished {
        readback_id: String,
        t_end: f64,
    },
    SpeechOnset {
        t: f64,
    },
    ConfirmTap {
        draft_id: String,
        version: i64,
        hash: String,
    },
    CancelTap {
        draft_id: String,
        version: i64,
    },
    /// An agent `ask` is waiting for this user's next utterance.
    AskPending,
}

#[derive(Default)]
pub(crate) struct TurnState {
    tracker: ConfirmTracker,
    active: Option<VoiceDraftRow>,
    hold: Option<HoldClock>,
    finals: VecDeque<String>,
    tool_refs: Vec<String>,
    tool_snippets: Vec<String>,
    manual: bool,
    turn_no: i64,
}

impl CallRunner {
    /// Provider-initiated responses off/on (acknowledged by the provider).
    async fn set_manual(&mut self, manual: bool) {
        if self.turn.manual == manual {
            return;
        }
        match self.engine_call.set_create_response(!manual).await {
            Ok(()) => self.turn.manual = manual,
            Err(err) => {
                tracing::warn!(session_id = %self.call.session_id, error = %err, "voice create_response toggle failed")
            }
        }
    }

    /// Backend-issued response with a budget reservation; refusal is logged.
    async fn respond(&mut self) {
        if self.meter.reserve_own().is_err() {
            tracing::info!(session_id = %self.call.session_id, "voice response refused: budget");
            return;
        }
        if self.engine_call.create_response().await.is_err() {
            self.meter.release_own();
        }
    }

    pub(crate) async fn on_tool_call(&mut self, call_id: &str, name: &str, arguments: &str) {
        let inner = self.inner.clone();
        let first = inner
            .drafts
            .repo
            .begin_tool_call(&self.call.session_id, call_id, name)
            .await
            .unwrap_or(false);
        if !first {
            tracing::info!(session_id = %self.call.session_id, tool = name, "voice tool call already handled (dedupe)");
            return;
        }
        let started = StdInstant::now();
        let args: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
        let outcome = inner.tools.execute(&self.call.user_id, name, &args).await;
        self.turn.tool_refs.extend(outcome.resolved.iter().cloned());
        if let Some(snippet) = &outcome.snippet {
            self.turn.tool_snippets.push(snippet.clone());
        }
        let output = outcome.output.to_string();
        if let Err(err) = self.engine_call.tool_result(call_id, &output).await {
            tracing::warn!(session_id = %self.call.session_id, error = %err, "voice tool result not delivered");
        }
        let status = match &outcome.effect {
            Some(ToolEffect::ActAttempt { .. }) => "rejected",
            _ if outcome.output.get("error").is_some() => "failed",
            _ => "done",
        };
        let _ = inner
            .drafts
            .repo
            .finish_tool_call(
                &self.call.session_id,
                call_id,
                status,
                started.elapsed().as_millis() as i64,
                output.len() as i64,
            )
            .await;
        tracing::info!(session_id = %self.call.session_id, tool = name, status,
            duration_ms = started.elapsed().as_millis() as u64, result_bytes = output.len(), "voice tool call");

        match outcome.effect {
            None => self.respond().await,
            Some(ToolEffect::Refine { new_conversation }) => self.start_draft(new_conversation).await,
            Some(ToolEffect::Edit { change }) => self.edit_draft(&change).await,
            Some(ToolEffect::ActAttempt { name }) => {
                inner.drafts.reject_act(&name);
                inner
                    .audit(
                        &self.call.user_id,
                        Some(&self.call.session_id),
                        "act_rejected",
                        json!({"tool": name}),
                    )
                    .await;
                self.respond().await;
            }
        }
    }

    async fn start_draft(&mut self, new_conversation: bool) {
        self.set_manual(true).await;
        let transcript = self.turn.finals.iter().cloned().collect::<Vec<_>>().join(" ");
        if transcript.trim().is_empty() {
            self.set_manual(false).await;
            self.respond().await;
            return;
        }
        let request = DraftRequest {
            transcript,
            new_conversation,
            focus: self.call.focus.clone(),
            tool_refs: self.turn.tool_refs.clone(),
            tool_snippets: self.turn.tool_snippets.clone(),
        };
        let drafts = self.inner.drafts.clone();
        let created = drafts
            .create(&self.call.user_id, Some(&self.call.session_id), request)
            .await;
        self.turn.finals.clear();
        self.adopt_draft(created).await;
    }

    async fn edit_draft(&mut self, change: &str) {
        let Some(active) = self.turn.active.clone() else {
            self.respond().await;
            return;
        };
        self.set_manual(true).await;
        let drafts = self.inner.drafts.clone();
        let next = drafts.edit(&active, change).await;
        self.adopt_draft(next).await;
    }

    async fn adopt_draft(&mut self, draft: Option<VoiceDraftRow>) {
        self.turn.hold = None;
        match draft {
            Some(row) => {
                match &row.readback_id {
                    Some(readback) => self.turn.tracker.await_readback(readback),
                    None => self.turn.tracker.reset(),
                }
                if let Some(text) = &row.readback_text
                    && let Err(err) = self.engine_call.add_context(text).await
                {
                    tracing::debug!(error = %err, "voice read-back context not added");
                }
                self.turn.active = Some(row);
            }
            None => {
                self.turn.active = None;
                self.turn.tracker.reset();
                self.set_manual(false).await;
            }
        }
    }

    async fn draft_done(&mut self) {
        self.turn.active = None;
        self.turn.hold = None;
        self.turn.tracker.reset();
        if !self.inner.asks.has_pending(&self.call.user_id) {
            self.set_manual(false).await;
        }
    }

    pub(crate) fn on_user_speech_started(&mut self) {
        let now = Instant::now();
        self.turn.tracker.provider_speech_started(now);
        self.freeze_hold(now);
    }

    /// Speech during the hold freezes its deadline until the transcript.
    fn freeze_hold(&mut self, now: Instant) {
        let Some(hold) = &mut self.turn.hold else {
            return;
        };
        let was_frozen = hold.is_frozen();
        hold.freeze(now);
        if !was_frozen && let Some(row) = &self.turn.active {
            self.inner.drafts.emit(row, self.hold_progress());
        }
    }

    /// A final user transcript (transcription model, not the conversational one).
    pub(crate) async fn on_user_final(&mut self, text: &str) {
        self.turn.turn_no += 1;
        let _ = self
            .inner
            .drafts
            .repo
            .add_transcript(&NewVoiceTranscript {
                session_id: &self.call.session_id,
                user_id: &self.call.user_id,
                turn_no: self.turn.turn_no,
                role: "user",
                text,
                expires_at: aionui_common::now_ms() + TRANSCRIPT_TTL_MS,
            })
            .await;

        // An `ask` answer is never a command or an approval.
        if let Some(ask_id) = self.inner.asks.answer_next(&self.call.user_id, text) {
            tracing::info!(session_id = %self.call.session_id, ask_id = %ask_id, "voice ask answered");
            if self.turn.active.is_none() {
                self.set_manual(false).await;
            }
            return;
        }

        let holding = self.turn.hold.is_some();
        if holding && let Some(row) = self.turn.active.clone() {
            if is_cancel_phrase(text) {
                self.inner.drafts.cancel(&row).await;
                self.draft_done().await;
            } else {
                if let Some(hold) = &mut self.turn.hold {
                    hold.resume(Instant::now());
                }
                self.inner.drafts.emit(&row, self.hold_progress());
            }
            return;
        }

        let Some(row) = self.turn.active.clone() else {
            self.push_final(text);
            return;
        };
        if row.state == DraftState::Ambiguous.as_str() {
            let drafts = self.inner.drafts.clone();
            match drafts.choose(&row, text).await {
                Some(next) => self.adopt_draft(Some(next)).await,
                None => self.push_final(text),
            }
            return;
        }
        match self.turn.tracker.final_transcript(text) {
            TranscriptVerdict::Confirmed if row.spoken_confirm => self.commit(&row, None).await,
            TranscriptVerdict::Confirmed => {
                self.reject_spoken(&row, "Please tap the card to confirm this one.")
                    .await;
            }
            TranscriptVerdict::Cancel => {
                self.inner.drafts.cancel(&row).await;
                self.draft_done().await;
            }
            TranscriptVerdict::Rejected(reason) => {
                tracing::info!(session_id = %self.call.session_id, draft_id = %row.id, reason, "voice spoken confirmation rejected");
                self.reject_spoken(
                    &row,
                    "I did not catch a clear confirmation. Say it again, or tap the card.",
                )
                .await;
            }
            TranscriptVerdict::Ignored => {
                // Not about the draft: the model may treat it as an edit request.
                self.push_final(text);
                self.respond().await;
            }
        }
    }

    fn push_final(&mut self, text: &str) {
        self.turn.finals.push_back(text.to_owned());
        while self.turn.finals.len() > MAX_FINALS {
            self.turn.finals.pop_front();
        }
    }

    async fn reject_spoken(&mut self, row: &VoiceDraftRow, line: &str) {
        let drafts = self.inner.drafts.clone();
        let current = if row.state == DraftState::PendingTranscript.as_str() {
            drafts.back_to_drafted(row).await.unwrap_or_else(|| row.clone())
        } else {
            row.clone()
        };
        self.turn.active = Some(current.clone());
        drafts
            .speak_line(
                &self.call.user_id,
                Some(&self.call.session_id),
                current.target_id.as_deref().unwrap_or_default(),
                "confirm_rejected",
                line,
                ClipVoice::Jarvis,
            )
            .await;
    }

    async fn commit(&mut self, row: &VoiceDraftRow, hash: Option<&str>) {
        let drafts = self.inner.drafts.clone();
        match drafts.confirm(row, hash).await {
            Some(committing) => {
                self.turn.active = Some(committing);
                self.turn.hold = Some(HoldClock::start(Instant::now()));
                self.turn.tracker.reset();
            }
            None => tracing::info!(draft_id = %row.id, "voice confirm did not apply"),
        }
    }

    pub(crate) async fn on_client(&mut self, event: ClientEvent) {
        let drafts = self.inner.drafts.clone();
        match event {
            ClientEvent::PlaybackFinished { readback_id, t_end } => {
                self.turn.tracker.playback_finished(&readback_id, t_end);
            }
            ClientEvent::SpeechOnset { t } => {
                let now = Instant::now();
                if self.turn.hold.is_some() {
                    self.freeze_hold(now);
                    return;
                }
                match self.turn.tracker.client_onset(t, now) {
                    OnsetVerdict::Candidate => {
                        if let Some(row) = self.turn.active.clone()
                            && row.state == DraftState::Drafted.as_str()
                            && let Some(pending) = drafts.mark_pending(&row).await
                        {
                            self.turn.active = Some(pending);
                        }
                    }
                    OnsetVerdict::TooEarly => {
                        if let Some(row) = self.turn.active.clone() {
                            self.reject_spoken(&row, "Say it again, or tap the card.").await;
                        }
                    }
                    OnsetVerdict::NotAwaiting | OnsetVerdict::Conflict => {}
                }
            }
            ClientEvent::ConfirmTap {
                draft_id,
                version,
                hash,
            } => {
                let row = drafts
                    .repo
                    .get_draft(&self.call.user_id, &draft_id, version)
                    .await
                    .ok()
                    .flatten();
                if let Some(row) = row {
                    self.commit(&row, Some(&hash)).await;
                }
            }
            ClientEvent::CancelTap { draft_id, version } => {
                let row = drafts
                    .repo
                    .get_draft(&self.call.user_id, &draft_id, version)
                    .await
                    .ok()
                    .flatten();
                if let Some(row) = row {
                    drafts.cancel(&row).await;
                }
                if self.turn.active.as_ref().is_some_and(|a| a.id == draft_id) {
                    self.draft_done().await;
                }
            }
            ClientEvent::AskPending => self.set_manual(true).await,
        }
    }

    /// Lifecycle tick: the hold, and active drafts that ended elsewhere.
    pub(crate) async fn tick_drafts(&mut self) {
        let Some(row) = self.turn.active.clone() else {
            return;
        };
        if let Some(hold) = &mut self.turn.hold {
            if hold.tick(Instant::now()) == HoldEvent::Due {
                let drafts = self.inner.drafts.clone();
                drafts.start_send(&row).await;
                self.draft_done().await;
            }
            return;
        }
        // Cancelled by a tap on another device, expired by the worker, …
        let fresh = self
            .inner
            .drafts
            .repo
            .get_draft(&row.user_id, &row.id, row.version)
            .await
            .ok()
            .flatten();
        if fresh
            .as_ref()
            .is_none_or(|f| DraftState::parse(&f.state).is_none_or(|s| s.is_terminal()))
        {
            self.draft_done().await;
        } else if let Some(fresh) = fresh {
            self.turn.active = Some(fresh);
        }
    }

    /// After the config gate: carry the previous call's summary into the
    /// model context and re-announce a draft still waiting for the user.
    pub(crate) async fn on_call_ready(&mut self, carry: Option<String>) {
        if let Some(summary) = carry
            && let Err(err) = self.engine_call.add_context(&summary).await
        {
            tracing::debug!(error = %err, "voice previous-call summary not added");
        }
        let drafts = self.inner.drafts.clone();
        if let Some(row) = drafts.pending_for_user(&self.call.user_id).await {
            self.set_manual(true).await;
            let fresh = drafts.reannounce(&row, &self.call.session_id).await;
            self.adopt_draft(fresh).await;
        }
    }

    /// Store an assistant final transcript (rotation summaries use both sides).
    pub(crate) async fn on_assistant_final(&mut self, text: &str) {
        self.turn.turn_no += 1;
        let _ = self
            .inner
            .drafts
            .repo
            .add_transcript(&NewVoiceTranscript {
                session_id: &self.call.session_id,
                user_id: &self.call.user_id,
                turn_no: self.turn.turn_no,
                role: "assistant",
                text,
                expires_at: aionui_common::now_ms() + TRANSCRIPT_TTL_MS,
            })
            .await;
    }

    /// Whether a draft is pending (rotation waits for it).
    pub(crate) fn draft_pending(&self) -> bool {
        self.turn.active.is_some()
    }

    /// Hold progress for the card (`holdRemainingMs`, `holdFrozen`).
    pub(crate) fn hold_progress(&self) -> Option<(u64, bool)> {
        self.turn
            .hold
            .as_ref()
            .map(|h| (h.remaining(Instant::now()).min(HOLD).as_millis() as u64, h.is_frozen()))
    }
}
