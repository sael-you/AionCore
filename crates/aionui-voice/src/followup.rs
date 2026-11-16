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
    let follow = |kind| Signal::FollowUp {
        user_id: user_id.clone(),
        conversation_id: conversation_id.clone(),
        turn_id: turn_id.clone(),
        kind,
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
}

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
                let queued = self
                    .drafts
                    .repo
                    .user_drafts_in_states(&user_id, &["queued"])
                    .await
                    .unwrap_or_default();
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
            } => {
                if self.drafts.watched.get(&conversation_id).map(|u| u.clone()).as_deref() != Some(user_id.as_str()) {
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
                tracing::info!(conversation_id = %conversation_id, status = kind.status(), "voice follow-up");
                let clip = self
                    .drafts
                    .speak_line(
                        &user_id,
                        None,
                        &conversation_id,
                        kind.status(),
                        &kind.line(&name),
                        ClipVoice::Agent,
                    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
