use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{ServerAuditRow, ServerRow};
use crate::repository::server_pool::{
    CreateServerParams, IServerPoolRepository, SERVER_NAME_TAKEN_MSG, ServerAuditParams, UpdateServerParams,
};

const SERVER_COLUMNS: &str = "s.id, s.user_id, s.name, s.host, s.port, s.ssh_user, s.host_key, s.labels, \
     s.environment, s.tier, s.notes, s.enabled, \
     EXISTS(SELECT 1 FROM server_secrets k WHERE k.server_id = s.id AND k.user_id = s.user_id) AS has_key, \
     s.created_at, s.updated_at, s.deleted_at";

/// SQLite implementation of [`IServerPoolRepository`] (migration 910).
#[derive(Clone)]
pub struct SqliteServerPoolRepository {
    pool: SqlitePool,
}

impl SqliteServerPoolRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn name_conflict(err: sqlx::Error) -> DbError {
    match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::Conflict(SERVER_NAME_TAKEN_MSG.to_owned()),
        _ => DbError::Query(err),
    }
}

#[async_trait::async_trait]
impl IServerPoolRepository for SqliteServerPoolRepository {
    async fn list_servers(&self, user_id: &str) -> Result<Vec<ServerRow>, DbError> {
        let sql = format!(
            "SELECT {SERVER_COLUMNS} FROM servers s WHERE s.user_id = ? AND s.deleted_at IS NULL ORDER BY s.name"
        );
        Ok(sqlx::query_as::<_, ServerRow>(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn find_server(&self, user_id: &str, id: &str) -> Result<Option<ServerRow>, DbError> {
        let sql =
            format!("SELECT {SERVER_COLUMNS} FROM servers s WHERE s.user_id = ? AND s.id = ? AND s.deleted_at IS NULL");
        Ok(sqlx::query_as::<_, ServerRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn find_server_by_name(&self, user_id: &str, name: &str) -> Result<Option<ServerRow>, DbError> {
        let sql = format!(
            "SELECT {SERVER_COLUMNS} FROM servers s WHERE s.user_id = ? AND s.name = ? AND s.deleted_at IS NULL"
        );
        Ok(sqlx::query_as::<_, ServerRow>(&sql)
            .bind(user_id)
            .bind(name)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn any_enabled_server(&self, user_id: &str) -> Result<bool, DbError> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM servers WHERE user_id = ? AND deleted_at IS NULL AND enabled = 1 LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    async fn create_server(&self, params: &CreateServerParams<'_>) -> Result<ServerRow, DbError> {
        let id = aionui_common::generate_prefixed_id("srv");
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO servers (id, user_id, name, host, port, ssh_user, host_key, labels, environment, tier, \
             notes, enabled, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(params.user_id)
        .bind(params.name)
        .bind(params.host)
        .bind(params.port)
        .bind(params.ssh_user)
        .bind(params.host_key)
        .bind(params.labels)
        .bind(params.environment)
        .bind(params.tier)
        .bind(params.notes)
        .bind(params.enabled)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(name_conflict)?;
        if let Some(key) = params.key_encrypted {
            sqlx::query(
                "INSERT INTO server_secrets (server_id, user_id, value_encrypted, updated_at) VALUES (?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(params.user_id)
            .bind(key)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.find_server(params.user_id, &id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("server {id}")))
    }

    async fn update_server(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateServerParams<'_>,
    ) -> Result<ServerRow, DbError> {
        let result = sqlx::query(
            "UPDATE servers SET \
                name = COALESCE(?, name), host = COALESCE(?, host), port = COALESCE(?, port), \
                ssh_user = COALESCE(?, ssh_user), host_key = COALESCE(?, host_key), labels = COALESCE(?, labels), \
                environment = COALESCE(?, environment), tier = COALESCE(?, tier), notes = COALESCE(?, notes), \
                enabled = COALESCE(?, enabled), updated_at = ? \
             WHERE user_id = ? AND id = ? AND deleted_at IS NULL",
        )
        .bind(params.name)
        .bind(params.host)
        .bind(params.port)
        .bind(params.ssh_user)
        .bind(params.host_key)
        .bind(params.labels)
        .bind(params.environment)
        .bind(params.tier)
        .bind(params.notes)
        .bind(params.enabled)
        .bind(aionui_common::now_ms())
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(name_conflict)?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("server {id}")));
        }
        self.find_server(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("server {id}")))
    }

    async fn delete_server(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE servers SET deleted_at = ?, updated_at = ?, enabled = 0 \
             WHERE user_id = ? AND id = ? AND deleted_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query("DELETE FROM server_secrets WHERE user_id = ? AND server_id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM kit_attachments WHERE user_id = ? AND item_kind = 'server' AND item_ref = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM kit_items WHERE user_id = ? AND kind = 'server' AND ref = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn set_server_key(&self, user_id: &str, id: &str, value_encrypted: &str) -> Result<(), DbError> {
        // One statement: the key is written only for a live server of the user.
        let result = sqlx::query(
            "INSERT INTO server_secrets (server_id, user_id, value_encrypted, updated_at) \
             SELECT s.id, s.user_id, ?, ? FROM servers s \
             WHERE s.user_id = ? AND s.id = ? AND s.deleted_at IS NULL \
             ON CONFLICT(server_id) DO UPDATE SET \
                value_encrypted = excluded.value_encrypted, updated_at = excluded.updated_at",
        )
        .bind(value_encrypted)
        .bind(aionui_common::now_ms())
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("server {id}")));
        }
        Ok(())
    }

    async fn server_key(&self, user_id: &str, id: &str) -> Result<Option<String>, DbError> {
        Ok(
            sqlx::query_scalar("SELECT value_encrypted FROM server_secrets WHERE user_id = ? AND server_id = ?")
                .bind(user_id)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn insert_server_audit(&self, params: &ServerAuditParams<'_>) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO server_audit (id, user_id, conversation_id, server_id, server_name, channel, decision, \
             reason, command, exit_code, duration_ms, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(aionui_common::generate_prefixed_id("srvaud"))
        .bind(params.user_id)
        .bind(params.conversation_id)
        .bind(params.server_id)
        .bind(params.server_name)
        .bind(params.channel)
        .bind(params.decision)
        .bind(params.reason)
        .bind(params.command)
        .bind(params.exit_code)
        .bind(params.duration_ms)
        .bind(aionui_common::now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_server_audit(
        &self,
        user_id: &str,
        server_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ServerAuditRow>, DbError> {
        Ok(sqlx::query_as::<_, ServerAuditRow>(
            "SELECT id, user_id, conversation_id, server_id, server_name, channel, decision, reason, command, \
             exit_code, duration_ms, created_at FROM server_audit \
             WHERE user_id = ? AND (? IS NULL OR server_id = ?) \
             ORDER BY created_at DESC, rowid DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(server_id)
        .bind(server_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}
