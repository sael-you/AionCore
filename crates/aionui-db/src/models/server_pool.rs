use aionui_common::TimestampMs;

/// A registered SSH server (`servers`, migration 910). `has_key` is computed
/// (`EXISTS` on `server_secrets`); the ciphertext never rides on this row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ServerRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub host: String,
    pub port: i64,
    pub ssh_user: String,
    /// known_hosts key part(s), one per line; empty = trust on first use.
    pub host_key: String,
    /// JSON array of strings.
    pub labels: String,
    /// `production` | `staging` | `sandbox` | `local`.
    pub environment: String,
    /// `read` | `write` — guidance only.
    pub tier: String,
    pub notes: String,
    pub enabled: bool,
    pub has_key: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub deleted_at: Option<TimestampMs>,
}

/// One audited call (`server_audit`, migration 910).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ServerAuditRow {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub server_id: String,
    pub server_name: String,
    /// `standard` | `crucial`.
    pub channel: String,
    /// `ran` | `failed` | `needs_approval` | `refused`.
    pub decision: String,
    pub reason: String,
    pub command: String,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub created_at: TimestampMs,
}
