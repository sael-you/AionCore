use aionui_common::TimestampMs;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{VoiceAuditRow, VoiceSessionRow, VoiceSettingsRow};
use crate::repository::voice::{
    CreateVoiceSessionParams, IVoiceAuditRepository, IVoiceSessionRepository, IVoiceSettingsRepository,
    NewVoiceAuditEntry, VoiceUsageDelta,
};

/// SQLite-backed implementation of the three voice repositories.
///
/// One struct serves all voice tables; callers hold it behind the trait they need.
#[derive(Clone, Debug)]
pub struct SqliteVoiceRepository {
    pool: SqlitePool,
}

impl SqliteVoiceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IVoiceSettingsRepository for SqliteVoiceRepository {
    async fn get(&self, user_id: &str) -> Result<Option<VoiceSettingsRow>, DbError> {
        let row = sqlx::query_as::<_, VoiceSettingsRow>("SELECT * FROM voice_settings WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn upsert(&self, row: &VoiceSettingsRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO voice_settings \
                (user_id, enabled, engine, model, car_mode, voice, eagerness, daily_budget_micros, \
                 provider_id, extra, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET \
                enabled = excluded.enabled, engine = excluded.engine, model = excluded.model, \
                car_mode = excluded.car_mode, voice = excluded.voice, eagerness = excluded.eagerness, \
                daily_budget_micros = excluded.daily_budget_micros, provider_id = excluded.provider_id, \
                extra = excluded.extra, updated_at = excluded.updated_at",
        )
        .bind(&row.user_id)
        .bind(row.enabled)
        .bind(&row.engine)
        .bind(&row.model)
        .bind(row.car_mode)
        .bind(&row.voice)
        .bind(&row.eagerness)
        .bind(row.daily_budget_micros)
        .bind(&row.provider_id)
        .bind(&row.extra)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl IVoiceSessionRepository for SqliteVoiceRepository {
    async fn create_starting(&self, params: CreateVoiceSessionParams<'_>) -> Result<VoiceSessionRow, DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "INSERT INTO voice_sessions \
                (id, user_id, engine, model, state, resume_of, device, instructions_hash, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'starting', ?, ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(params.user_id)
        .bind(params.engine)
        .bind(params.model)
        .bind(params.resume_of)
        .bind(params.device)
        .bind(params.instructions_hash)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;
        if let Err(err) = result {
            let err = DbError::from(err);
            if err.is_unique_violation() {
                return Err(DbError::Conflict("user already has a live voice session".into()));
            }
            return Err(err);
        }
        IVoiceSessionRepository::get(self, params.user_id, params.id)
            .await?
            .ok_or_else(|| DbError::Init("voice session insert succeeded but row not found".into()))
    }

    async fn mark_live(&self, user_id: &str, id: &str, call_id: &str) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE voice_sessions SET state = 'live', call_id = ?, updated_at = ? \
             WHERE user_id = ? AND id = ? AND state = 'starting'",
        )
        .bind(call_id)
        .bind(aionui_common::now_ms())
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("starting voice session '{id}' not found")));
        }
        Ok(())
    }

    async fn mark_ready(&self, user_id: &str, id: &str, ready_at: TimestampMs) -> Result<(), DbError> {
        sqlx::query("UPDATE voice_sessions SET ready_at = ?, updated_at = ? WHERE user_id = ? AND id = ?")
            .bind(ready_at)
            .bind(aionui_common::now_ms())
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn add_usage(&self, user_id: &str, id: &str, delta: &VoiceUsageDelta) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE voice_sessions SET \
                input_tokens = input_tokens + ?, cached_input_tokens = cached_input_tokens + ?, \
                output_tokens = output_tokens + ?, responses = responses + ?, \
                unknown_responses = unknown_responses + ?, stt_seconds = stt_seconds + ?, \
                cost_micros = cost_micros + ?, updated_at = ? \
             WHERE user_id = ? AND id = ?",
        )
        .bind(delta.input_tokens)
        .bind(delta.cached_input_tokens)
        .bind(delta.output_tokens)
        .bind(delta.responses)
        .bind(delta.unknown_responses)
        .bind(delta.stt_seconds)
        .bind(delta.cost_micros)
        .bind(aionui_common::now_ms())
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn finish(&self, user_id: &str, id: &str, state: &str, reason: &str) -> Result<bool, DbError> {
        if state != "ended" && state != "failed" {
            return Err(DbError::Init(format!("invalid terminal voice session state '{state}'")));
        }
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE voice_sessions SET state = ?, end_reason = ?, ended_at = ?, updated_at = ? \
             WHERE user_id = ? AND id = ? AND state IN ('starting', 'live')",
        )
        .bind(state)
        .bind(reason)
        .bind(now)
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn get(&self, user_id: &str, id: &str) -> Result<Option<VoiceSessionRow>, DbError> {
        let row = sqlx::query_as::<_, VoiceSessionRow>("SELECT * FROM voice_sessions WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list(&self, user_id: &str, limit: u32) -> Result<Vec<VoiceSessionRow>, DbError> {
        let rows = sqlx::query_as::<_, VoiceSessionRow>(
            "SELECT * FROM voice_sessions WHERE user_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_open(&self) -> Result<Vec<VoiceSessionRow>, DbError> {
        let rows = sqlx::query_as::<_, VoiceSessionRow>(
            "SELECT * FROM voice_sessions WHERE state IN ('starting', 'live') ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn cost_since(&self, user_id: &str, since: TimestampMs) -> Result<i64, DbError> {
        let (total,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_micros), 0) FROM voice_sessions WHERE user_id = ? AND created_at >= ?",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(total)
    }
}

#[async_trait::async_trait]
impl IVoiceAuditRepository for SqliteVoiceRepository {
    async fn append(&self, entry: NewVoiceAuditEntry<'_>) -> Result<(), DbError> {
        sqlx::query("INSERT INTO voice_audit (user_id, session_id, event, detail, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(entry.user_id)
            .bind(entry.session_id)
            .bind(entry.event)
            .bind(entry.detail)
            .bind(aionui_common::now_ms())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list(&self, user_id: &str, limit: u32) -> Result<Vec<VoiceAuditRow>, DbError> {
        let rows =
            sqlx::query_as::<_, VoiceAuditRow>("SELECT * FROM voice_audit WHERE user_id = ? ORDER BY id DESC LIMIT ?")
                .bind(user_id)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }
}

#[cfg(test)]
#[path = "sqlite_voice_test.rs"]
mod sqlite_voice_test;
