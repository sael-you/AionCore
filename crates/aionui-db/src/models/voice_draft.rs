use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for `voice_drafts` (one row per draft version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct VoiceDraftRow {
    pub id: String,
    pub version: i64,
    pub user_id: String,
    pub session_id: Option<String>,
    /// `send_message` | `create_conversation`.
    pub action: String,
    pub state: String,
    pub queued_reason: Option<String>,
    pub target_id: Option<String>,
    pub target_name: Option<String>,
    pub project_id: Option<String>,
    pub text: Option<String>,
    pub payload_hash: String,
    pub readback_id: Option<String>,
    pub readback_text: Option<String>,
    pub spoken_confirm: bool,
    pub delivery: Option<String>,
    pub hold_deadline_at: Option<TimestampMs>,
    pub send_started_at: Option<TimestampMs>,
    pub outcome_msg_id: Option<String>,
    pub outcome_turn_id: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_until: Option<TimestampMs>,
    pub expires_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
