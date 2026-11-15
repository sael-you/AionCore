//! Engine-neutral port (§3.1): `open`, `listen`, `speak`, `interrupt`,
//! `close` + events. Drafts, budgets and confirmation rules only see these
//! types, never provider event names.

pub mod openai;
mod openai_config;
mod openai_events;

use std::time::Duration;

use crate::credentials::ResolvedCredential;
use crate::pricing::Usage;

pub use openai::OpenAiRealtimeEngine;

/// Server-owned call configuration (§3.1). Mid-session only
/// `create_response` may change (keeps the provider cache prefix stable).
#[derive(Debug, Clone, PartialEq)]
pub struct EngineSessionConfig {
    pub model: String,
    pub instructions: String,
    pub voice: String,
    pub eagerness: String,
    pub noise_reduction: String,
    pub transcription_model: String,
    /// Project/conversation names that bias transcription (S1b fills it).
    pub keywords: Vec<String>,
    pub reasoning_effort: String,
    pub max_output_tokens: u64,
    pub create_response: bool,
    pub tools: Vec<serde_json::Value>,
}

/// Inputs to [`VoiceEngine::open`].
#[derive(Debug, Clone)]
pub struct OpenCallRequest {
    pub credential: ResolvedCredential,
    pub sdp_offer: String,
    pub config: EngineSessionConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    User,
    Assistant,
}

impl TranscriptRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// How `speak` voices a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakMode {
    /// Exact wording, outside the conversation (read-backs, follow-ups).
    Exact,
}

/// Engine-neutral events.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    UserSpeechStarted {
        audio_start_ms: Option<u64>,
    },
    UserSpeechStopped {
        audio_end_ms: Option<u64>,
    },
    InputCommitted {
        item_id: Option<String>,
    },
    Transcript {
        role: TranscriptRole,
        text: String,
        is_final: bool,
        /// Transcription usage in whole seconds (user finals only).
        stt_seconds: Option<u64>,
    },
    ResponseStarted {
        response_id: String,
    },
    ResponseDone {
        response_id: String,
        status: String,
        usage: Option<Usage>,
    },
    AssistantAudioStarted {
        response_id: Option<String>,
    },
    AssistantAudioFinished {
        response_id: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Unsolicited config acknowledgement.
    ConfigApplied,
    ProviderError {
        code: String,
    },
    /// The control channel closed without us closing it.
    ControlLost,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// Call creation refused with this HTTP status.
    #[error("provider refused the call (HTTP {0})")]
    Rejected(u16),
    /// The provider no longer knows the call (hung up / expired).
    #[error("provider call not found")]
    CallGone,
    #[error("control channel attach failed: {0}")]
    AttachFailed(String),
    /// No `session.updated` for our `session.update` in time (§3.6.2).
    #[error("provider did not acknowledge the session config in time")]
    ConfigTimeout,
    #[error("control channel is not attached")]
    NotAttached,
    #[error("transport error: {0}")]
    Transport(String),
}

/// Timeouts of the engine adapters (production defaults per §3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineTimings {
    /// Sideband attach retries on 503 for up to this long.
    pub attach_window: Duration,
    pub attach_retry_delay: Duration,
    /// Wait for `session.updated` after `session.update`.
    pub config_gate: Duration,
    pub http_timeout: Duration,
}

impl Default for EngineTimings {
    fn default() -> Self {
        Self {
            attach_window: Duration::from_secs(5),
            attach_retry_delay: Duration::from_millis(250),
            config_gate: Duration::from_secs(5),
            http_timeout: Duration::from_secs(15),
        }
    }
}

/// Creates calls on one provider.
#[async_trait::async_trait]
pub trait VoiceEngine: Send + Sync {
    /// Engine id (`openai-realtime`, …).
    fn kind(&self) -> &'static str;

    /// Create the provider call. Returns the SDP answer at once; the control
    /// channel attaches later in [`EngineCall::listen`].
    async fn open(&self, request: OpenCallRequest) -> Result<(String, Box<dyn EngineCall>), EngineError>;

    /// Handle to an existing call (restart recovery); nothing is sent yet.
    fn resume(
        &self,
        credential: ResolvedCredential,
        call_id: String,
        config: EngineSessionConfig,
    ) -> Box<dyn EngineCall>;
}

/// One live provider call.
#[async_trait::async_trait]
pub trait EngineCall: Send {
    fn call_id(&self) -> &str;

    /// Attach the control channel and apply the server-owned config; resolves
    /// when the provider acknowledged it. Audio stays muted client-side until then.
    async fn listen(&mut self) -> Result<(), EngineError>;

    /// Re-attach after [`EngineEvent::ControlLost`] (lossy, §3.6.4).
    async fn reattach(&mut self) -> Result<(), EngineError>;

    /// Next event. Cancel-safe. `None` = control channel not attached.
    async fn next_event(&mut self) -> Option<EngineEvent>;

    /// Toggle provider-initiated responses; resolves on acknowledgement.
    async fn set_create_response(&mut self, enabled: bool) -> Result<(), EngineError>;

    /// Backend-issued response (budget is reserved by the caller first).
    async fn create_response(&mut self) -> Result<(), EngineError>;

    /// Say `text` (see [`SpeakMode`]).
    async fn speak(&mut self, text: &str, mode: SpeakMode) -> Result<(), EngineError>;

    /// Cancel the response in progress.
    async fn interrupt(&mut self) -> Result<(), EngineError>;

    /// Answer a tool call (`call_id` from [`EngineEvent::ToolCall`]).
    async fn tool_result(&mut self, call_id: &str, output: &str) -> Result<(), EngineError>;

    /// Add an assistant text item to the model's context (e.g. the read-back
    /// that was played as a local clip, so "change it to…" works).
    async fn add_context(&mut self, text: &str) -> Result<(), EngineError>;

    /// Hang up the provider call. An already-ended call is not an error.
    async fn close(&mut self, reason: &str) -> Result<(), EngineError>;
}
