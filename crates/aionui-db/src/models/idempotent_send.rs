use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for `message_idempotency_keys`: the ids a keyed send produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct IdempotentSendRow {
    pub conversation_id: String,
    pub idempotency_key: String,
    pub user_id: String,
    pub msg_id: String,
    pub turn_id: String,
    pub delivered_midturn: bool,
    pub created_at: TimestampMs,
}
