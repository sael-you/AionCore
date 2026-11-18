use sqlx::SqlitePool;

use crate::error::DbError;
use crate::kit_resolve::{self, KIT_LEVELS, KitScope};
use crate::models::{RuleRow, RuleSourceRow};
use crate::repository::rule::{
    CreateRuleParams, EffectiveRule, IRuleRepository, ImportRulesParams, RULE_NAME_TAKEN_MSG, RULE_SOURCE_EXISTS_MSG,
    UpdateRuleParams,
};
use crate::repository::sqlite_kit::{conversation_scope, load_layers};

const RULE_COLUMNS: &str =
    "id, user_id, name, description, body, enabled, origin, origin_ref, created_at, updated_at, deleted_at";
const SOURCE_COLUMNS: &str = "id, user_id, scope, project_id, path, original, status, imported_at, managed_at";

/// SQLite implementation of [`IRuleRepository`] (migration 912).
#[derive(Clone)]
pub struct SqliteRuleRepository {
    pool: SqlitePool,
}

impl SqliteRuleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn conflict(msg: &'static str) -> impl Fn(sqlx::Error) -> DbError {
    move |err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::Conflict(msg.to_owned()),
        _ => DbError::Query(err),
    }
}

#[async_trait::async_trait]
impl IRuleRepository for SqliteRuleRepository {
    async fn list_rules(&self, user_id: &str) -> Result<Vec<RuleRow>, DbError> {
        let sql = format!("SELECT {RULE_COLUMNS} FROM rules WHERE user_id = ? AND deleted_at IS NULL ORDER BY name");
        Ok(sqlx::query_as::<_, RuleRow>(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn find_rule(&self, user_id: &str, id: &str) -> Result<Option<RuleRow>, DbError> {
        let sql = format!("SELECT {RULE_COLUMNS} FROM rules WHERE user_id = ? AND id = ? AND deleted_at IS NULL");
        Ok(sqlx::query_as::<_, RuleRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn create_rule(&self, params: &CreateRuleParams<'_>) -> Result<RuleRow, DbError> {
        let id = aionui_common::generate_prefixed_id("rule");
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO rules (id, user_id, name, description, body, enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(params.user_id)
        .bind(params.name)
        .bind(params.description)
        .bind(params.body)
        .bind(params.enabled)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(conflict(RULE_NAME_TAKEN_MSG))?;
        self.find_rule(params.user_id, &id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("rule {id}")))
    }

    async fn update_rule(&self, user_id: &str, id: &str, params: &UpdateRuleParams<'_>) -> Result<RuleRow, DbError> {
        let result = sqlx::query(
            "UPDATE rules SET name = COALESCE(?, name), description = COALESCE(?, description), \
                body = COALESCE(?, body), enabled = COALESCE(?, enabled), updated_at = ? \
             WHERE user_id = ? AND id = ? AND deleted_at IS NULL AND (? IS NULL OR updated_at = ?)",
        )
        .bind(params.name)
        .bind(params.description)
        .bind(params.body)
        .bind(params.enabled)
        .bind(aionui_common::now_ms())
        .bind(user_id)
        .bind(id)
        .bind(params.expected_updated_at)
        .bind(params.expected_updated_at)
        .execute(&self.pool)
        .await
        .map_err(conflict(RULE_NAME_TAKEN_MSG))?;
        if result.rows_affected() == 0 {
            return match self.find_rule(user_id, id).await? {
                Some(_) => Err(DbError::Conflict("rule changed since it was read".to_owned())),
                None => Err(DbError::NotFound(format!("rule {id}"))),
            };
        }
        self.find_rule(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("rule {id}")))
    }

    async fn delete_rule(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE rules SET deleted_at = ?, updated_at = ? WHERE user_id = ? AND id = ? AND deleted_at IS NULL",
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
        sqlx::query("DELETE FROM kit_attachments WHERE user_id = ? AND item_kind = 'rule' AND item_ref = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM kit_items WHERE user_id = ? AND kind = 'rule' AND ref = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn effective_rules(&self, user_id: &str, scope: &KitScope) -> Result<Vec<EffectiveRule>, DbError> {
        let layers = load_layers(&self.pool, user_id, scope).await?;
        let resolution = kit_resolve::resolve(scope, &layers);
        let rules = self.list_rules(user_id).await?;
        let mut out: Vec<EffectiveRule> = rules
            .into_iter()
            .filter(|r| r.enabled)
            .filter_map(|rule| {
                let decision = resolution.rules.get(&rule.id).filter(|d| d.included)?;
                Some(EffectiveRule {
                    rule,
                    level: decision.level,
                })
            })
            .collect();
        let order = |level: &str| KIT_LEVELS.iter().position(|l| *l == level).unwrap_or(KIT_LEVELS.len());
        out.sort_by(|a, b| (order(a.level), a.rule.name.as_str()).cmp(&(order(b.level), b.rule.name.as_str())));
        Ok(out)
    }

    async fn conversation_rules(&self, user_id: &str, conversation_id: &str) -> Result<Vec<EffectiveRule>, DbError> {
        match conversation_scope(&self.pool, user_id, conversation_id).await? {
            Some(scope) => self.effective_rules(user_id, &scope).await,
            None => Ok(Vec::new()),
        }
    }

    async fn import_rules(&self, params: &ImportRulesParams<'_>) -> Result<(RuleSourceRow, Vec<RuleRow>), DbError> {
        let now = aionui_common::now_ms();
        let source_id = aionui_common::generate_prefixed_id("rulesrc");
        let (level, target) = if params.scope == "project" {
            ("project", params.project_id)
        } else {
            ("global", "")
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO rule_sources (id, user_id, scope, project_id, path, original, status, imported_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'imported', ?)",
        )
        .bind(&source_id)
        .bind(params.user_id)
        .bind(params.scope)
        .bind(params.project_id)
        .bind(params.path)
        .bind(params.original)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(conflict(RULE_SOURCE_EXISTS_MSG))?;
        let mut ids = Vec::with_capacity(params.rules.len());
        for rule in params.rules {
            let id = aionui_common::generate_prefixed_id("rule");
            sqlx::query(
                "INSERT INTO rules (id, user_id, name, description, body, enabled, origin, origin_ref, created_at, \
                 updated_at) VALUES (?, ?, ?, ?, ?, 1, 'import', ?, ?, ?)",
            )
            .bind(&id)
            .bind(params.user_id)
            .bind(rule.name)
            .bind(rule.description)
            .bind(rule.body)
            .bind(&source_id)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(conflict(RULE_NAME_TAKEN_MSG))?;
            sqlx::query(
                "INSERT INTO kit_attachments (id, user_id, level, target_id, item_kind, item_ref, op, created_at, \
                 updated_at) VALUES (?, ?, ?, ?, 'rule', ?, 'add', ?, ?)",
            )
            .bind(aionui_common::generate_prefixed_id("kitatt"))
            .bind(params.user_id)
            .bind(level)
            .bind(target)
            .bind(&id)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            ids.push(id);
        }
        tx.commit().await?;
        let mut rules = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(rule) = self.find_rule(params.user_id, &id).await? {
                rules.push(rule);
            }
        }
        let source = self
            .list_rule_sources(params.user_id)
            .await?
            .into_iter()
            .find(|s| s.id == source_id)
            .ok_or_else(|| DbError::NotFound(format!("rule source {source_id}")))?;
        Ok((source, rules))
    }

    async fn list_rule_sources(&self, user_id: &str) -> Result<Vec<RuleSourceRow>, DbError> {
        let sql = format!("SELECT {SOURCE_COLUMNS} FROM rule_sources WHERE user_id = ? ORDER BY imported_at, rowid");
        Ok(sqlx::query_as::<_, RuleSourceRow>(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn mark_rule_source_managed(&self, user_id: &str, id: &str) -> Result<RuleSourceRow, DbError> {
        let result =
            sqlx::query("UPDATE rule_sources SET status = 'managed', managed_at = ? WHERE user_id = ? AND id = ?")
                .bind(aionui_common::now_ms())
                .bind(user_id)
                .bind(id)
                .execute(&self.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("rule source {id}")));
        }
        self.list_rule_sources(user_id)
            .await?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| DbError::NotFound(format!("rule source {id}")))
    }
}
