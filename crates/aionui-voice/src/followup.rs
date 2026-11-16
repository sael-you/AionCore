//! Follow-up watcher (§3.4): for conversations the user sent to by voice,
//! turn the event bus into short spoken lines and retry queued drafts.
//!
//! Event names and `message.stream` `type` values are taken from the code
//! that emits them: `turn.completed` (`runtime_completion.rs`,
//! `stream_persistence.rs`), `confirmation.remove` (conversation service),
//! and `message.stream` carrying `AgentStreamEvent` serialised with
//! `tag = "type", rename_all = "snake_case"` (`aionui-ai-agent`
//! `protocol/events`): `error`, `permission`, `acp_permission`, `ask`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{VoiceServerFrame, VoiceWake, WebSocketMessage};
use aionui_auth::RateLimiter;
use aionui_realtime::BroadcastEventBus;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::drafts::DraftEngine;
use crate::frames::VoiceSockets;
use crate::readback::ClipVoice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUpKind {
    Finished,
    Failed,
    NeedsApproval,
}

impl FollowUpKind {
    pub fn status(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::NeedsApproval => "needs_approval",
        }
    }

    /// The bound conversation's finished line (owner: "just say that the task
    /// is finished"): no conversation name, he is talking to that one.
    pub const BOUND_FINISHED: &'static str = "The task is finished.";

    pub fn line(self, name: &str) -> String {
        match self {
            Self::Finished => format!("{name} finished."),
            Self::Failed => format!("{name} failed."),
            Self::NeedsApproval => format!("{name} needs an approval. It is open on your screen."),
        }
    }
}

/// What one bus event means for voice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    FollowUp {
        user_id: String,
        conversation_id: String,
        turn_id: Option<String>,
        kind: FollowUpKind,
        /// `turn.completed`'s `last_outcome` (`finished` | `error` | `cancelled`).
        outcome: Option<String>,
    },
    /// The target may accept queued drafts now (turn ended / approval gone).
    Retry { user_id: String, conversation_id: String },
}

