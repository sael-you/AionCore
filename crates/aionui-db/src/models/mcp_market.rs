use aionui_common::TimestampMs;

/// A `mcp_servers` row seen through the market columns added by migration 904.
///
/// Kept apart from [`super::McpServerRow`] so the many non-market readers of
/// that struct are unchanged (`SELECT *` ignores the extra columns there).
/// `market_status` is `NULL` for ordinary rows; every other `market_*` column
/// is meaningful only when it is set. JSON columns are opaque strings here.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct McpMarketRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub description: Option<String>,
    pub transport_type: String,
    /// JSON tool list from the last connection test.
    pub tools: Option<String>,
    pub last_test_status: String,
    pub last_connected: Option<TimestampMs>,
    pub deleted_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub market_status: Option<String>,
    pub market_template_id: Option<String>,
    pub market_template: Option<String>,
    pub market_environment: Option<String>,
    pub market_tier: Option<String>,
    pub market_enforcement: Option<String>,
    pub market_clis: Option<String>,
    pub market_plain_env: Option<String>,
    pub market_secret_refs: Option<String>,
    pub market_tools_allow: Option<String>,
    pub market_tools_block: Option<String>,
    pub market_approval_ref: Option<String>,
}

impl McpMarketRow {
    /// True when the row is a market instance (whatever its status).
    pub fn is_market(&self) -> bool {
        self.market_status.is_some()
    }
}

/// One encrypted secret of a market instance (`mcp_secrets`, migration 904).
///
/// `value_encrypted` is the provider-key AES-256-GCM ciphertext; it never
/// leaves the backend except to the launcher, which decrypts it in-process.
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct McpSecretRow {
    pub id: String,
    pub user_id: String,
    pub instance_id: String,
    pub name: String,
    pub value_encrypted: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

impl std::fmt::Debug for McpSecretRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ciphertext is not a secret on its own, but keep it out of logs anyway.
        f.debug_struct("McpSecretRow")
            .field("id", &self.id)
            .field("instance_id", &self.instance_id)
            .field("name", &self.name)
            .field("updated_at", &self.updated_at)
            .finish_non_exhaustive()
    }
}

/// One normalized market assignment (`mcp_assignments`, migration 905).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct McpAssignmentRow {
    pub id: String,
    pub user_id: String,
    pub mcp_server_id: String,
    /// `None` = global.
    pub project_id: Option<String>,
    /// `None` = the whole project (native files); `Some` = one assistant (AionUi binding).
    pub assistant_id: Option<String>,
    pub cli: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
