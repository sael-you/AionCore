//! Voice Concierge "Jarvis" (card C9, design/voice/PLAN.md v15).
//!
//! S1a: settings + kill switch, admission and the `openai-realtime` call
//! lifecycle (call create → sideband → `session.updated` gate → hangup),
//! budget reservations, and the `/api/voice/ws` control socket, all behind
//! the §3.5 auth rules. Voice depends only on its own ports; `aionui-app`
//! supplies the adapters.

mod admission;
mod agent;
mod agent_routes;
mod agent_service;
mod budget;
mod conversation_port;
mod credentials;
pub mod drafts;
mod engine;
mod error;
mod followup;
mod frames;
mod guard;
mod instructions;
mod lifecycle;
mod ports;
mod pricing;
mod readback;
mod refiner;
pub mod resolver;
mod routes;
mod runner;
mod runner_drafts;
mod service;
mod settings;
mod speech;
mod state;
pub mod tools;
mod ws;

#[cfg(feature = "voice-test")]
pub mod fake_provider;

pub use agent_routes::{VoiceAgentRouterState, voice_agent_routes};
pub use budget::{BudgetMeter, BudgetThreshold, VOICE_MAX_OUTPUT_TOKENS};
pub use conversation_port::{
    ConversationInfo, ConversationPort, ConversationStatus, MessageHit, ProjectInfo, SendOutcome, SendProbe,
};
pub use credentials::{CredentialPolicy, FAKE_PROVIDER_URL_ENV, OPENAI_OFFICIAL_BASE_URL, ResolvedCredential};
pub use engine::{
    EngineCall, EngineError, EngineEvent, EngineSessionConfig, EngineTimings, OpenAiRealtimeEngine, OpenCallRequest,
    SpeakMode, TranscriptRole, VoiceEngine,
};
pub use error::VoiceError;
pub use frames::VoiceSockets;
pub use guard::{OriginAllowlist, SessionTokenVerifier, VoiceHttpGuards};
pub use lifecycle::{EndReason, VoiceTimings};
pub use ports::{ApiKey, AuthRejected, AuthVerifier, CredentialPort, ProviderCredential, VerifiedUser};
pub use readback::{Clip, ClipVoice, SynthPort, TranscribePort};
pub use refiner::{RawRefinement, RefineInput, RefinerPort};
pub use routes::voice_routes;
pub use service::{VoiceService, VoiceServiceDeps};
pub use settings::VoiceSettings;
pub use speech::ProviderSpeechFactory;
pub use state::VoiceRouterState;
