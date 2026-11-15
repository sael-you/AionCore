//! `say` / `ask` / `wait` for agents (§3.4) on [`VoiceService`].

use std::time::Duration;

use aionui_api_types::{VoiceAgentReply, VoiceServerFrame, VoiceWake};

use crate::agent::AskStatus;
use crate::conversation_mode::{BOUND_SAY_MAX_CHARS, CLIP_MAX_CHARS, split_for_speech};
use crate::drafts::evidence::contains_confirm_grammar;
use crate::error::VoiceError;
use crate::readback::ClipVoice;
use crate::runner::CallCommand;
use crate::runner_drafts::ClientEvent;
use crate::service::VoiceService;

const MAX_AGENT_TEXT: usize = 500;

fn reply(status: &str, id: Option<String>, answer: Option<String>) -> VoiceAgentReply {
    VoiceAgentReply {
        status: status.to_owned(),
        id,
        answer,
    }
}

/// Who is speaking: the bound conversation of conversation voice mode
/// (C26: no prefix, longer text, no confirm-word ban) or a C9 conversation.
struct AgentAccess {
    name: String,
    bound: bool,
}

impl VoiceService {
    /// Whether `conversation_id` is the user's conversation voice-mode binding.
    pub fn is_bound(&self, user_id: &str, conversation_id: &str) -> bool {
        self.inner.bound_conversation(user_id).as_deref() == Some(conversation_id)
    }

    /// Agents may speak only for the bound conversation, conversations the
    /// user sent to by voice, or that the user opted in.
    async fn agent_allowed(&self, user_id: &str, conversation_id: &str) -> Result<AgentAccess, VoiceError> {
        let settings = self.settings(user_id).await?;
        if !settings.enabled {
            return Err(VoiceError::Disabled);
        }
        if self.is_bound(user_id, conversation_id) {
            return Ok(AgentAccess {
                name: String::new(),
                bound: true,
            });
        }
        let watched = self
            .inner
            .drafts
            .watched
            .get(conversation_id)
            .is_some_and(|u| u.as_str() == user_id);
        if !watched && !settings.agent_conversations().iter().any(|c| c == conversation_id) {
            return Err(VoiceError::Forbidden(
                "this conversation may not speak by voice (no voice request was sent to it)".into(),
            ));
        }
        let name = self
            .inner
            .drafts
            .conversations
            .list_conversations(user_id)
            .await
            .ok()
            .and_then(|list| list.into_iter().find(|c| c.id == conversation_id).map(|c| c.name))
            .unwrap_or_else(|| "A conversation".to_owned());
        Ok(AgentAccess { name, bound: false })
    }

    /// Text of a bound `say`/`ask`: 1–1 200 characters, no control characters.
    fn bound_text(text: &str) -> Result<String, VoiceError> {
        let text: String = text
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .to_owned();
        if text.is_empty() || text.chars().count() > BOUND_SAY_MAX_CHARS {
            return Err(VoiceError::BadRequest(format!(
                "text must be 1-{BOUND_SAY_MAX_CHARS} characters"
            )));
        }
        Ok(text)
    }

    /// The bound conversation speaks: ≤ 300-character clips played in order,
    /// synthesized in the background so `say` returns at once. With no live
    /// call each clip is announced by `voice.wake` (no shared wake limit).
    fn bound_speak(&self, user_id: &str, conversation_id: &str, status: &'static str, text: &str) {
        self.inner.drafts.spoke.insert(conversation_id.to_owned(), ());
        let lines = split_for_speech(text, CLIP_MAX_CHARS);
        let service = self.clone();
        let user_id = user_id.to_owned();
        let conversation_id = conversation_id.to_owned();
        tokio::spawn(async move {
            for line in lines {
                let session = service
                    .inner
                    .live
                    .iter()
                    .find(|c| c.user_id == user_id && c.focus.as_deref() == Some(conversation_id.as_str()))
                    .map(|c| c.session_id.clone());
                let clip = service
                    .inner
                    .drafts
                    .speak_unchecked(
                        &user_id,
                        session.as_deref(),
                        &conversation_id,
                        status,
                        &line,
                        ClipVoice::Agent,
                    )
                    .await;
                if session.is_none()
                    && let Some(clip_id) = clip
                {
                    service.inner.sockets.send_user(
                        &user_id,
                        &VoiceServerFrame::Wake(VoiceWake {
                            voice_session_id: None,
                            reason: status.to_owned(),
                            item_id: clip_id,
                        }),
                    );
                }
            }
        });
    }

    fn agent_text(text: &str) -> Result<String, VoiceError> {
        let text: String = text
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .to_owned();
        if text.is_empty() || text.chars().count() > MAX_AGENT_TEXT {
            return Err(VoiceError::BadRequest(format!(
                "text must be 1-{MAX_AGENT_TEXT} characters"
            )));
        }
        if contains_confirm_grammar(&text) {
            return Err(VoiceError::BadRequest(
                "text must not contain confirmation words (send, yes, confirm, go ahead)".into(),
            ));
        }
        Ok(text)
    }