fn field(data: &Value, key: &str) -> Option<String> {
    data.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Classify one bus event (pure; one test per frame type).
pub fn classify(event: &WebSocketMessage<Value>) -> Vec<Signal> {
    let data = &event.data;
    let (Some(user_id), Some(conversation_id)) = (field(data, "user_id"), field(data, "conversation_id")) else {
        return Vec::new();
    };
    let turn_id = field(data, "turn_id");
    let outcome = field(data, "last_outcome");
    let follow = |kind| Signal::FollowUp {
        user_id: user_id.clone(),
        conversation_id: conversation_id.clone(),
        turn_id: turn_id.clone(),
        kind,
        outcome: outcome.clone(),
    };
    let retry = || Signal::Retry {
        user_id: user_id.clone(),
        conversation_id: conversation_id.clone(),
    };
    match event.name.as_str() {
        "turn.completed" => vec![follow(FollowUpKind::Finished), retry()],
        "confirmation.remove" => vec![retry()],
        "message.stream" => match data.get("type").and_then(Value::as_str) {
            Some("error") => vec![follow(FollowUpKind::Failed)],
            Some("permission" | "acp_permission" | "ask") => vec![follow(FollowUpKind::NeedsApproval)],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Is the user in a live call? (Without one, a clip is announced by `voice.wake`.)
pub trait CallPresence: Send + Sync {
    fn has_live_call(&self, user_id: &str) -> bool;

    /// The conversation the user's conversation voice mode is bound to (C26).
    fn bound_conversation(&self, user_id: &str) -> Option<String>;

    /// The bound conversation's turn ended (restart recovery reads it).
    fn note_bound_turn_end(&self, user_id: &str, conversation_id: &str);
}

/// `voice.followUp.status` of a failed TURN of the bound conversation
/// (`failed` there means the send itself failed).
pub const STATUS_TURN_FAILED: &str = "turn_failed";

pub struct FollowUpWatcher {
    pub bus: Arc<BroadcastEventBus>,
    pub drafts: Arc<DraftEngine>,
    pub sockets: VoiceSockets,
    pub presence: Arc<dyn CallPresence>,
    pub wake_limiter: Arc<RateLimiter>,
}

/// Server wakes per user per hour (§3.4 default), follow-ups and agent
/// `say`/`ask` together.
pub const MAX_WAKES_PER_HOUR: u32 = 6;

/// The per-user wake limiter shared by follow-ups and agent lines.
pub fn wake_limiter() -> Arc<RateLimiter> {
    Arc::new(RateLimiter::new(MAX_WAKES_PER_HOUR, Duration::from_secs(3600)))
}

impl FollowUpWatcher {
    pub fn new(
        bus: Arc<BroadcastEventBus>,
        drafts: Arc<DraftEngine>,
        sockets: VoiceSockets,
        presence: Arc<dyn CallPresence>,
        wake_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            bus,
            drafts,
            sockets,
            presence,
            wake_limiter,
        }
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        let mut rx = self.bus.subscribe();
        tokio::spawn(async move {
            let mut recent: VecDeque<(String, Option<String>, FollowUpKind)> = VecDeque::new();
            loop {
                let event = match rx.recv().await {
                    Ok(event) => event,
                    // Lagged: the worker's tick re-reads state from the DB.
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "voice follow-up watcher lagged");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                };
                for signal in classify(&event) {
                    self.handle(signal, &mut recent).await;
                }
            }
        })
    }

    async fn handle(&self, signal: Signal, recent: &mut VecDeque<(String, Option<String>, FollowUpKind)>) {
        match signal {
            Signal::Retry {
                user_id,
                conversation_id,
            } => {
                let mut queued = self
                    .drafts
                    .repo
                    .user_drafts_in_states(&user_id, &["queued"])
                    .await
                    .unwrap_or_default();
                // Oldest first: queued messages keep the order they were said in.
                queued.sort_by_key(|r| (r.created_at, r.id.clone()));
                for row in queued
                    .into_iter()
                    .filter(|r| r.target_id.as_deref() == Some(&conversation_id))
                {
                    self.drafts.retry_queued(&row).await;
                }
            }
            Signal::FollowUp {
                user_id,
                conversation_id,
                turn_id,
                kind,
                outcome,
            } => {
                let bound = self.presence.bound_conversation(&user_id).as_deref() == Some(conversation_id.as_str());
                let watched =
                    self.drafts.watched.get(&conversation_id).map(|u| u.clone()).as_deref() == Some(user_id.as_str());
                if !bound && !watched {
                    return;
                }
                let key = (conversation_id.clone(), turn_id, kind);
                if recent.contains(&key) {
                    return;
                }
                recent.push_back(key);
                if recent.len() > 64 {
                    recent.pop_front();
                }
                if kind == FollowUpKind::Finished {
                    // One follow-up per sent message.
                    self.drafts.watched.remove(&conversation_id);
                }
                let name = self
                    .drafts
                    .conversations
                    .list_conversations(&user_id)
                    .await
                    .ok()
                    .and_then(|list| list.into_iter().find(|c| c.id == conversation_id).map(|c| c.name))
                    .unwrap_or_else(|| "A conversation".to_owned());
                tracing::info!(conversation_id = %conversation_id, status = kind.status(), bound, "voice follow-up");
                if bound {
                    self.bound_follow_up(&user_id, &conversation_id, &name, kind, outcome.as_deref())
                        .await;
                    return;
                }
                let line = self.line_for(&user_id, &conversation_id, &name, kind, false).await;
                let clip = self
                    .drafts
                    .speak_line(&user_id, None, &conversation_id, kind.status(), &line, ClipVoice::Agent)
                    .await;
                if !self.presence.has_live_call(&user_id)
                    && let Some(clip_id) = clip
                    && self.wake_limiter.check_and_increment(&user_id).is_ok()
                {
                    self.sockets.send_user(
                        &user_id,
                        &VoiceServerFrame::Wake(VoiceWake {
                            voice_session_id: None,
                            reason: "follow_up".to_owned(),
                            item_id: clip_id,
                        }),
                    );
                }
            }
        }
    }
}

impl FollowUpWatcher {
    /// The spoken line: a finished turn adds a short summary of the
    /// conversation's last reply when one can be made (§3.4), kept within
    /// one clip; otherwise the template.
    /// `bound`: the conversation the voice call is bound to (no name spoken).
    async fn line_for(
        &self,
        user_id: &str,
        conversation_id: &str,
        name: &str,
        kind: FollowUpKind,
        bound: bool,
    ) -> String {
        let template = if bound && kind == FollowUpKind::Finished {
            FollowUpKind::BOUND_FINISHED.to_owned()
        } else {
            kind.line(name)
        };
        if kind != FollowUpKind::Finished {
            return template;
        }
        let kit = self.drafts.speech.for_user(user_id).await;
        let reply = self
            .drafts
            .conversations
            .last_reply(user_id, conversation_id)
            .await
            .ok()
            .flatten();
        match crate::summary::summary_of(kit.summarizer.as_deref(), reply.as_deref()).await {
            Some(summary) => {
                let line = if bound {
                    format!("{} {summary}", FollowUpKind::BOUND_FINISHED)
                } else {
                    format!("{name} finished: {summary}")
                };
                if line.chars().count() <= crate::conversation_mode::CLIP_MAX_CHARS {
                    line
                } else {
                    template
                }
            }
            None => template,
        }
    }

    /// Whether the conversation's latest user-side message is the owner's own
    /// (typed or spoken), not a session message, a child result or a hidden
    /// system prompt (cron). Unknown (lookup failed, nothing found) counts as
    /// the owner's: better one line too many than his own turn left silent.
    async fn owner_started(&self, user_id: &str, conversation_id: &str) -> bool {
        match self
            .drafts
            .conversations
            .last_user_message(user_id, conversation_id)
            .await
        {
            Ok(Some(message)) => is_owner_message(&message),
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(conversation_id, error = %err, "voice turn origin unknown; announcing");
                true
            }
        }
    }

    /// The bound conversation (conversation voice mode): its own agent speaks
    /// with `say`; the watcher only fills gaps. A finished turn whose agent
    /// already spoke gets a text line (the "your turn" cue), otherwise the
    /// spoken "<name> finished."; a cancelled turn was already answered
    /// with "Stopped.". Bound lines never count against the wake limit.
    async fn bound_follow_up(
        &self,
        user_id: &str,
        conversation_id: &str,
        name: &str,
        kind: FollowUpKind,
        outcome: Option<&str>,
    ) {
        self.presence.note_bound_turn_end(user_id, conversation_id);
        let spoke = self.drafts.spoke.remove(conversation_id).is_some();
        // A finished turn the owner did not start (a session message, a child
        // result, cron) is no news for him: nothing is said, no cue (TASK-112).
        if kind == FollowUpKind::Finished
            && !spoke
            && outcome != Some("error")
            && !self.owner_started(user_id, conversation_id).await
        {
            tracing::info!(
                conversation_id,
                "voice bound turn end not announced: not started by the owner"
            );
            return;
        }
        let (kind, quiet) = match (kind, outcome) {
            (FollowUpKind::Finished, Some("cancelled")) => (FollowUpKind::Finished, true),
            (FollowUpKind::Finished, Some("error")) => (FollowUpKind::Failed, false),
            (FollowUpKind::Finished, _) => (FollowUpKind::Finished, spoke),
            (other, _) => (other, false),
        };
        let status = if kind == FollowUpKind::Failed {
            STATUS_TURN_FAILED
        } else {
            kind.status()
        };
        if quiet {
            let line = if kind == FollowUpKind::Finished {
                FollowUpKind::BOUND_FINISHED.to_owned()
            } else {
                kind.line(name)
            };
            self.drafts.voice_line(user_id, None, conversation_id, status, &line);
            return;
        }
        let line = self.line_for(user_id, conversation_id, name, kind, true).await;
        let clip = self
            .drafts
            .speak_line(user_id, None, conversation_id, status, &line, ClipVoice::Agent)
            .await;
        if !self.presence.has_live_call(user_id)
            && let Some(clip_id) = clip
        {
            self.sockets.send_user(
                user_id,
                &VoiceServerFrame::Wake(VoiceWake {
                    voice_session_id: None,
                    reason: "follow_up".to_owned(),
                    item_id: clip_id,
                }),
            );
        }
    }
}

/// The owner's own message: not hidden and not delivered by another
/// conversation (`[[AION_SESSION_MESSAGE]]`) or a child (`[[AION_CHILD_RESULT …]]`).
pub(crate) fn is_owner_message(message: &crate::conversation_port::UserMessage) -> bool {
    !message.hidden
        && !message
            .text
            .contains(aionui_common::constants::AIONUI_SESSION_MESSAGE_MARKER)
        && !message.text.trim_start().starts_with("[[AION_CHILD_RESULT")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_the_owners_own_messages_start_announced_turns() {
        use crate::conversation_port::UserMessage;
        let msg = |text: &str, hidden: bool| UserMessage {
            text: text.into(),
            hidden,
        };
        assert!(is_owner_message(&msg("fix the login test", false)));
        assert!(is_owner_message(&msg(
            "add a test\n\n[[AION_VOICE]]\n…\n[[/AION_VOICE]]",
            false
        )));
        assert!(!is_owner_message(&msg(
            "[[AION_SESSION_MESSAGE]]\nfrom: builder\n[[/AION_SESSION_MESSAGE]]\nTASK-91 done",
            false
        )));
        assert!(!is_owner_message(&msg(
            "[[AION_CHILD_RESULT child_id=c2 name=Builder status=finished]]\ndone\n[[/AION_CHILD_RESULT]]",
            false
        )));
        assert!(
            !is_owner_message(&msg("run the nightly check", true)),
            "cron prompts are hidden"
        );
    }

    fn event(name: &str, data: Value) -> WebSocketMessage<Value> {
        WebSocketMessage::new(name, data)
    }

    fn base(extra: Value) -> Value {
        let mut data = json!({"user_id": "u1", "conversation_id": "c1", "turn_id": "t1"});
        for (k, v) in extra.as_object().unwrap() {
            data[k] = v.clone();
        }
        data
    }

    #[test]
    fn turn_completed_is_finished_and_retries() {
        let signals = classify(&event("turn.completed", base(json!({"status": "finished"}))));
        assert!(matches!(
            signals[0],
            Signal::FollowUp {
                kind: FollowUpKind::Finished,
                ..
            }
        ));
        assert!(matches!(signals[1], Signal::Retry { .. }));
    }

    #[test]
    fn stream_error_is_failed() {
        let signals = classify(&event("message.stream", base(json!({"type": "error", "data": {}}))));
        assert!(matches!(
            signals.as_slice(),
            [Signal::FollowUp {
                kind: FollowUpKind::Failed,
                ..
            }]
        ));
    }

    #[test]
    fn permission_frames_need_approval() {
        for kind in ["permission", "acp_permission", "ask"] {
            let signals = classify(&event("message.stream", base(json!({"type": kind}))));
            assert!(
                matches!(
                    signals.as_slice(),
                    [Signal::FollowUp {
                        kind: FollowUpKind::NeedsApproval,
                        ..
                    }]
                ),
                "{kind}"
            );
        }
    }

    #[test]
    fn confirmation_remove_retries_only() {
        let signals = classify(&event("confirmation.remove", base(json!({}))));
        assert!(matches!(signals.as_slice(), [Signal::Retry { .. }]));
    }

    #[test]
    fn content_and_foreign_frames_are_ignored() {
        assert!(classify(&event("message.stream", base(json!({"type": "content"})))).is_empty());
        assert!(classify(&event("conversation.listChanged", base(json!({})))).is_empty());
        assert!(
            classify(&event("turn.completed", json!({"conversation_id": "c1"}))).is_empty(),
            "no user"
        );
    }

    #[test]
    fn lines_have_no_confirm_grammar() {
        for kind in [
            FollowUpKind::Finished,
            FollowUpKind::Failed,
            FollowUpKind::NeedsApproval,
        ] {
            assert!(!crate::drafts::evidence::contains_confirm_grammar(
                &kind.line("Kanban build")
            ));
        }
    }
}
