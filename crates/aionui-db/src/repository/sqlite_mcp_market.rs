use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{McpAssignmentRow, McpMarketRow, McpSecretRow};
use crate::repository::mcp_market::{
    CreateAssignmentParams, IMcpMarketRepository, MarketInstanceParams, UpdateMarketInstanceParams,
};

/// SQLite implementation of [`IMcpMarketRepository`].
#[derive(Clone, Debug)]
pub struct SqliteMcpMarketRepository {
    pool: SqlitePool,
}

impl SqliteMcpMarketRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

pub(crate) const MARKET_COLUMNS: &str = "id, user_id, name, description, transport_type, tools, last_test_status, \
     last_connected, deleted_at, created_at, updated_at, market_status, market_template_id, market_template, \
     market_environment, market_tier, market_enforcement, market_clis, market_plain_env, market_secret_refs, \
     market_tools_allow, market_tools_block, market_approval_ref";

const SECRET_COLUMNS: &str = "id, user_id, instance_id, name, value_encrypted, created_at, updated_at";
const ASSIGNMENT_COLUMNS: &str = "id, user_id, mcp_server_id, project_id, assistant_id, cli, created_at, updated_at";

fn unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db) if db.code().is_some_and(|c| c == "2067" || c == "1555"))
}

fn map_conflict(err: sqlx::Error, what: &str) -> DbError {
    if unique_violation(&err) {
        DbError::Conflict(what.to_owned())
    } else {
        DbError::Query(err)
    }
}

