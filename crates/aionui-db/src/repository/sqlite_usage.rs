use sqlx::SqlitePool;

use crate::error::DbError;
use crate::kit_resolve::{self, KitScope};
use crate::repository::sqlite_kit::load_layers;
use crate::repository::usage::{
    IUsageRepository, ProjectCounts, UsageConversationRow, UsageDayRow, UsageModelRow, UsageProjectRow, UsageTurnParams,
};

const SUMS: &str = "COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, \
     COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens, \
     COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens, \
     COALESCE(SUM(thought_tokens), 0) AS thought_tokens, SUM(cost_micro_usd) AS cost_micro_usd";

/// SQLite implementation of [`IUsageRepository`] (migration 913).
#[derive(Clone)]
pub struct SqliteUsageRepository {
    pool: SqlitePool,
}

impl SqliteUsageRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// `YYYY-MM-DD` (UTC) of an epoch-ms instant, computed by SQLite so the write
/// and every read agree on the day boundary.
const DAY_OF_MS: &str = "strftime('%Y-%m-%d', ? / 1000, 'unixepoch')";

/// Shared by [`IUsageRepository::record_usage_turn`] and
/// `IAcpSessionRepository::record_usage_turn` (the session pump only holds
/// the latter).
pub(crate) async fn record_usage_turn(pool: &SqlitePool, p: &UsageTurnParams<'_>) -> Result<bool, DbError> {
    if p.is_empty() {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    // Project, backend and model as they are now, in one read; no row = not
    // the user's conversation.
    let context: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT c.project_id, \
                (SELECT m.backend FROM acp_session a JOIN agent_metadata m ON m.id = a.agent_id \
                  WHERE a.conversation_id = c.id), \
                (SELECT CASE WHEN json_valid(a.session_config) \
                             THEN json_extract(a.session_config, '$.runtime.current_model_id') END \
                   FROM acp_session a WHERE a.conversation_id = c.id) \
           FROM conversations c WHERE c.id = ? AND c.user_id = ?",
    )
    .bind(p.conversation_id)
    .bind(p.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((project_id, backend, model)) = context else {
        return Ok(false);
    };
    let project_id = project_id.unwrap_or_default();
    let backend = backend.unwrap_or_default();
    let model = model.unwrap_or_default();
    sqlx::query(
        "INSERT INTO usage_turns (id, user_id, conversation_id, project_id, backend, model, started_at, \
         input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, thought_tokens, cost_micro_usd) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(aionui_common::generate_prefixed_id("usage"))
    .bind(p.user_id)
    .bind(p.conversation_id)
    .bind(&project_id)
    .bind(&backend)
    .bind(&model)
    .bind(p.at)
    .bind(p.input_tokens)
    .bind(p.output_tokens)
    .bind(p.cache_read_tokens)
    .bind(p.cache_write_tokens)
    .bind(p.thought_tokens)
    .bind(p.cost_micro_usd)
    .execute(&mut *tx)
    .await?;
    let sql = format!(
        "INSERT INTO usage_daily (user_id, project_id, day, backend, model, input_tokens, output_tokens, \
         cache_read_tokens, cache_write_tokens, thought_tokens, cost_micro_usd, turns, first_at) \
         VALUES (?, ?, {DAY_OF_MS}, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?) \
         ON CONFLICT (user_id, project_id, day, backend, model) DO UPDATE SET \
            input_tokens = input_tokens + excluded.input_tokens, \
            output_tokens = output_tokens + excluded.output_tokens, \
            cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens, \
            cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens, \
            thought_tokens = thought_tokens + excluded.thought_tokens, \
            cost_micro_usd = CASE WHEN cost_micro_usd IS NULL AND excluded.cost_micro_usd IS NULL THEN NULL \
                                  ELSE COALESCE(cost_micro_usd, 0) + COALESCE(excluded.cost_micro_usd, 0) END, \
            turns = turns + 1"
    );
    sqlx::query(&sql)
        .bind(p.user_id)
        .bind(&project_id)
        .bind(p.at)
        .bind(&backend)
        .bind(&model)
        .bind(p.input_tokens)
        .bind(p.output_tokens)
        .bind(p.cache_read_tokens)
        .bind(p.cache_write_tokens)
        .bind(p.thought_tokens)
        .bind(p.cost_micro_usd)
        .bind(p.at)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

#[async_trait::async_trait]
impl IUsageRepository for SqliteUsageRepository {
    async fn record_usage_turn(&self, params: &UsageTurnParams<'_>) -> Result<bool, DbError> {
        record_usage_turn(&self.pool, params).await
    }

    async fn usage_since(&self, user_id: &str, project: Option<&str>) -> Result<Option<i64>, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT MIN(first_at) FROM usage_daily WHERE user_id = ? AND (? IS NULL OR project_id = ?)",
        )
        .bind(user_id)
        .bind(project)
        .bind(project)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn usage_by_day(
        &self,
        user_id: &str,
        project: Option<&str>,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<UsageDayRow>, DbError> {
        let sql = format!(
            "SELECT day, {SUMS}, COALESCE(SUM(turns), 0) AS turns FROM usage_daily \
             WHERE user_id = ? AND (? IS NULL OR project_id = ?) AND day >= ? AND day <= ? \
             GROUP BY day ORDER BY day"
        );
        Ok(sqlx::query_as::<_, UsageDayRow>(&sql)
            .bind(user_id)
            .bind(project)
            .bind(project)
            .bind(from_day)
            .bind(to_day)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn usage_by_model(
        &self,
        user_id: &str,
        project: Option<&str>,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<UsageModelRow>, DbError> {
        let sql = format!(
            "SELECT model, backend, {SUMS}, COALESCE(SUM(turns), 0) AS turns FROM usage_daily \
             WHERE user_id = ? AND (? IS NULL OR project_id = ?) AND day >= ? AND day <= ? \
             GROUP BY model, backend ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC, model"
        );
        Ok(sqlx::query_as::<_, UsageModelRow>(&sql)
            .bind(user_id)
            .bind(project)
            .bind(project)
            .bind(from_day)
            .bind(to_day)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn usage_by_project(
        &self,
        user_id: &str,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<UsageProjectRow>, DbError> {
        let sql = format!(
            "SELECT d.project_id, \
                    COALESCE((SELECT p.name FROM projects p WHERE p.project_id = d.project_id AND p.user_id = d.user_id), '') AS name, \
                    {SUMS}, COALESCE(SUM(turns), 0) AS turns \
             FROM usage_daily d WHERE d.user_id = ? AND d.day >= ? AND d.day <= ? \
             GROUP BY d.project_id ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC, d.project_id"
        );
        Ok(sqlx::query_as::<_, UsageProjectRow>(&sql)
            .bind(user_id)
            .bind(from_day)
            .bind(to_day)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn usage_by_conversation(
        &self,
        user_id: &str,
        project_id: &str,
        from_ms: i64,
        limit: i64,
    ) -> Result<Vec<UsageConversationRow>, DbError> {
        let sql = format!(
            "SELECT t.conversation_id, \
                    COALESCE((SELECT c.name FROM conversations c WHERE c.id = t.conversation_id AND c.user_id = t.user_id), '') AS name, \
                    MAX(t.started_at) AS last_at, {SUMS}, COUNT(*) AS turns \
             FROM usage_turns t WHERE t.user_id = ? AND t.project_id = ? AND t.started_at >= ? \
             GROUP BY t.conversation_id \
             ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC, t.conversation_id LIMIT ?"
        );
        Ok(sqlx::query_as::<_, UsageConversationRow>(&sql)
            .bind(user_id)
            .bind(project_id)
            .bind(from_ms)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn project_counts(&self, user_id: &str, project_id: &str) -> Result<ProjectCounts, DbError> {
        let (total, active): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN archived_at IS NULL THEN 1 ELSE 0 END), 0) \
             FROM conversations WHERE user_id = ? AND project_id = ?",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_one(&self.pool)
        .await?;
        let open_cards: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM board_cards k WHERE k.user_id = ? AND k.project_id = ? \
               AND NOT EXISTS (SELECT 1 FROM board_columns col \
                                WHERE col.project_id = k.project_id AND col.key = k.column_key \
                                  AND col.category = 'done')",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_one(&self.pool)
        .await?;
        let scope = KitScope {
            project_id: Some(project_id.to_owned()),
            ..Default::default()
        };
        let layers = load_layers(&self.pool, user_id, &scope).await?;
        let resolution = kit_resolve::resolve(&scope, &layers);
        let included = |m: &std::collections::BTreeMap<String, kit_resolve::KitDecision>| {
            i64::try_from(m.values().filter(|d| d.included).count()).unwrap_or(i64::MAX)
        };
        Ok(ProjectCounts {
            conversations_active: active,
            conversations_total: total,
            open_cards,
            kit_skills: included(&resolution.skills),
            kit_mcps: included(&resolution.mcps),
            kit_rules: included(&resolution.rules),
            kit_servers: included(&resolution.servers),
        })
    }
}
