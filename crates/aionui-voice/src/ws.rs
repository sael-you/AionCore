//! `/api/voice/ws` control socket (§3.9 S1a frames, §3.5 frame rules).
//!
//! The upgrade request already passed the voice HTTP guards (credential
//! precedence, Origin allowlist, cookie session via `auth_middleware`). Here:
//! `voice.hello` first, per-frame `voiceSessionId` ownership, size and rate
//! limits, device liveness, and 60 s re-validation of the session cookie.

use std::time::Duration;

use aionui_api_types::{VoiceClientFrame, VoiceReady, VoiceServerFrame};
use aionui_auth::RateLimiter;
use aionui_realtime::{ConnectionId, PER_CONNECTION_BUFFER, WebSocketCloseCode, WsOutbound, send_loop};
use axum::extract::ws::{Message, WebSocket};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::frames::{error_frame, to_text};
use crate::ports::AuthRejected;
use crate::runner_drafts::ClientEvent;
use crate::service::VoiceService;

/// Control frames: ≤ 64 KB each (also the upgrade's `max_message_size`).
pub const MAX_CONTROL_FRAME_BYTES: usize = 64 * 1024;
/// Control frames per second per socket.
pub const MAX_CONTROL_FRAMES_PER_SECOND: u32 = 10;
/// Audio frames (S3 devices): ≤ 32 KB and ≤ 50 per second.
pub const MAX_AUDIO_FRAME_BYTES: usize = 32 * 1024;
pub const MAX_AUDIO_FRAMES_PER_SECOND: u32 = 50;
/// Limit violations tolerated before the socket is closed.
const MAX_VIOLATIONS: u32 = 20;

/// Per-socket frame limits, built on the shared fixed-window `RateLimiter`.
pub struct FrameLimits {
    control: RateLimiter,
    audio: RateLimiter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameVerdict {
    Ok,
    TooLarge,
    RateLimited,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self {
            control: RateLimiter::new(MAX_CONTROL_FRAMES_PER_SECOND, Duration::from_secs(1)),
            audio: RateLimiter::new(MAX_AUDIO_FRAMES_PER_SECOND, Duration::from_secs(1)),
        }
    }
}

impl FrameLimits {
    pub fn check_control(&self, bytes: usize) -> FrameVerdict {
        if bytes > MAX_CONTROL_FRAME_BYTES {
            return FrameVerdict::TooLarge;
        }
        match self.control.check_and_increment("control") {
            Ok(()) => FrameVerdict::Ok,
            Err(_) => FrameVerdict::RateLimited,
        }
    }

    pub fn check_audio(&self, bytes: usize) -> FrameVerdict {
        if bytes > MAX_AUDIO_FRAME_BYTES {
            return FrameVerdict::TooLarge;
        }
        match self.audio.check_and_increment("audio") {
            Ok(()) => FrameVerdict::Ok,
            Err(_) => FrameVerdict::RateLimited,
        }
    }
}

struct Socket {
    service: VoiceService,
    user_id: String,
    token: Option<String>,
    conn_id: ConnectionId,
    tx: mpsc::Sender<WsOutbound>,
    bound_session: Option<String>,
    hello_done: bool,
    violations: u32,
}

enum Flow {
    Continue,
    Close,
}

impl Socket {
    fn send(&self, frame: &VoiceServerFrame) {
        self.service.sockets().send_conn(self.conn_id, frame);
    }

    async fn close_with(&self, frame: &VoiceServerFrame, reason: &str) {
        let _ = self
            .tx
            .send(WsOutbound::TextThenClose(
                to_text(frame),
                WebSocketCloseCode::PolicyViolation,
                reason.to_owned(),
            ))
            .await;
    }

    fn violation(&mut self, code: &str, message: &str) -> Flow {
        self.violations += 1;
        self.send(&error_frame(self.bound_session.as_deref(), code, message));
        if self.violations >= MAX_VIOLATIONS {
            tracing::warn!(user_id = %self.user_id, code, "voice socket closed after repeated violations");
            Flow::Close
        } else {
            Flow::Continue
        }
    }

