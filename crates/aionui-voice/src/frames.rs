//! Outbound `/api/voice/ws` frames, delivered through a dedicated
//! `WebSocketManager` (reused from `aionui-realtime`) that holds only voice sockets.

use std::sync::Arc;

use aionui_api_types::{
    VoiceCallEnded, VoiceErrorFrame, VoiceQuotesAttached, VoiceServerCapabilities, VoiceServerFrame, VoiceStateFrame,
    WebSocketMessage,
};
use aionui_realtime::{ConnectionId, WebSocketManager};
use dashmap::DashMap;
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
        capabilities: None,
    })
}

/// What this backend supports on the socket, advertised on the hello reply
/// and every `voice.ready` (TASK-118: `voice.attachQuotes`).
pub(crate) const SERVER_CAPABILITIES: VoiceServerCapabilities = VoiceServerCapabilities { attach_quotes: true };

/// The hello reply without a ready call: `voice.state` armed + capabilities.
pub(crate) fn hello_armed_frame(session_id: Option<&str>, reason: Option<&str>) -> VoiceServerFrame {
    VoiceServerFrame::State(VoiceStateFrame {
        voice_session_id: session_id.map(str::to_owned),
        state: "armed".to_owned(),
        reason: reason.map(str::to_owned),
        capabilities: Some(SERVER_CAPABILITIES),
    })
}

/// `voice.quotesAttached` (TASK-118).
pub(crate) fn quotes_attached_frame(session_id: &str, consumed: bool, reason: &str) -> VoiceServerFrame {
    VoiceServerFrame::QuotesAttached(VoiceQuotesAttached {
        voice_session_id: session_id.to_owned(),
        consumed,
        reason: reason.to_owned(),
    })
}

/// Voice socket registry: user-scoped fan-out over the shared manager.
#[derive(Clone)]
pub struct VoiceSockets {
    manager: Arc<WebSocketManager>,
    /// Sockets whose hello said the device is armed in Jarvis mode (C26):
    /// connection → user. They never keep a conversation binding alive.
    jarvis: Arc<DashMap<ConnectionId, String>>,
}

impl VoiceSockets {
    pub fn new(manager: Arc<WebSocketManager>) -> Self {
        Self {
            manager,
            jarvis: Arc::new(DashMap::new()),
        }
    }

    /// Record the mode a socket's hello announced (`None`: unknown, an older client).
    pub(crate) fn set_jarvis(&self, conn_id: ConnectionId, user_id: &str, jarvis: bool) {
        if jarvis {
            self.jarvis.insert(conn_id, user_id.to_owned());
        } else {
            self.jarvis.remove(&conn_id);
        }
    }

    /// The socket closed.
    pub(crate) fn forget(&self, conn_id: ConnectionId) {
        self.jarvis.remove(&conn_id);
    }

    /// Connected sockets of the user not known to be in Jarvis mode: only
    /// these keep a conversation binding alive.
    pub fn conversation_capable_count(&self, user_id: &str) -> usize {
        let jarvis = self.jarvis.iter().filter(|e| e.value() == user_id).count();
        self.count_for_user(user_id).saturating_sub(jarvis)
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
