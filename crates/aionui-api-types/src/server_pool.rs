//! Server access pools (card C36, TASK-130): SSH servers the owner registers
//! once and assigns through the kit attach levels (`item_kind = "server"`).
//!
//! Request types are `deny_unknown_fields`: the UI must send exactly these
//! fields. The private key is write-only: no response ever carries it, only
//! `has_key`.
//!
//! Errors (`ApiError` code, HTTP status):
//! | code | status | when |
//! |---|---|---|
//! | `SERVER_INVALID` | 400 | a field fails validation (name, host, port, user, key, environment, tier, labels) |
//! | `SERVER_NAME_TAKEN` | 409 | another live server of the user has this name |
//! | `SERVER_NOT_FOUND` | 404 | no live server with this id |
//! | `MCP_MARKET_SESSION_REQUIRED` | 403 | a write from an agent (runtime token) or `--local` caller |

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Shown with every server: the tier is advice to the agent, not a lock.
pub const SERVER_TIER_NOTE: &str = "The read/write tier is guidance for agents only: AionUi cannot tell a read \
from a write inside a shell. Restrict what agents may do through the project's rules and instructions; on a \
production server with the write tier, crucial commands always ask the owner first.";

/// `GET /api/servers`, `GET /api/servers/{id}` and every write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerResponse {
    pub id: String,
    /// What agents call it (`[a-z0-9][a-z0-9-]*`).
    pub name: String,
    pub host: String,
    pub port: u16,
    pub ssh_user: String,
    /// known_hosts key part(s) (`<type> <base64>`, one per line); empty until set.
    pub host_key: String,
    pub labels: Vec<String>,
    /// `production` | `staging` | `sandbox` | `local`.
    pub environment: String,
    /// `read` | `write` — guidance only, see [`SERVER_TIER_NOTE`].
    pub tier: String,
    pub notes: String,
    pub enabled: bool,
    /// A private key is stored (it is never returned).
    pub has_key: bool,
    /// `production` + `write`: crucial commands ask the owner first.
    pub crucial_commands_ask: bool,
    pub tier_note: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `POST /api/servers`. Stored keys only (owner decision 2026-11-17): no
/// agent-forwarding mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateServerRequest {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub ssh_user: String,
    /// OpenSSH private key (PEM/OpenSSH format, no passphrase). Write-only.
    pub private_key: String,
    #[serde(default)]
    pub host_key: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `PUT /api/servers/{id}`: PATCH semantics, absent fields keep their value.
/// A new `private_key` replaces the stored one; changing `host`, `port` or
/// `ssh_user` without a new key keeps the key (the owner re-enters the key in
/// the UI when the destination changes) but clears `host_key` unless one is
/// sent, so the old host's key is never trusted for a new destination.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateServerRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub ssh_user: Option<String>,
    #[serde(default)]
    pub private_key: Option<String>,
    #[serde(default)]
    pub host_key: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Query of `GET /api/servers/audit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListServerAuditQuery {
    #[serde(default)]
    pub server_id: Option<String>,
    /// 1..=500, default 100.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// One audited `run_command` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerAuditResponse {
    pub id: String,
    pub conversation_id: String,
    pub server_id: String,
    pub server_name: String,
    /// `standard` | `crucial` (the MCP server the agent called).
    pub channel: String,
    /// `ran` | `failed` | `needs_approval` | `refused`.
    pub decision: String,
    /// Why: the crucial rule that matched, or the refusal.
    pub reason: String,
    /// The command, secrets redacted, truncated.
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    pub created_at: TimestampMs,
}