    async fn on_text(&mut self, text: &str, limits: &FrameLimits) -> Flow {
        match limits.check_control(text.len()) {
            FrameVerdict::Ok => {}
            FrameVerdict::TooLarge => return self.violation("FRAME_TOO_LARGE", "Control frames are limited to 64 KB."),
            FrameVerdict::RateLimited => return self.violation("RATE_LIMITED", "Too many control frames."),
        }
        let frame: VoiceClientFrame = match serde_json::from_str(text) {
            Ok(frame) => frame,
            Err(_) => return self.violation("INVALID_FRAME", "Unknown or malformed voice frame."),
        };

        if !self.hello_done && !matches!(frame, VoiceClientFrame::Hello(_)) {
            self.close_with(
                &error_frame(None, "HELLO_REQUIRED", "The first frame must be voice.hello."),
                "hello required",
            )
            .await;
            return Flow::Close;
        }

        // Per-frame ownership check (§3.5 frames row).
        if let Some(claimed) = frame.voice_session_id()
            && self.bound_session.as_deref() != Some(claimed)
        {
            if !self.service.session_belongs(&self.user_id, claimed).await {
                tracing::warn!(user_id = %self.user_id, "voice frame refused: session not owned by socket user");
                return self.violation("SESSION_MISMATCH", "This voice session does not belong to you.");
            }
            self.bound_session = Some(claimed.to_owned());
        }
        let session = frame.voice_session_id().map(str::to_owned);

        match frame {
            VoiceClientFrame::Hello(hello) => {
                if !self.service.is_enabled(&self.user_id).await {
                    self.close_with(
                        &error_frame(None, "VOICE_DISABLED", "Voice is turned off."),
                        "voice disabled",
                    )
                    .await;
                    return Flow::Close;
                }
                self.hello_done = true;
                tracing::debug!(user_id = %self.user_id, device = %log_safe(&hello.device), "voice socket hello");
                self.service.note_heartbeat(&self.user_id, session.as_deref());
                self.service
                    .note_hello_mode(&self.user_id, self.conn_id, hello.mode.as_deref());
                if let (Some(id), Some(token)) = (session.as_deref(), self.token.as_deref()) {
                    self.service.adopt_token(&self.user_id, id, token);
                }
                match session
                    .as_deref()
                    .and_then(|id| self.service.ready_call(&self.user_id, id).map(|r| (id.to_owned(), r)))
                {
                    Some((id, (engine, model))) => self.send(&VoiceServerFrame::Ready(VoiceReady {
                        voice_session_id: id,
                        engine,
                        model,
                        capabilities: Some(crate::frames::SERVER_CAPABILITIES),
                    })),
                    None => self.send(&crate::frames::hello_armed_frame(session.as_deref(), None)),
                }
                // A page that connects after the call is ready still gets the card.
                for frame in self.service.pending_draft_frames(&self.user_id).await {
                    self.send(&frame);
                }
                self.service.speak_interrupted(&self.user_id);
            }
            VoiceClientFrame::Heartbeat(_) => self.service.note_heartbeat(&self.user_id, session.as_deref()),
            VoiceClientFrame::Frozen(_) => {
                tracing::info!(user_id = %self.user_id, "voice device frozen");
                self.service.note_frozen(&self.user_id, session.as_deref(), true);
            }
            VoiceClientFrame::Resumed(_) => {
                tracing::info!(user_id = %self.user_id, "voice device resumed");
                self.service.note_frozen(&self.user_id, session.as_deref(), false);
                match session
                    .as_deref()
                    .and_then(|id| self.service.ready_call(&self.user_id, id).map(|r| (id.to_owned(), r)))
                {
                    Some((id, (engine, model))) => self.send(&VoiceServerFrame::Ready(VoiceReady {
                        voice_session_id: id,
                        engine,
                        model,
                        capabilities: Some(crate::frames::SERVER_CAPABILITIES),
                    })),
                    None => self.send(&crate::frames::hello_armed_frame(session.as_deref(), Some("resumed"))),
                }
            }
            VoiceClientFrame::PlaybackFinished(frame) => {
                self.service
                    .client_event(
                        &self.user_id,
                        session.as_deref(),
                        ClientEvent::PlaybackFinished {
                            readback_id: frame.readback_id,
                            t_end: frame.t_end,
                        },
                    )
                    .await;
            }
            VoiceClientFrame::SpeechOnset(frame) => {
                self.service
                    .client_event(
                        &self.user_id,
                        session.as_deref(),
                        ClientEvent::SpeechOnset { t: frame.t },
                    )
                    .await;
            }
            VoiceClientFrame::ConfirmTap(tap) => {
                self.service
                    .client_event(
                        &self.user_id,
                        session.as_deref(),
                        ClientEvent::ConfirmTap {
                            draft_id: tap.draft_id,
                            version: tap.version,
                            hash: tap.hash,
                        },
                    )
                    .await;
            }
            VoiceClientFrame::CancelTap(tap) => {
                self.service
                    .client_event(
                        &self.user_id,
                        session.as_deref(),
                        ClientEvent::CancelTap {
                            draft_id: tap.draft_id,
                            version: tap.version,
                        },
                    )
                    .await;
            }
            VoiceClientFrame::Commit(_) => {
                self.service
                    .client_event(&self.user_id, session.as_deref(), ClientEvent::Commit)
                    .await;
            }
            VoiceClientFrame::AttachQuotes(quotes) => {
                if let Err(reason) = self
                    .service
                    .attach_quotes(&self.user_id, session.as_deref(), quotes.text)
                    .await
                {
                    tracing::info!(user_id = %self.user_id, reason, "voice quotes refused");
                    self.send(&crate::frames::quotes_attached_frame(
                        session.as_deref().unwrap_or_default(),
                        false,
                        reason,
                    ));
                }
            }
        }
        Flow::Continue
    }

