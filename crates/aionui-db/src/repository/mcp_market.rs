use crate::error::DbError;
use crate::models::{McpAssignmentRow, McpMarketRow, McpSecretRow};

/// Data access for the MCP market (migrations 904/905): market instances
/// (market columns of `mcp_servers`), their encrypted secrets and their
/// assignments. Business rules live in `aionui-mcp`'s market service; this
/// layer only stores opaque strings.
#[async_trait::async_trait]
pub trait IMcpMarketRepository: Send + Sync {
    /// Market instances of a user (soft-deleted rows excluded), oldest first.
    async fn list_instances(&self, user_id: &str) -> Result<Vec<McpMarketRow>, DbError>;

    /// A market instance by row id (soft-deleted rows excluded).
    async fn find_instance(&self, user_id: &str, id: &str) -> Result<Option<McpMarketRow>, DbError>;

    /// Any `mcp_servers` row with this name, market or not, soft-deleted or not.
    async fn find_by_name_any(&self, user_id: &str, name: &str) -> Result<Option<McpMarketRow>, DbError>;

    /// Insert a new market instance row (`enabled = 0`, launcher placeholder transport).
    async fn create_instance(&self, params: &MarketInstanceParams<'_>) -> Result<McpMarketRow, DbError>;

    /// Reuse the soft-deleted row `id` for a new market instance (R1 #10):
    /// every market column is overwritten, `deleted_at` cleared, `enabled = 0`,
    /// test results cleared, and the row's assignments and secrets deleted, in
    /// one transaction.
    async fn revive_instance(
        &self,
        user_id: &str,
        id: &str,
        params: &MarketInstanceParams<'_>,
    ) -> Result<McpMarketRow, DbError>;

    /// Partial update of a market instance: only the columns given are written
    /// (`None` keeps the stored value), in one statement. With
    /// `expected_updated_at` the write happens only if the row is unchanged
    /// since it was read (`DbError::Conflict` otherwise, TASK-71).
    async fn update_instance(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateMarketInstanceParams<'_>,
    ) -> Result<McpMarketRow, DbError>;

    /// Soft-delete a market instance and delete its assignments and secrets.
    async fn delete_instance(&self, user_id: &str, id: &str) -> Result<(), DbError>;

    /// All secrets of a user (ciphertext included; callers map to status only).
    async fn list_secrets(&self, user_id: &str) -> Result<Vec<McpSecretRow>, DbError>;

    /// Secrets owned by one instance.
    async fn list_instance_secrets(&self, user_id: &str, instance_id: &str) -> Result<Vec<McpSecretRow>, DbError>;

    /// Insert or replace the secret `name` owned by `instance_id`. Returns
    /// `DbError::Conflict` when the name is owned by another instance.
    async fn upsert_secret(
        &self,
        user_id: &str,
        instance_id: &str,
        name: &str,
        value_encrypted: &str,
    ) -> Result<McpSecretRow, DbError>;

    /// Delete the secret `name` of `instance_id`. Returns whether a row was deleted.
    async fn delete_secret(&self, user_id: &str, instance_id: &str, name: &str) -> Result<bool, DbError>;

    /// Assignments of a user, optionally only those of one instance.
    async fn list_assignments(
        &self,
        user_id: &str,
        instance_id: Option<&str>,
    ) -> Result<Vec<McpAssignmentRow>, DbError>;

    async fn find_assignment(&self, user_id: &str, id: &str) -> Result<Option<McpAssignmentRow>, DbError>;

    /// Insert one assignment. Returns `DbError::Conflict` on a duplicate.
    async fn create_assignment(&self, params: &CreateAssignmentParams<'_>) -> Result<McpAssignmentRow, DbError>;

    /// Insert several assignments in one transaction: all or none
    /// (`DbError::Conflict` on a duplicate, nothing inserted, TASK-73).
    async fn create_assignments(&self, params: &[CreateAssignmentParams<'_>])
    -> Result<Vec<McpAssignmentRow>, DbError>;

    async fn delete_assignment(&self, user_id: &str, id: &str) -> Result<bool, DbError>;

    /// Delete every assignment of an instance; returns the removed rows.
    async fn delete_instance_assignments(
        &self,
        user_id: &str,
        instance_id: &str,
    ) -> Result<Vec<McpAssignmentRow>, DbError>;

    /// Whether exactly this (instance, project, assistant, cli) assignment exists.
    /// `project_id = None` means global, `assistant_id = None` the whole project.
    async fn has_assignment(
        &self,
        user_id: &str,
        instance_id: &str,
        project_id: Option<&str>,
        assistant_id: Option<&str>,
        cli: &str,
    ) -> Result<bool, DbError>;

    /// `conversations.project_id` of a conversation, if bound.
    async fn conversation_project_id(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, DbError>;

    /// Whether `projects.project_id = project_id` exists.
    async fn project_exists(&self, project_id: &str) -> Result<bool, DbError>;
}

/// Every market column of an instance, for create and revive.
#[derive(Debug, Clone)]
pub struct MarketInstanceParams<'a> {
    pub user_id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    /// `stdio` or `http` (the template's transport).
    pub transport_type: &'a str,
    /// Launcher placeholder JSON stored in `transport_config`.
    pub transport_config: &'a str,
    pub status: &'a str,
    pub template_id: &'a str,
    pub template_json: &'a str,
    pub environment: &'a str,
    pub tier: &'a str,
    pub enforcement: &'a str,
    pub clis_json: &'a str,
    pub plain_env_json: &'a str,
    pub secret_refs_json: &'a str,
    pub tools_allow_json: Option<&'a str>,
    pub tools_block_json: &'a str,
    pub approval_ref: Option<&'a str>,
}

/// Partial update of a market instance. `None` keeps the stored value;
/// `approval_ref = Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct UpdateMarketInstanceParams<'a> {
    pub description: Option<Option<&'a str>>,
    /// New template snapshot (template refresh, R10).
    pub template_json: Option<&'a str>,
    pub status: Option<&'a str>,
    pub environment: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub enforcement: Option<&'a str>,
    pub clis_json: Option<&'a str>,
    pub plain_env_json: Option<&'a str>,
    pub secret_refs_json: Option<&'a str>,
    pub tools_allow_json: Option<Option<&'a str>>,
    pub tools_block_json: Option<&'a str>,
    pub approval_ref: Option<Option<&'a str>>,
    /// Write only if the row's `updated_at` still equals this value (the
    /// service read-decide-write paths); `DbError::Conflict` otherwise.
    /// `None` = unconditional column-level write (revoke/dormant).
    pub expected_updated_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CreateAssignmentParams<'a> {
    pub user_id: &'a str,
    pub instance_id: &'a str,
    pub project_id: Option<&'a str>,
    pub assistant_id: Option<&'a str>,
    pub cli: &'a str,
}