    /// Deliver a clip to the user's device; `false` when no device is connected.
    async fn agent_speak(&self, user_id: &str, conversation_id: &str, status: &str, line: &str) -> bool {
        if self.inner.sockets.count_for_user(user_id) == 0 {
            return false;
        }
        let clip = self
            .inner
            .drafts
            .speak_line(user_id, None, conversation_id, status, line, ClipVoice::Agent)
            .await;
        if !self.has_live_call(user_id)
            && let Some(clip_id) = clip
            && self.inner.wake_limiter.check_and_increment(user_id).is_ok()
        {
            self.inner.sockets.send_user(
                user_id,
                &VoiceServerFrame::Wake(VoiceWake {
                    voice_session_id: None,
                    reason: status.to_owned(),
                    item_id: clip_id,
                }),
            );
        }
        true
    }

    pub async fn agent_say(
        &self,
        user_id: &str,
        conversation_id: &str,
        text: &str,
    ) -> Result<VoiceAgentReply, VoiceError> {
        let access = self.agent_allowed(user_id, conversation_id).await?;
        let text = if access.bound {
            Self::bound_text(text)?
        } else {
            Self::agent_text(text)?
        };
        if self.inner.say_limiter.check_and_increment(conversation_id).is_err() {
            return Err(VoiceError::RateLimited);
        }
        tracing::info!(user_id, conversation_id, bound = access.bound, "voice agent say");
        if access.bound {
            if self.inner.sockets.count_for_user(user_id) == 0 {
                return Ok(reply("no_device", None, None));
            }
            self.bound_speak(user_id, conversation_id, "says", &text);
            return Ok(reply("delivered", None, None));
        }
        let name = access.name;
        let delivered = self
            .agent_speak(user_id, conversation_id, "says", &format!("{name} says: {text}"))
            .await;
        Ok(reply(if delivered { "delivered" } else { "no_device" }, None, None))
    }

    pub async fn agent_ask(
        &self,
        user_id: &str,
        conversation_id: &str,
        question: &str,
        timeout: Duration,
    ) -> Result<VoiceAgentReply, VoiceError> {
        let access = self.agent_allowed(user_id, conversation_id).await?;
        let question = if access.bound {
            Self::bound_text(question)?
        } else {
            Self::agent_text(question)?
        };
        if self.inner.sockets.count_for_user(user_id) == 0 {
            return Ok(reply("no_device", None, None));
        }
        // Same budget as `say` (each ask is spoken), one open ask per conversation.
        if self.inner.say_limiter.check_and_increment(conversation_id).is_err() {
            return Err(VoiceError::RateLimited);
        }
        let Some(id) = self.inner.asks.create(user_id, conversation_id) else {
            return Err(VoiceError::RateLimited);
        };
        tracing::info!(user_id, conversation_id, ask_id = %id, "voice agent ask");
        // While the ask is pending the model may not act on the answer.
        let calls: Vec<_> = self
            .inner
            .live
            .iter()
            .filter(|c| c.user_id == user_id)
            .map(|c| c.cmd_tx.clone())
            .collect();
        for tx in calls {
            let _ = tx.send(CallCommand::Client(ClientEvent::AskPending)).await;
        }
        if access.bound {
            self.bound_speak(user_id, conversation_id, "asks", &question);
        } else {
            let name = access.name;
            self.agent_speak(user_id, conversation_id, "asks", &format!("{name} asks: {question}"))
                .await;
        }
        self.ask_reply(user_id, conversation_id, &id, timeout).await
    }

    pub async fn agent_wait(
        &self,
        user_id: &str,
        conversation_id: &str,
        id: &str,
    ) -> Result<VoiceAgentReply, VoiceError> {
        self.ask_reply(user_id, conversation_id, id, crate::agent::MAX_WAIT)
            .await
    }

    async fn ask_reply(
        &self,
        user_id: &str,
        conversation_id: &str,
        id: &str,
        timeout: Duration,
    ) -> Result<VoiceAgentReply, VoiceError> {
        let status = self
            .inner
            .asks
            .wait(user_id, conversation_id, id, timeout)
            .await
            .ok_or_else(|| VoiceError::NotFound(format!("ask '{id}' not found")))?;
        Ok(match status {
            AskStatus::Answered(answer) => reply("answered", Some(id.to_owned()), Some(answer)),
            AskStatus::Pending => reply("pending", Some(id.to_owned()), None),
            AskStatus::Cancelled => reply("cancelled", Some(id.to_owned()), None),
            AskStatus::Expired => reply("timeout", Some(id.to_owned()), None),
        })
    }
}
