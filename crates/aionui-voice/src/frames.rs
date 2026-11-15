//! Outbound `/api/voice/ws` frames, delivered through a dedicated
//! `WebSocketManager` (reused from `aionui-realtime`) that holds only voice sockets.

use std::sync::Arc;

use aionui_api_types::{VoiceCallEnded, VoiceErrorFrame, VoiceServerFrame, VoiceStateFrame, WebSocketMessage};
use aionui_realtime::{ConnectionId, WebSocketManager};
use serde_json::Value;

/// Serialize a frame into the standard `{name, data}` envelope.
pub(crate) fn to_message(frame: &VoiceServerFrame) -> Option<WebSocketMessage<Value>> {
    let mut value = serde_json::to_value(frame).ok()?;
    let data = value.get_mut("data").map(Value::take).unwrap_or(Value::Null);
    Some(WebSocketMessage::new(frame.name(), data))
}

pub(crate) fn to_text(frame: &VoiceServerFrame) -> String {
    to_message(frame)
        .and_then(|msg| serde_json::to_string(&msg).ok())
        .unwrap_or_default()
}

pub(crate) fn error_frame(session_id: Option<&str>, code: &str, message: &str) -> VoiceServerFrame {
    VoiceServerFrame::Error(VoiceErrorFrame {
        voice_session_id: session_id.map(str::to_owned),
        code: code.to_owned(),
        message: message.to_owned(),
    })
}

pub(crate) fn call_ended_frame(session_id: &str, reason: &str) -> VoiceServerFrame {
    VoiceServerFrame::CallEnded(VoiceCallEnded {
        voice_session_id: session_id.to_owned(),
        reason: reason.to_owned(),
    })
}

pub(crate) fn state_frame(session_id: Option<&str>, state: &str, reason: Option<&str>) -> VoiceServerFrame {
    VoiceServerFrame::State(VoiceStateFrame {
        voice_session_id: session_id.map(str::to_owned),
        state: state.to_owned(),
        reason: reason.map(str::to_owned),
    })
}

/// Voice socket registry: user-scoped fan-out over the shared manager.
#[derive(Clone)]
pub struct VoiceSockets {
    manager: Arc<WebSocketManager>,
}

impl VoiceSockets {
    pub fn new(manager: Arc<WebSocketManager>) -> Self {
        Self { manager }
    }

    pub fn manager(&self) -> &Arc<WebSocketManager> {
        &self.manager
    }

    /// Send to every voice socket of `user_id` (never to other users).
    pub fn send_user(&self, user_id: &str, frame: &VoiceServerFrame) {
        if let Some(msg) = to_message(frame) {
            self.manager.broadcast_to_user(user_id, msg);
        }
    }

    pub fn send_conn(&self, conn_id: ConnectionId, frame: &VoiceServerFrame) {
        if let Some(msg) = to_message(frame) {
            self.manager.send_to(conn_id, msg);
        }
    }

    /// Close every voice socket of the user (kill switch, revoked session).
    pub fn close_user(&self, user_id: &str, reason: &str) -> usize {
        self.manager.disconnect_user(user_id, reason)
    }

    pub fn count_for_user(&self, user_id: &str) -> usize {
        self.manager.client_count_for_user(user_id)
    }
}
