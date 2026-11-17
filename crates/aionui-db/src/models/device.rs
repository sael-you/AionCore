//! Rows of migration 911 (C38 devices registry).

use serde::{Deserialize, Serialize};

/// A phone, Mac or add-on reached through a relay client (or hosted on the
/// server), with its agent switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DeviceRow {
    pub id: String,
    pub user_id: String,
    pub kind: String,
    pub pairing_id: Option<String>,
    pub source_id: String,
    pub name: String,
    pub enabled: bool,
    pub enabled_until: Option<i64>,
    pub meta: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: Option<i64>,
}

/// A relay client. `token_sha256` is the hex SHA-256 of the token; the token
/// itself is never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevicePairingRow {
    pub id: String,
    pub user_id: String,
    pub kind: String,
    pub name: String,
    pub token_sha256: String,
    pub version: Option<String>,
    pub capabilities: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DeviceAuditRow {
    pub id: i64,
    pub user_id: String,
    pub device_id: String,
    pub at: i64,
    pub verb: String,
    pub package: Option<String>,
    pub origin: Option<String>,
    pub outcome: String,
    pub conversation_id: Option<String>,
    pub detail: Option<String>,
}
