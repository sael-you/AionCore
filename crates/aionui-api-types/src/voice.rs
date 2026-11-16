//! Voice Concierge ("Jarvis", card C9) HTTP and `/api/voice/ws` contracts.
//!
//! WebSocket frames use the standard `WebSocketMessage` envelope
//! (`{"name": "voice.camelCaseAction", "data": {...}}`); every frame carries
//! `voiceSessionId` so the backend can check it against the socket's user.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `GET /api/voice/settings` / `PUT` response. REST bodies are snake_case
/// (like most core DTOs); WebSocket frame data is camelCase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceSettingsResponse {
    /// Kill switch: off by default; off hangs up calls and refuses sessions.
    pub enabled: bool,
    pub engine: String,
    pub wake_word_enabled: bool,
    pub car_mode: bool,
    pub daily_budget_usd: f64,
    /// Spent today (UTC day) across the user's sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_today_usd: Option<f64>,
    pub model: String,
    pub voice: String,
    pub eagerness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
}

/// `PUT /api/voice/settings` body. Absent fields keep their value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateVoiceSettingsRequest {
    pub enabled: Option<bool>,
    pub engine: Option<String>,
    pub wake_word_enabled: Option<bool>,
    pub car_mode: Option<bool>,
    pub daily_budget_usd: Option<f64>,
    pub model: Option<String>,
    pub voice: Option<String>,
    pub eagerness: Option<String>,
    /// `Some("")` clears the pinned provider.
    pub provider_id: Option<String>,
    /// Read-only in responses; accepted and ignored so clients can PUT back
    /// what they GET.
    #[serde(default, skip_serializing)]
    pub used_today_usd: Option<f64>,
}

/// `POST /api/voice/sessions` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateVoiceSessionRequest {
    /// Requested engine; must be the configured one when given.
    #[serde(default)]
    pub engine: Option<String>,
    /// WebRTC SDP offer (audio only, no data channel).
    pub sdp_offer: String,
    #[serde(default)]
    pub device_id: Option<String>,
    /// Conversation the user armed voice in (`/voice`, composer mic).
    #[serde(default)]
    pub focus_conversation_id: Option<String>,
    /// Previous session id when re-dialling after a network loss (§3.6.5).
    #[serde(default)]
    pub resume_of: Option<String>,
    /// `jarvis` (default: the dispatcher) | `conversation` (C26: the call is
    /// only an ear for `focus_conversation_id`, whose own agent answers).
    #[serde(default)]
    pub mode: Option<String>,
}

/// `POST /api/voice/sessions` response (201).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateVoiceSessionResponse {
    pub voice_session_id: String,
    /// SDP answer to apply to the browser peer connection.
    pub sdp_answer: String,
    pub engine: String,
    pub model: String,
}

/// `POST /api/voice/sessions/{id}/end` body (optional).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EndVoiceSessionRequest {
    /// `user` | `sleep` | `idle` | `offline` | `network`; others are recorded as `user`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// One voice session (provider call) with its metering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceSessionResponse {
    pub id: String,
    pub engine: String,
    pub model: String,
    /// `starting` | `live` | `ended` | `failed`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub responses: i64,
    pub unknown_responses: i64,
    pub cost_usd: f64,
}

/// `GET /api/voice/status` response (also read by the deploy script).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceStatusResponse {
    pub enabled: bool,
    /// Live provider calls of this user (0 or 1).
    pub live_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    pub used_today_usd: f64,
    pub limit_usd: f64,
}

// ---------------------------------------------------------------------------
// /api/voice/ws frames (PLAN §3.9, S1a set)
// ---------------------------------------------------------------------------

/// Frames the client sends on `/api/voice/ws`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "data")]
pub enum VoiceClientFrame {
    #[serde(rename = "voice.hello")]
    Hello(VoiceHello),
    #[serde(rename = "voice.heartbeat")]
    Heartbeat(VoiceHeartbeat),
    #[serde(rename = "voice.frozen")]
    Frozen(VoiceSessionRef),
    #[serde(rename = "voice.resumed")]
    Resumed(VoiceSessionRef),
    #[serde(rename = "voice.playbackFinished")]
    PlaybackFinished(VoicePlaybackFinished),
    #[serde(rename = "voice.speechOnset")]
    SpeechOnset(VoiceSpeechOnset),
    #[serde(rename = "voice.confirmTap")]
    ConfirmTap(VoiceDraftTap),
    #[serde(rename = "voice.cancelTap")]
    CancelTap(VoiceDraftTap),
    /// Conversation mode "Done": end the user's utterance now (C26).
    #[serde(rename = "voice.commit")]
    Commit(VoiceSessionRef),
}