impl SqliteMcpMarketRepository {
    async fn fetch_instance_any(&self, user_id: &str, id: &str) -> Result<Option<McpMarketRow>, DbError> {
        let sql = format!("SELECT {MARKET_COLUMNS} FROM mcp_servers WHERE user_id = ? AND id = ?");
        Ok(sqlx::query_as::<_, McpMarketRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }
}

#[async_trait::async_trait]
impl IMcpMarketRepository for SqliteMcpMarketRepository {
    async fn list_instances(&self, user_id: &str) -> Result<Vec<McpMarketRow>, DbError> {
        let sql = format!(
            "SELECT {MARKET_COLUMNS} FROM mcp_servers WHERE user_id = ? AND market_status IS NOT NULL \
             AND deleted_at IS NULL ORDER BY created_at ASC, rowid ASC"
        );
        Ok(sqlx::query_as::<_, McpMarketRow>(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn find_instance(&self, user_id: &str, id: &str) -> Result<Option<McpMarketRow>, DbError> {
        let sql = format!(
            "SELECT {MARKET_COLUMNS} FROM mcp_servers WHERE user_id = ? AND id = ? \
             AND market_status IS NOT NULL AND deleted_at IS NULL"
        );
        Ok(sqlx::query_as::<_, McpMarketRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn find_by_name_any(&self, user_id: &str, name: &str) -> Result<Option<McpMarketRow>, DbError> {
        let sql = format!("SELECT {MARKET_COLUMNS} FROM mcp_servers WHERE user_id = ? AND name = ?");
        Ok(sqlx::query_as::<_, McpMarketRow>(&sql)
            .bind(user_id)
            .bind(name)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn create_instance(&self, p: &MarketInstanceParams<'_>) -> Result<McpMarketRow, DbError> {
        let id = aionui_common::generate_prefixed_id("mcp");
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO mcp_servers (id, user_id, name, description, enabled, transport_type, transport_config, \
             tools, last_test_status, last_connected, original_json, builtin, deleted_at, created_at, updated_at, \
             market_status, market_template_id, market_template, market_environment, market_tier, \
             market_enforcement, market_clis, market_plain_env, market_secret_refs, market_tools_allow, \
             market_tools_block, market_approval_ref) \
             VALUES (?, ?, ?, ?, 0, ?, ?, NULL, 'disconnected', NULL, NULL, 0, NULL, ?, ?, \
             ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(p.user_id)
        .bind(p.name)
        .bind(p.description)
        .bind(p.transport_type)
        .bind(p.transport_config)
        .bind(now)
        .bind(now)
        .bind(p.status)
        .bind(p.template_id)
        .bind(p.template_json)
        .bind(p.environment)
        .bind(p.tier)
        .bind(p.enforcement)
        .bind(p.clis_json)
        .bind(p.plain_env_json)
        .bind(p.secret_refs_json)
        .bind(p.tools_allow_json)
        .bind(p.tools_block_json)
        .bind(p.approval_ref)
        .execute(&self.pool)
        .await
        .map_err(|e| map_conflict(e, &format!("MCP server name '{}' already exists", p.name)))?;
        self.fetch_instance_any(p.user_id, &id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("MCP server {id}")))
    }

    async fn revive_instance(
        &self,
        user_id: &str,
        id: &str,
        p: &MarketInstanceParams<'_>,
    ) -> Result<McpMarketRow, DbError> {
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE mcp_servers SET description = ?, enabled = 0, transport_type = ?, transport_config = ?, \
             tools = NULL, last_test_status = 'disconnected', last_connected = NULL, original_json = NULL, \
             builtin = 0, deleted_at = NULL, updated_at = ?, market_status = ?, market_template_id = ?, \
             market_template = ?, market_environment = ?, market_tier = ?, market_enforcement = ?, \
             market_clis = ?, market_plain_env = ?, market_secret_refs = ?, market_tools_allow = ?, \
             market_tools_block = ?, market_approval_ref = ? \
             WHERE user_id = ? AND id = ? AND deleted_at IS NOT NULL",
        )
        .bind(p.description)
        .bind(p.transport_type)
        .bind(p.transport_config)
        .bind(now)
        .bind(p.status)
        .bind(p.template_id)
        .bind(p.template_json)
        .bind(p.environment)
        .bind(p.tier)
        .bind(p.enforcement)
        .bind(p.clis_json)
        .bind(p.plain_env_json)
        .bind(p.secret_refs_json)
        .bind(p.tools_allow_json)
        .bind(p.tools_block_json)
        .bind(p.approval_ref)
        .bind(user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(DbError::NotFound(format!("soft-deleted MCP server {id}")));
        }
        sqlx::query("DELETE FROM mcp_assignments WHERE user_id = ? AND mcp_server_id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mcp_secrets WHERE user_id = ? AND instance_id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.fetch_instance_any(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("MCP server {id}")))
    }

    async fn update_instance(
        &self,
        user_id: &str,
        id: &str,
        p: &UpdateMarketInstanceParams<'_>,
    ) -> Result<McpMarketRow, DbError> {
        let current = self
            .find_instance(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("MCP market instance {id}")))?;
        let pick = |new: Option<&str>, old: &Option<String>| new.map(str::to_owned).or_else(|| old.clone());
        let pick_nullable =
            |new: Option<Option<&str>>, old: &Option<String>| new.map(|v| v.map(str::to_owned)).unwrap_or(old.clone());
        sqlx::query(
            "UPDATE mcp_servers SET description = ?, updated_at = ?, market_status = ?, market_environment = ?, \
             market_tier = ?, market_enforcement = ?, market_clis = ?, market_plain_env = ?, \
             market_secret_refs = ?, market_tools_allow = ?, market_tools_block = ?, market_approval_ref = ? \
             WHERE user_id = ? AND id = ? AND deleted_at IS NULL",
        )
        .bind(pick_nullable(p.description, &current.description))
        .bind(aionui_common::now_ms())
        .bind(pick(p.status, &current.market_status))
        .bind(pick(p.environment, &current.market_environment))
        .bind(pick(p.tier, &current.market_tier))
        .bind(pick(p.enforcement, &current.market_enforcement))
        .bind(pick(p.clis_json, &current.market_clis))
        .bind(pick(p.plain_env_json, &current.market_plain_env))
        .bind(pick(p.secret_refs_json, &current.market_secret_refs))
        .bind(pick_nullable(p.tools_allow_json, &current.market_tools_allow))
        .bind(pick(p.tools_block_json, &current.market_tools_block))
        .bind(pick_nullable(p.approval_ref, &current.market_approval_ref))
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.fetch_instance_any(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("MCP server {id}")))
    }

    async fn delete_instance(&self, user_id: &str, id: &str) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        let affected = sqlx::query(
            "UPDATE mcp_servers SET deleted_at = ?, enabled = 0, updated_at = ? \
             WHERE user_id = ? AND id = ? AND market_status IS NOT NULL AND deleted_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(DbError::NotFound(format!("MCP market instance {id}")));
        }
        sqlx::query("DELETE FROM mcp_assignments WHERE user_id = ? AND mcp_server_id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mcp_secrets WHERE user_id = ? AND instance_id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn list_secrets(&self, user_id: &str) -> Result<Vec<McpSecretRow>, DbError> {
        let sql = format!("SELECT {SECRET_COLUMNS} FROM mcp_secrets WHERE user_id = ? ORDER BY name ASC");
        Ok(sqlx::query_as::<_, McpSecretRow>(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn list_instance_secrets(&self, user_id: &str, instance_id: &str) -> Result<Vec<McpSecretRow>, DbError> {
        let sql =
            format!("SELECT {SECRET_COLUMNS} FROM mcp_secrets WHERE user_id = ? AND instance_id = ? ORDER BY name ASC");
        Ok(sqlx::query_as::<_, McpSecretRow>(&sql)
            .bind(user_id)
            .bind(instance_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn upsert_secret(
        &self,
        user_id: &str,
        instance_id: &str,
        name: &str,
        value_encrypted: &str,
    ) -> Result<McpSecretRow, DbError> {
        let now = aionui_common::now_ms();
        let existing: Option<(String, String)> =
            sqlx::query_as("SELECT id, instance_id FROM mcp_secrets WHERE user_id = ? AND name = ?")
                .bind(user_id)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        let id = match existing {
            Some((_, owner)) if owner != instance_id => {
                return Err(DbError::Conflict(format!(
                    "secret '{name}' belongs to another instance"
                )));
            }
            Some((id, _)) => {
                sqlx::query("UPDATE mcp_secrets SET value_encrypted = ?, updated_at = ? WHERE id = ?")
                    .bind(value_encrypted)
                    .bind(now)
                    .bind(&id)
                    .execute(&self.pool)
                    .await?;
                id
            }
            None => {
                let id = aionui_common::generate_prefixed_id("mcps");
                sqlx::query(
                    "INSERT INTO mcp_secrets (id, user_id, instance_id, name, value_encrypted, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(user_id)
                .bind(instance_id)
                .bind(name)
                .bind(value_encrypted)
                .bind(now)
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(|e| map_conflict(e, &format!("secret '{name}' already exists")))?;
                id
            }
        };
        let sql = format!("SELECT {SECRET_COLUMNS} FROM mcp_secrets WHERE id = ?");
        Ok(sqlx::query_as::<_, McpSecretRow>(&sql)
            .bind(&id)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn delete_secret(&self, user_id: &str, instance_id: &str, name: &str) -> Result<bool, DbError> {
        let affected = sqlx::query("DELETE FROM mcp_secrets WHERE user_id = ? AND instance_id = ? AND name = ?")
            .bind(user_id)
            .bind(instance_id)
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    async fn list_assignments(
        &self,
        user_id: &str,
        instance_id: Option<&str>,
    ) -> Result<Vec<McpAssignmentRow>, DbError> {
        let sql = format!(
            "SELECT {ASSIGNMENT_COLUMNS} FROM mcp_assignments WHERE user_id = ? \
             AND (? IS NULL OR mcp_server_id = ?) ORDER BY created_at ASC, rowid ASC"
        );
        Ok(sqlx::query_as::<_, McpAssignmentRow>(&sql)
            .bind(user_id)
            .bind(instance_id)
            .bind(instance_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn find_assignment(&self, user_id: &str, id: &str) -> Result<Option<McpAssignmentRow>, DbError> {
        let sql = format!("SELECT {ASSIGNMENT_COLUMNS} FROM mcp_assignments WHERE user_id = ? AND id = ?");
        Ok(sqlx::query_as::<_, McpAssignmentRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn create_assignment(&self, p: &CreateAssignmentParams<'_>) -> Result<McpAssignmentRow, DbError> {
        let id = aionui_common::generate_prefixed_id("mcpa");
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO mcp_assignments (id, user_id, mcp_server_id, project_id, assistant_id, cli, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(p.user_id)
        .bind(p.instance_id)
        .bind(p.project_id)
        .bind(p.assistant_id)
        .bind(p.cli)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| map_conflict(e, "assignment already exists"))?;
        self.find_assignment(p.user_id, &id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("assignment {id}")))
    }

    async fn delete_assignment(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        let affected = sqlx::query("DELETE FROM mcp_assignments WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    async fn delete_instance_assignments(
        &self,
        user_id: &str,
        instance_id: &str,
    ) -> Result<Vec<McpAssignmentRow>, DbError> {
        let rows = self.list_assignments(user_id, Some(instance_id)).await?;
        sqlx::query("DELETE FROM mcp_assignments WHERE user_id = ? AND mcp_server_id = ?")
            .bind(user_id)
            .bind(instance_id)
            .execute(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn has_assignment(
        &self,
        user_id: &str,
        instance_id: &str,
        project_id: Option<&str>,
        assistant_id: Option<&str>,
        cli: &str,
    ) -> Result<bool, DbError> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM mcp_assignments WHERE user_id = ? AND mcp_server_id = ? AND cli = ? \
             AND project_id IS ? AND assistant_id IS ? LIMIT 1",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(cli)
        .bind(project_id)
        .bind(assistant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    async fn conversation_project_id(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, DbError> {
        let row: Option<Option<String>> =
            sqlx::query_scalar("SELECT project_id FROM conversations WHERE user_id = ? AND id = ?")
                .bind(user_id)
                .bind(conversation_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.flatten().filter(|p| !p.is_empty()))
    }

    async fn project_exists(&self, project_id: &str) -> Result<bool, DbError> {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM projects WHERE project_id = ? LIMIT 1")
            .bind(project_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
    }
}
