//! Row models for the project board tables (migration 901).
//!
//! Enum-like columns are TEXT (validated by SQL CHECKs and the board service);
//! JSON columns are TEXT and parsed by the service layer.

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// `board_settings`: one row per board (= per standard project).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardSettingsRow {
    pub project_id: String,
    pub user_id: String,
    pub enabled: bool,
    /// Always `studio` (D39).
    pub template: String,
    /// The number the next created card receives (TASK-N).
    pub next_number: i64,
    /// Guards settings and the columns collection.
    pub version: i64,
    /// JSON object.
    pub settings: String,
    /// Set while the project is archived; the board is read-only then.
    pub archived_at: Option<TimestampMs>,
    /// The project's Board Keeper conversation (D40).
    pub keeper_conversation_id: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `board_columns`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardColumnRow {
    pub id: String,
    pub project_id: String,
    pub user_id: String,
    pub key: String,
    /// NULL = the UI renders the i18n default `board.columns.<key>`.
    pub name: Option<String>,
    /// `backlog` | `todo` | `in_progress` | `done`.
    pub category: String,
    /// `normal` | `gate` | `terminal`.
    pub kind: String,
    /// `completed` | `dropped` | NULL.
    pub resolution: Option<String>,
    /// `allow` | `deny`.
    pub agent_move: String,
    pub wip_limit: Option<i64>,
    /// JSON object or NULL.
    pub exit_rule: Option<String>,
    /// JSON object or NULL.
    pub gate: Option<String>,
    pub position: i64,
    pub archived: bool,
}

/// `board_cards`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardCardRow {
    pub id: String,
    pub project_id: String,
    pub user_id: String,
    pub number: i64,
    pub title: String,
    pub description: String,
    #[sqlx(rename = "type")]
    pub card_type: String,
    pub priority: String,
    pub size: Option<String>,
    pub tier: Option<String>,
    pub column_key: String,
    pub position: i64,
    /// JSON array of strings.
    pub labels: String,
    /// JSON array of strings.
    pub skills: String,
    /// JSON array of department names.
    pub routing: String,
    pub creator_kind: String,
    pub creator_ref: String,
    pub assignee_kind: Option<String>,
    pub assignee_ref: Option<String>,
    pub blocked: bool,
    pub blocked_reason: Option<String>,
    pub change_id: Option<String>,
    /// JSON object.
    pub metadata: String,
    pub version: i64,
    pub archived_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `board_card_reviews`: the current verdict of one department.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardReviewRow {
    pub id: String,
    pub card_id: String,
    pub project_id: String,
    pub user_id: String,
    pub department: String,
    pub reviewer_ref: String,
    pub conversation_id: Option<String>,
    pub verdict: String,
    pub summary: String,
    /// JSON array.
    pub findings: String,
    pub recommendation: String,
    /// JSON array.
    pub assumptions: String,
    pub confidence: Option<f64>,
    pub tokens_used: i64,
    pub version: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `board_card_questions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardQuestionRow {
    pub id: String,
    pub card_id: String,
    pub project_id: String,
    pub user_id: String,
    pub from_department: String,
    pub to_department: String,
    pub text: String,
    /// Id of the `commented` event that answered it; NULL while open.
    pub answered_by_event: Option<String>,
    pub version: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `board_card_links`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardLinkRow {
    pub card_id: String,
    pub conversation_id: String,
    pub project_id: String,
    pub user_id: String,
    pub role: String,
    pub department: Option<String>,
    pub created_at: TimestampMs,
}

/// A link joined with the linked conversation's current state (`None` when
/// the conversation no longer exists — shown as "removed").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardLinkViewRow {
    pub card_id: String,
    pub conversation_id: String,
    pub project_id: String,
    pub user_id: String,
    pub role: String,
    pub department: Option<String>,
    pub created_at: TimestampMs,
    pub conversation_name: Option<String>,
    pub conversation_status: Option<String>,
    pub conversation_exists: bool,
    pub conversation_archived_at: Option<TimestampMs>,
}

/// `board_card_deps`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardDepRow {
    pub card_id: String,
    pub depends_on_card_id: String,
    pub project_id: String,
    pub user_id: String,
    pub created_at: TimestampMs,
}

/// `board_events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardEventRow {
    pub seq: i64,
    pub id: String,
    pub card_id: Option<String>,
    pub project_id: String,
    pub user_id: String,
    pub actor_kind: String,
    pub actor_ref: String,
    pub conversation_id: Option<String>,
    pub kind: String,
    /// JSON object.
    pub payload: String,
    pub created_at: TimestampMs,
}

/// `board_idempotency`. Written by gate decisions and assign (S4+).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardIdempotencyRow {
    pub user_id: String,
    pub project_id: String,
    pub key: String,
    pub request_hash: String,
    /// `pending` | `done`.
    pub state: String,
    pub assignment_id: Option<String>,
    /// JSON response stored for replays.
    pub response: Option<String>,
    /// Fencing token of the current reservation holder (assign takeover).
    pub claim_token: Option<String>,
    /// Stable id of the delivered packet message (assign resend).
    pub packet_message_id: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `board_deploy_jobs`. Written by the Deploy approval and the supervisor (S5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardDeployJobRow {
    pub id: String,
    pub card_id: String,
    pub project_id: String,
    pub user_id: String,
    pub idempotency_key: String,
    /// `requested` | `running` | `ok` | `failed`.
    pub state: String,
    /// JSON object (result, record path, SHAs).
    pub detail: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `board_supervisor_cursor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BoardSupervisorCursorRow {
    pub project_id: String,
    pub user_id: String,
    pub last_seq: i64,
    pub updated_at: TimestampMs,
}