    async fn revalidate(&mut self) -> Flow {
        let verdict = match self.token.as_deref() {
            // No cookie to re-check: fail closed.
            None => Err(AuthRejected::Rejected),
            Some(token) => self.service.verify_token(token).await,
        };
        match verdict {
            Ok(user) if user.id == self.user_id => Flow::Continue,
            Ok(_) | Err(AuthRejected::Rejected) => {
                tracing::warn!(user_id = %self.user_id, "voice socket session re-validation failed; closing");
                self.close_with(
                    &error_frame(None, "AUTH_EXPIRED", "Your session ended."),
                    "session ended",
                )
                .await;
                // Only the calls on this cookie or bound to this socket.
                self.service
                    .end_calls_for_socket(&self.user_id, self.token.as_deref(), self.bound_session.as_deref())
                    .await;
                Flow::Close
            }
            Err(AuthRejected::Unavailable) => {
                tracing::warn!(user_id = %self.user_id, "voice socket re-validation unavailable");
                Flow::Continue
            }
        }
    }
}

/// Client-supplied text for logs: at most 64 printable characters.
fn log_safe(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Drive one upgraded voice socket until it closes.
pub async fn handle_voice_socket(socket: WebSocket, user_id: String, token: Option<String>, service: VoiceService) {
    service.mark_listening();
    let (sink, mut stream) = socket.split();
    let (tx, rx) = mpsc::channel(PER_CONNECTION_BUFFER);
    let conn_id =
        service
            .sockets()
            .manager()
            .add_client_for_user(user_id.clone(), token.clone().unwrap_or_default(), tx.clone());
    let writer = tokio::spawn(send_loop(conn_id, rx, sink));
    tracing::info!(user_id = %user_id, %conn_id, "voice socket opened");

    let timings = service.timings();
    let limits = FrameLimits::default();
    let mut state = Socket {
        service: service.clone(),
        user_id: user_id.clone(),
        token,
        conn_id,
        tx,
        bound_session: None,
        hello_done: false,
        violations: 0,
    };
    let hello_deadline = Instant::now() + timings.hello_timeout;
    let mut revalidate = tokio::time::interval_at(Instant::now() + timings.revalidate_every, timings.revalidate_every);

    loop {
        let flow = tokio::select! {
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => state.on_text(text.as_str(), &limits).await,
                Some(Ok(Message::Binary(bytes))) => {
                    match limits.check_audio(bytes.len()) {
                        FrameVerdict::Ok => state.violation("UNSUPPORTED", "Audio frames are not accepted on this socket yet."),
                        FrameVerdict::TooLarge => state.violation("FRAME_TOO_LARGE", "Audio frames are limited to 32 KB."),
                        FrameVerdict::RateLimited => state.violation("RATE_LIMITED", "Too many audio frames."),
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => Flow::Close,
                Some(Ok(_)) => Flow::Continue,
            },
            _ = tokio::time::sleep_until(hello_deadline), if !state.hello_done => {
                state.close_with(&error_frame(None, "HELLO_REQUIRED", "voice.hello not received in time."), "hello timeout").await;
                Flow::Close
            },
            _ = revalidate.tick() => state.revalidate().await,
        };
        if matches!(flow, Flow::Close) {
            break;
        }
    }

    service.sockets().manager().remove_client(conn_id);
    service.sockets().forget(conn_id);
    // Let the writer flush a pending close frame, then stop it.
    drop(state);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    tracing::info!(user_id = %user_id, %conn_id, "voice socket closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_limits_size_and_rate() {
        let limits = FrameLimits::default();
        assert_eq!(
            limits.check_control(MAX_CONTROL_FRAME_BYTES + 1),
            FrameVerdict::TooLarge
        );
        for _ in 0..MAX_CONTROL_FRAMES_PER_SECOND {
            assert_eq!(limits.check_control(10), FrameVerdict::Ok);
        }
        assert_eq!(limits.check_control(10), FrameVerdict::RateLimited);
    }

    #[test]
    fn logged_device_names_are_capped_and_printable() {
        assert_eq!(log_safe("phone\u{0}\n"), "phone");
        assert_eq!(log_safe(&"x".repeat(500)).len(), 64);
    }

    #[test]
    fn audio_limits_size_and_rate() {
        let limits = FrameLimits::default();
        assert_eq!(limits.check_audio(MAX_AUDIO_FRAME_BYTES + 1), FrameVerdict::TooLarge);
        for _ in 0..MAX_AUDIO_FRAMES_PER_SECOND {
            assert_eq!(limits.check_audio(1024), FrameVerdict::Ok);
        }
        assert_eq!(limits.check_audio(1024), FrameVerdict::RateLimited);
    }
}
