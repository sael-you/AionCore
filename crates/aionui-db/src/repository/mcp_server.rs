use crate::error::DbError;
use crate::models::McpServerRow;

/// MCP server configuration data access abstraction.
///
/// Provides CRUD operations, batch upsert, and name-based lookup
/// on the `mcp_servers` table. JSON fields (`transport_config`, `tools`)
/// are opaque strings at this layer; the service layer handles
/// serialization/deserialization.
///
/// Object-safe via `async_trait` to support `Arc<dyn IMcpServerRepository>`.
#[async_trait::async_trait]
pub trait IMcpServerRepository: Send + Sync {
    /// Returns all MCP servers, ordered by creation time ascending.
    async fn list(&self, user_id: &str) -> Result<Vec<McpServerRow>, DbError>;

    /// Finds an MCP server by ID, or `None` if not found.
    async fn find_by_id(&self, user_id: &str, id: &str) -> Result<Option<McpServerRow>, DbError>;

    /// Finds an MCP server by name, or `None` if not found.
    async fn find_by_name(&self, user_id: &str, name: &str) -> Result<Option<McpServerRow>, DbError>;

    /// Finds an MCP server by ID, including soft-deleted rows.
    async fn find_by_id_any(&self, user_id: &str, id: &str) -> Result<Option<McpServerRow>, DbError> {
        self.find_by_id(user_id, id).await
    }

    /// Finds an MCP server by name, including soft-deleted rows.
    async fn find_by_name_any(&self, user_id: &str, name: &str) -> Result<Option<McpServerRow>, DbError> {
        self.find_by_name(user_id, name).await
    }

    /// Finds a set of MCP servers by ID, including soft-deleted rows.
    async fn list_by_ids_any(&self, user_id: &str, ids: &[String]) -> Result<Vec<McpServerRow>, DbError> {
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = self.find_by_id_any(user_id, id).await? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Creates a new MCP server and returns the inserted row.
    /// Returns `DbError::Conflict` if the name already exists.
    async fn create(&self, params: CreateMcpServerParams<'_>) -> Result<McpServerRow, DbError>;

    /// Updates an existing MCP server. Returns `DbError::NotFound` if the ID
    /// doesn't exist, `DbError::Conflict` if the new name collides with another.
    async fn update(&self, user_id: &str, id: &str, params: UpdateMcpServerParams<'_>)
    -> Result<McpServerRow, DbError>;

    /// Soft-deletes an MCP server by ID. Returns `DbError::NotFound` if the ID
    /// doesn't exist.
    async fn delete(&self, user_id: &str, id: &str) -> Result<(), DbError>;

    /// Upserts multiple servers by name: existing names are updated,
    /// new names are inserted. Returns the count of affected rows.
    async fn batch_upsert(
        &self,
        user_id: &str,
        servers: &[CreateMcpServerParams<'_>],
    ) -> Result<Vec<McpServerRow>, DbError>;

    /// Updates only the latest connection-test result status
    /// (and optionally `last_connected`).
    /// Returns `DbError::NotFound` if the ID doesn't exist.
    async fn update_status(
        &self,
        user_id: &str,
        id: &str,
        status: &str,
        last_connected: Option<aionui_common::TimestampMs>,
    ) -> Result<(), DbError>;

    /// Updates only the tools JSON for a server.
    /// Returns `DbError::NotFound` if the ID doesn't exist.
    async fn update_tools(&self, user_id: &str, id: &str, tools: Option<&str>) -> Result<(), DbError>;

    /// The ONE market predicate (MCP market, R1 #5) every session MCP
    /// selection path runs its rows through before delivery.
    ///
    /// Non-market rows pass unchanged (`market: None`). A market row passes
    /// only when its `market_status` is `active` and it is not soft-deleted,
    /// even when it was explicitly selected; with a conversation in `scope` it
    /// must also have an assistant-channel assignment for (instance, the
    /// conversation's project, `scope.assistant_id`, `scope.cli`) — whole-project
    /// and global assignments are delivered by the native files, never by AionUi
    /// (R7). Without a conversation (snapshot time) only the status is checked;
    /// the spawn path and the launcher check the assignment again.
    ///
    /// The default keeps every row as non-market: only the SQLite repository can
    /// see the market columns, and test doubles hold no market rows.
    async fn market_gate(
        &self,
        _user_id: &str,
        _scope: &McpMarketScope<'_>,
        rows: Vec<McpServerRow>,
    ) -> Result<Vec<GatedMcpRow>, DbError> {
        Ok(rows.into_iter().map(|row| GatedMcpRow { row, market: None }).collect())
    }
}

/// Where a set of MCP rows is about to be delivered (see
/// [`IMcpServerRepository::market_gate`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct McpMarketScope<'a> {
    /// The conversation being spawned; its `project_id` scopes the assignment
    /// check. `None` = snapshot time (status check only).
    pub conversation_id: Option<&'a str>,
    /// The conversation's assistant (`preset_assistant_id`).
    pub assistant_id: Option<&'a str>,
    /// The backend CLI label (`claude`, `codex`, `gemini`, ...).
    pub cli: Option<&'a str>,
}

/// A row that passed [`IMcpServerRepository::market_gate`].
#[derive(Debug, Clone)]
pub struct GatedMcpRow {
    pub row: McpServerRow,
    /// `Some` for a market row: how the launcher must be invoked for it.
    pub market: Option<MarketDelivery>,
}

/// Launcher scope of a market row that passed the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketDelivery {
    /// `None` at snapshot time (no conversation in scope).
    pub project_id: Option<String>,
    pub assistant_id: Option<String>,
    /// `read` or `write`.
    pub tier: String,
    /// `production`, `staging`, `sandbox` or `local` (`market_environment`).
    pub environment: String,
    /// JSON array (read tier allowlist), as stored.
    pub tools_allow: Option<String>,
    /// JSON array (blocklist), as stored.
    pub tools_block: Option<String>,
}

impl MarketDelivery {
    /// A write-tier instance on production data (TASK-97): every tool call must
    /// be confirmed by the owner, so each CLI is configured to ask first.
    pub fn requires_owner_approval(&self) -> bool {
        self.environment == "production" && self.tier == "write"
    }
}

/// Parameters for creating a new MCP server.
#[derive(Debug, Clone)]
pub struct CreateMcpServerParams<'a> {
    pub user_id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub enabled: bool,
    pub transport_type: &'a str,
    pub transport_config: &'a str,
    pub tools: Option<&'a str>,
    pub original_json: Option<&'a str>,
    pub builtin: bool,
}

/// Parameters for updating an existing MCP server.
///
/// All fields are optional; `None` means "keep the current value".
/// For nullable fields, `Some(None)` means "clear the value" and
/// `Some(Some(v))` means "set to v".
#[derive(Debug, Default)]
pub struct UpdateMcpServerParams<'a> {
    pub name: Option<&'a str>,
    pub description: Option<Option<&'a str>>,
    pub enabled: Option<bool>,
    pub transport_type: Option<&'a str>,
    pub transport_config: Option<&'a str>,
    pub tools: Option<Option<&'a str>>,
    pub original_json: Option<Option<&'a str>>,
    pub builtin: Option<bool>,
    pub deleted_at: Option<Option<aionui_common::TimestampMs>>,
}
