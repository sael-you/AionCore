use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `voice_settings` table (one row per user).
///
/// A missing row means "defaults" — in particular voice stays disabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct VoiceSettingsRow {
    pub user_id: String,
    pub enabled: bool,
    pub engine: String,
    pub model: String,
    pub car_mode: bool,
    pub voice: String,
    pub eagerness: String,
    pub daily_budget_micros: i64,
    pub provider_id: Option<String>,
    /// JSON object with settings added by later slices.
    pub extra: String,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `voice_sessions` table (one provider call).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct VoiceSessionRow {
    pub id: String,
    pub user_id: String,
    pub engine: String,
    pub model: String,
    pub call_id: Option<String>,
    /// `starting` | `live` | `ended` | `failed`.
    pub state: String,
    pub end_reason: Option<String>,
    pub resume_of: Option<String>,
    pub device: Option<String>,
    pub instructions_hash: Option<String>,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub responses: i64,
    pub unknown_responses: i64,
    pub stt_seconds: i64,
    pub cost_micros: i64,
    pub created_at: TimestampMs,
    pub ready_at: Option<TimestampMs>,
    pub ended_at: Option<TimestampMs>,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `voice_audit` table (metadata only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct VoiceAuditRow {
    pub id: i64,
    pub user_id: String,
    pub session_id: Option<String>,
    pub event: String,
    /// JSON object (ids, codes, amounts — never transcripts or keys).
    pub detail: String,
    pub created_at: TimestampMs,
}
