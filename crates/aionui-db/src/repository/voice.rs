use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{VoiceAuditRow, VoiceSessionRow, VoiceSettingsRow};

/// Per-user Voice Concierge settings (`voice_settings`).
#[async_trait::async_trait]
pub trait IVoiceSettingsRepository: Send + Sync {
    /// Returns the user's settings row, or `None` when never saved (defaults apply).
    async fn get(&self, user_id: &str) -> Result<Option<VoiceSettingsRow>, DbError>;

    /// Inserts or replaces the user's settings row.
    async fn upsert(&self, row: &VoiceSettingsRow) -> Result<(), DbError>;
}

/// Voice sessions = provider calls (`voice_sessions`).
#[async_trait::async_trait]
pub trait IVoiceSessionRepository: Send + Sync {
    /// Inserts a `starting` session. Returns [`DbError::Conflict`] when the user
    /// already has a `starting`/`live` session (one live call per user).
    async fn create_starting(&self, params: CreateVoiceSessionParams<'_>) -> Result<VoiceSessionRow, DbError>;

    /// `starting` → `live` with the provider call id. `NotFound` if the row is
    /// not in `starting`.
    async fn mark_live(&self, user_id: &str, id: &str, call_id: &str) -> Result<(), DbError>;

    /// Records when the provider acknowledged the server-owned config.
    async fn mark_ready(&self, user_id: &str, id: &str, ready_at: TimestampMs) -> Result<(), DbError>;

    /// Adds usage and cost deltas to the session row; the cost is attributed
    /// to the UTC day of `charged_at` (daily budget).
    async fn add_usage(
        &self,
        user_id: &str,
        id: &str,
        delta: &VoiceUsageDelta,
        charged_at: TimestampMs,
    ) -> Result<(), DbError>;

    /// Moves a `starting`/`live` session to a terminal state. Returns `false`
    /// when it was already terminal (idempotent end).
    async fn finish(&self, user_id: &str, id: &str, state: &str, reason: &str) -> Result<bool, DbError>;

    async fn get(&self, user_id: &str, id: &str) -> Result<Option<VoiceSessionRow>, DbError>;

    /// Most recent sessions first.
    async fn list(&self, user_id: &str, limit: u32) -> Result<Vec<VoiceSessionRow>, DbError>;

    /// Every `starting`/`live` session of every user (startup recovery).
    async fn list_open(&self) -> Result<Vec<VoiceSessionRow>, DbError>;

    /// Cost charged to the user on UTC days starting at or after the day of `since`.
    async fn cost_since(&self, user_id: &str, since: TimestampMs) -> Result<i64, DbError>;
}

/// Append-only voice audit log (`voice_audit`).
#[async_trait::async_trait]
pub trait IVoiceAuditRepository: Send + Sync {
    async fn append(&self, entry: NewVoiceAuditEntry<'_>) -> Result<(), DbError>;

    /// Most recent entries first.
    async fn list(&self, user_id: &str, limit: u32) -> Result<Vec<VoiceAuditRow>, DbError>;
}

/// Parameters for [`IVoiceSessionRepository::create_starting`].
#[derive(Debug)]
pub struct CreateVoiceSessionParams<'a> {
    pub id: &'a str,
    pub user_id: &'a str,
    pub engine: &'a str,
    pub model: &'a str,
    pub resume_of: Option<&'a str>,
    pub device: Option<&'a str>,
    pub instructions_hash: Option<&'a str>,
}

/// Usage/cost increments for [`IVoiceSessionRepository::add_usage`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceUsageDelta {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub responses: i64,
    pub unknown_responses: i64,
    pub stt_seconds: i64,
    pub cost_micros: i64,
}

/// Parameters for [`IVoiceAuditRepository::append`].
#[derive(Debug)]
pub struct NewVoiceAuditEntry<'a> {
    pub user_id: &'a str,
    pub session_id: Option<&'a str>,
    pub event: &'a str,
    /// JSON object string.
    pub detail: &'a str,
}