impl VoiceClientFrame {
    /// The `voiceSessionId` the frame claims, if any.
    pub fn voice_session_id(&self) -> Option<&str> {
        match self {
            Self::Hello(f) => f.voice_session_id.as_deref(),
            Self::Heartbeat(f) => f.voice_session_id.as_deref(),
            Self::Frozen(f) | Self::Resumed(f) | Self::Commit(f) => f.voice_session_id.as_deref(),
            Self::PlaybackFinished(f) => f.voice_session_id.as_deref(),
            Self::SpeechOnset(f) => f.voice_session_id.as_deref(),
            Self::ConfirmTap(f) | Self::CancelTap(f) => f.voice_session_id.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceHello {
    #[serde(default)]
    pub voice_session_id: Option<String>,
    pub device: String,
    #[serde(default)]
    pub capabilities: Value,
    /// The mode the device is armed in: `jarvis` or `conversation` (C26).
    /// `jarvis` ends a conversation binding whose call is gone (a mode
    /// switch with no live call never reaches core otherwise). Absent from
    /// older clients: no effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceHeartbeat {
    #[serde(default)]
    pub voice_session_id: Option<String>,
    pub t: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSessionRef {
    #[serde(default)]
    pub voice_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoicePlaybackFinished {
    #[serde(default)]
    pub voice_session_id: Option<String>,
    pub readback_id: String,
    pub t_end: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSpeechOnset {
    #[serde(default)]
    pub voice_session_id: Option<String>,
    pub t: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceDraftTap {
    #[serde(default)]
    pub voice_session_id: Option<String>,
    pub draft_id: String,
    pub version: i64,
    pub hash: String,
}

/// Frames the backend sends on `/api/voice/ws`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "data")]
pub enum VoiceServerFrame {
    /// Sideband attached and the provider acknowledged the server-owned config:
    /// the client may unmute.
    #[serde(rename = "voice.ready")]
    Ready(VoiceReady),
    #[serde(rename = "voice.state")]
    State(VoiceStateFrame),
    #[serde(rename = "voice.caption")]
    Caption(VoiceCaption),
    #[serde(rename = "voice.assistantAudioFinished")]
    AssistantAudioFinished(VoiceAssistantAudioFinished),
    #[serde(rename = "voice.budget")]
    Budget(VoiceBudget),
    #[serde(rename = "voice.error")]
    Error(VoiceErrorFrame),
    /// Sent exactly once when a call ends, for any reason. The client ends
    /// its call only on this frame (`voice.state` is display-only).
    #[serde(rename = "voice.callEnded")]
    CallEnded(VoiceCallEnded),
    #[serde(rename = "voice.draftUpdated")]
    DraftUpdated(VoiceDraftUpdated),
    #[serde(rename = "voice.readbackReady")]
    ReadbackReady(VoiceReadbackReady),
    #[serde(rename = "voice.followUp")]
    FollowUp(VoiceFollowUp),
    #[serde(rename = "voice.wake")]
    Wake(VoiceWake),
}

impl VoiceServerFrame {
    /// Wire name of the frame (`voice.*`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ready(_) => "voice.ready",
            Self::State(_) => "voice.state",
            Self::Caption(_) => "voice.caption",
            Self::AssistantAudioFinished(_) => "voice.assistantAudioFinished",
            Self::Budget(_) => "voice.budget",
            Self::Error(_) => "voice.error",
            Self::CallEnded(_) => "voice.callEnded",
            Self::DraftUpdated(_) => "voice.draftUpdated",
            Self::ReadbackReady(_) => "voice.readbackReady",
            Self::FollowUp(_) => "voice.followUp",
            Self::Wake(_) => "voice.wake",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceReady {
    pub voice_session_id: String,
    pub engine: String,
    pub model: String,
}

/// `armed` | `listening` | `thinking` | `speaking` | `offline`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceStateFrame {
    pub voice_session_id: Option<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceCaption {
    pub voice_session_id: String,
    /// `user` | `assistant`.
    pub role: String,
    pub text: String,
    #[serde(rename = "final")]
    pub is_final: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceAssistantAudioFinished {
    pub voice_session_id: String,
    pub response_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceBudget {
    pub voice_session_id: Option<String>,
    pub used_usd: f64,
    pub limit_usd: f64,
    /// `"80"` at the 80 % warning, `"100"` when the cap is reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceCallEnded {
    pub voice_session_id: String,
    /// Persisted end reason (`user`, `sleep`, `idle`, `budget`, `rotation`,
    /// `disabled`, `control_lost`, … — see the crate's PROTOCOL.md).
    pub reason: String,
}

/// Draft target as shown on the card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceDraftTarget {
    pub id: String,
    pub name: String,
    /// `conversation` | `project`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// One transcript→draft diff op (`equal` | `insert` | `delete`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceDiffOp {
    pub op: String,
    pub text: String,
    /// `filler` | `reference` | `reorder`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceDraftUpdated {
    pub voice_session_id: Option<String>,
    pub draft_id: String,
    pub version: i64,
    /// Payload hash; a `voice.confirmTap` must carry it.
    pub hash: String,
    pub state: String,
    pub target: Option<VoiceDraftTarget>,
    /// Choices when `state` is `ambiguous`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<VoiceDraftTarget>,
    pub text: String,
    pub diff: Vec<VoiceDiffOp>,
    /// `new_turn` | `steer` | `queue_busy` | `queue_approval` | `new_conversation`.
    pub delivery: String,
    /// Spoken "send" is accepted for this draft (read-back name check passed).
    pub spoken_confirm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_remaining_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_frozen: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceReadbackReady {
    pub voice_session_id: Option<String>,
    pub draft_id: String,
    pub version: i64,
    pub readback_id: String,
    /// Same-origin path: `/api/voice/readbacks/{readbackId}`.
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceFollowUp {
    pub voice_session_id: Option<String>,
    pub conversation_id: String,
    /// `finished` | `failed` | `needs_approval` | `says` | `asks` | draft outcomes
    /// (`queued`, `sent`, `failed`, `saved_not_started`, `expired`, `interrupted`) |
    /// conversation-mode outcomes (`sent`, `added`, `queued`, `failed`, `stopped`, `silenced`).
    pub status: String,
    pub line: String,
    /// Clip id for `/api/voice/readbacks/{clipId}`; absent → show `line` with the tone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceWake {
    pub voice_session_id: Option<String>,
    /// `follow_up` (a follow-up clip) | `says` (agent `say`) | `asks` (agent `ask`).
    pub reason: String,
    pub item_id: String,
}

// ---------------------------------------------------------------------------
// Agent → user (runtime routes, `aioncore voice …`)
// ---------------------------------------------------------------------------

/// `POST /api/runtime/voice/say`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSayRequest {
    pub text: String,
    /// `low` (default, waits for idle) | `high`.
    #[serde(default)]
    pub priority: Option<String>,
}

/// `POST /api/runtime/voice/ask`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceAskRequest {
    pub question: String,
    /// Seconds to block (capped at `max_wait_s`).
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// Answer of `say`, `ask` and `wait`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceAgentReply {
    /// `delivered` | `answered` | `timeout` | `pending` | `channel` (no device:
    /// sent to the messaging channel as text) | `no_device` | `cancelled`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// `GET /api/runtime/voice/capabilities`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceAgentCapabilities {
    pub max_wait_s: u64,
    pub say_per_minute: u32,
    pub note: String,
    /// `on` while the calling conversation is in conversation voice mode (C26).
    pub voice_mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceErrorFrame {
    pub voice_session_id: Option<String>,
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_frames_use_domain_camel_case_envelope() {
        let raw = json!({"name": "voice.hello", "data": {"device": "web", "capabilities": {"webrtc": true}}});
        let frame: VoiceClientFrame = serde_json::from_value(raw).unwrap();
        assert!(matches!(&frame, VoiceClientFrame::Hello(h) if h.device == "web"));
        assert_eq!(frame.voice_session_id(), None);

        let raw = json!({"name": "voice.playbackFinished",
                         "data": {"voiceSessionId": "vs_1", "readbackId": "rb", "tEnd": 1.5}});
        let frame: VoiceClientFrame = serde_json::from_value(raw).unwrap();
        assert_eq!(frame.voice_session_id(), Some("vs_1"));

        let unknown = json!({"name": "voice.nope", "data": {}});
        assert!(serde_json::from_value::<VoiceClientFrame>(unknown).is_err());
    }

    #[test]
    fn server_frames_serialize_with_wire_names() {
        let frame = VoiceServerFrame::Caption(VoiceCaption {
            voice_session_id: "vs_1".into(),
            role: "user".into(),
            text: "hi".into(),
            is_final: true,
        });
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["name"], frame.name());
        assert_eq!(value["data"]["final"], true);
        assert_eq!(value["data"]["voiceSessionId"], "vs_1");

        let ready = VoiceServerFrame::Ready(VoiceReady {
            voice_session_id: "vs_1".into(),
            engine: "openai-realtime".into(),
            model: "gpt-realtime-2.1".into(),
        });
        assert_eq!(serde_json::to_value(&ready).unwrap()["name"], "voice.ready");

        let ended = VoiceServerFrame::CallEnded(VoiceCallEnded {
            voice_session_id: "vs_1".into(),
            reason: "idle".into(),
        });
        let value = serde_json::to_value(&ended).unwrap();
        assert_eq!(value["name"], "voice.callEnded");
        assert_eq!(value["data"]["voiceSessionId"], "vs_1");
        assert_eq!(value["data"]["reason"], "idle");
    }

    #[test]
    fn draft_frame_matches_the_ui_contract() {
        let frame = VoiceServerFrame::DraftUpdated(VoiceDraftUpdated {
            voice_session_id: Some("vs_1".into()),
            draft_id: "d1".into(),
            version: 2,
            hash: "h".into(),
            state: "committing".into(),
            target: Some(VoiceDraftTarget {
                id: "c1".into(),
                name: "Kanban build".into(),
                kind: Some("conversation".into()),
            }),
            options: vec![],
            text: "Goal: rerun".into(),
            diff: vec![VoiceDiffOp {
                op: "delete".into(),
                text: "um".into(),
                transform: Some("filler".into()),
            }],
            delivery: "new_turn".into(),
            spoken_confirm: true,
            hold_remaining_ms: Some(4000),
            hold_frozen: Some(false),
        });
        let v = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["name"], "voice.draftUpdated");
        assert_eq!(v["data"]["target"]["name"], "Kanban build");
        assert_eq!(v["data"]["diff"][0]["transform"], "filler");
        assert_eq!(v["data"]["holdRemainingMs"], 4000);
        assert!(v["data"].get("options").is_none());
    }

    #[test]
    fn settings_update_rejects_unknown_fields() {
        let bad = json!({"enabled": true, "api_key": "nope"});
        assert!(serde_json::from_value::<UpdateVoiceSettingsRequest>(bad).is_err());
        let ok = json!({"enabled": true, "wake_word_enabled": false, "used_today_usd": 1.0});
        assert!(serde_json::from_value::<UpdateVoiceSettingsRequest>(ok).is_ok());
    }

    #[test]
    fn rest_bodies_are_snake_case() {
        let req: CreateVoiceSessionRequest = serde_json::from_value(json!({
            "engine": "openai-realtime", "sdp_offer": "v=0", "device_id": "phone",
            "focus_conversation_id": "c1", "resume_of": null}))
        .unwrap();
        assert_eq!(req.device_id.as_deref(), Some("phone"));
        let resp = serde_json::to_value(CreateVoiceSessionResponse {
            voice_session_id: "vs_1".into(),
            sdp_answer: "v=0".into(),
            engine: "openai-realtime".into(),
            model: "gpt-realtime-2.1".into(),
        })
        .unwrap();
        assert_eq!(resp["voice_session_id"], "vs_1");
        assert_eq!(resp["sdp_answer"], "v=0");
    }
}
