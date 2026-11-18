use crate::error::DbError;
use crate::models::{ServerAuditRow, ServerRow};

/// `DbError::Conflict` message of a server name already taken by a live server.
pub const SERVER_NAME_TAKEN_MSG: &str = "server name already in use";

/// Data access for server access pools (migration 910). Validation, key
/// encryption and the crucial-command rules live in `aionui-mcp`'s
/// `server_pool` module; assignment lives in `kit_attachments`
/// (`item_kind = 'server'`) and is resolved by [`crate::kit_resolve`].
#[async_trait::async_trait]
pub trait IServerPoolRepository: Send + Sync {
    /// Live servers of a user, by name.
    async fn list_servers(&self, user_id: &str) -> Result<Vec<ServerRow>, DbError>;

    /// A live server by id.
    async fn find_server(&self, user_id: &str, id: &str) -> Result<Option<ServerRow>, DbError>;

    /// A live server by name.
    async fn find_server_by_name(&self, user_id: &str, name: &str) -> Result<Option<ServerRow>, DbError>;

    /// Whether the user has at least one live, enabled server (decides whether
    /// a session gets the built-in server MCPs at all).
    async fn any_enabled_server(&self, user_id: &str) -> Result<bool, DbError>;

    /// Insert a server and, when given, its encrypted key in one transaction
    /// (`Conflict` when a live server of the user already has this name).
    async fn create_server(&self, params: &CreateServerParams<'_>) -> Result<ServerRow, DbError>;

    /// Column-level update of the given fields in one statement (`None` keeps
    /// the stored value). `NotFound` when there is no such live server,
    /// `Conflict` on a taken name.
    async fn update_server(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateServerParams<'_>,
    ) -> Result<ServerRow, DbError>;

    /// Soft-delete a server and delete its key and every attachment of it, in
    /// one transaction. Returns whether a live server was deleted.
    async fn delete_server(&self, user_id: &str, id: &str) -> Result<bool, DbError>;

    /// Set or replace the encrypted private key of a live server.
    async fn set_server_key(&self, user_id: &str, id: &str, value_encrypted: &str) -> Result<(), DbError>;

    /// The encrypted private key of a server, if one is stored.
    async fn server_key(&self, user_id: &str, id: &str) -> Result<Option<String>, DbError>;

    /// Append one audit row.
    async fn insert_server_audit(&self, params: &ServerAuditParams<'_>) -> Result<(), DbError>;

    /// Newest audit rows first, optionally for one server.
    async fn list_server_audit(
        &self,
        user_id: &str,
        server_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ServerAuditRow>, DbError>;
}

#[derive(Debug, Clone)]
pub struct CreateServerParams<'a> {
    pub user_id: &'a str,
    pub name: &'a str,
    pub host: &'a str,
    pub port: i64,
    pub ssh_user: &'a str,
    pub host_key: &'a str,
    /// JSON array of strings.
    pub labels: &'a str,
    pub environment: &'a str,
    pub tier: &'a str,
    pub notes: &'a str,
    pub enabled: bool,
    pub key_encrypted: Option<&'a str>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateServerParams<'a> {
    pub name: Option<&'a str>,
    pub host: Option<&'a str>,
    pub port: Option<i64>,
    pub ssh_user: Option<&'a str>,
    pub host_key: Option<&'a str>,
    pub labels: Option<&'a str>,
    pub environment: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ServerAuditParams<'a> {
    pub user_id: &'a str,
    pub conversation_id: &'a str,
    pub server_id: &'a str,
    pub server_name: &'a str,
    pub channel: &'a str,
    pub decision: &'a str,
    pub reason: &'a str,
    pub command: &'a str,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
}
