use std::collections::HashMap;

use sqlx::{QueryBuilder, Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::kit_resolve::{KIT_LEVELS, KitLayers, KitScope};
use crate::models::{KitAttachmentRow, KitItemRow, KitRow};
use crate::repository::kit::{
    AttachParams, ConversationSkills, CreateKitParams, GlobalKitStep, IKitRepository, KIT_NAME_TAKEN_MSG,
    KIT_PROJECT_HAS_KIT_MSG, KIT_STALE_MSG, KitProjectInfo, SeedKit, UpdateKitParams,
};

/// SQLite implementation of [`IKitRepository`].
#[derive(Clone, Debug)]
pub struct SqliteKitRepository {
    pool: SqlitePool,
}

impl SqliteKitRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const KIT_COLUMNS: &str = "id, user_id, name, description, builtin_key, project_id, created_at, updated_at, deleted_at";
const ATTACHMENT_COLUMNS: &str = "id, user_id, level, target_id, item_kind, item_ref, op, created_at, updated_at";

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

async fn insert_items(
    tx: &mut Transaction<'_, Sqlite>,
    kit_id: &str,
    user_id: &str,
    kind: &str,
    refs: &[&str],
) -> Result<(), DbError> {
    for item in refs {
        sqlx::query("INSERT OR IGNORE INTO kit_items (kit_id, user_id, kind, ref) VALUES (?, ?, ?, ?)")
            .bind(kit_id)
            .bind(user_id)
            .bind(kind)
            .bind(item)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn replace_items(
    tx: &mut Transaction<'_, Sqlite>,
    kit_id: &str,
    user_id: &str,
    kind: &str,
    refs: Option<&[String]>,
) -> Result<(), DbError> {
    let Some(refs) = refs else {
        return Ok(());
    };
    sqlx::query("DELETE FROM kit_items WHERE kit_id = ? AND user_id = ? AND kind = ?")
        .bind(kit_id)
        .bind(user_id)
        .bind(kind)
        .execute(&mut **tx)
        .await?;
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    insert_items(tx, kit_id, user_id, kind, &refs).await
}

/// Shared by every reader of a scope (this repository, the MCP market gate
/// and the launcher check).
pub(crate) async fn load_layers(pool: &SqlitePool, user_id: &str, scope: &KitScope) -> Result<KitLayers, DbError> {
    let mut query = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {ATTACHMENT_COLUMNS} FROM kit_attachments WHERE user_id = "
    ));
    query.push_bind(user_id);
    query.push(" AND (");
    let mut first = true;
    for level in KIT_LEVELS {
        let Some(target) = scope.target(level) else {
            continue;
        };
        if !first {
            query.push(" OR ");
        }
        first = false;
        query.push("(level = ");
        query.push_bind(level);
        query.push(" AND target_id = ");
        query.push_bind(target);
        query.push(")");
    }
    query.push(")");
    let mut attachments: Vec<KitAttachmentRow> = query.build_query_as().fetch_all(pool).await?;

    // C28: the project kit is an implicit `project`/`add` attachment of its
    // own project (no row), unless an explicit project-level row names it.
    if let Some(project_id) = scope.target("project") {
        let project_kit: Option<String> = sqlx::query_scalar(
            "SELECT id FROM kits WHERE user_id = ? AND project_id = ? AND deleted_at IS NULL LIMIT 1",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_optional(pool)
        .await?;
        if let Some(kit_id) = project_kit {
            let explicit = attachments
                .iter()
                .any(|a| a.level == "project" && a.item_kind == "kit" && a.item_ref == kit_id);
            if !explicit {
                attachments.push(KitAttachmentRow {
                    id: format!("implicit:{kit_id}"),
                    user_id: user_id.to_owned(),
                    level: "project".to_owned(),
                    target_id: project_id.to_owned(),
                    item_kind: "kit".to_owned(),
                    item_ref: kit_id,
                    op: "add".to_owned(),
                    created_at: 0,
                    updated_at: 0,
                });
            }
        }
    }

    let kit_ids: Vec<&str> = attachments
        .iter()
        .filter(|a| a.item_kind == "kit")
        .map(|a| a.item_ref.as_str())
        .collect();
    let mut kits = HashMap::new();
    let mut items = Vec::new();
    if !kit_ids.is_empty() {
        let mut q = QueryBuilder::<Sqlite>::new(format!(
            "SELECT {KIT_COLUMNS} FROM kits WHERE deleted_at IS NULL AND user_id = "
        ));
        q.push_bind(user_id);
        q.push(" AND id IN (");
        let mut sep = q.separated(", ");
        for id in &kit_ids {
            sep.push_bind(*id);
        }
        sep.push_unseparated(")");
        let rows: Vec<KitRow> = q.build_query_as().fetch_all(pool).await?;
        let mut q = QueryBuilder::<Sqlite>::new("SELECT kit_id, user_id, kind, ref FROM kit_items WHERE user_id = ");
        q.push_bind(user_id);
        q.push(" AND kit_id IN (");
        let mut sep = q.separated(", ");
        for row in &rows {
            sep.push_bind(&row.id);
        }
        sep.push_unseparated(")");
        if !rows.is_empty() {
            items = q.build_query_as().fetch_all(pool).await?;
        }
        kits = rows.into_iter().map(|k| (k.id.clone(), k)).collect();
    }
    Ok(KitLayers {
        attachments,
        kits,
        items,
    })
}

/// A conversation's kit scope (project + `extra.preset_assistant_id`).
pub(crate) async fn conversation_scope(
    pool: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
) -> Result<Option<KitScope>, DbError> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT project_id, CASE WHEN json_valid(extra) THEN json_extract(extra, '$.preset_assistant_id') END \
         FROM conversations WHERE user_id = ? AND id = ?",
    )
    .bind(user_id)
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(project_id, assistant_id)| KitScope {
        project_id: project_id.filter(|p| !p.is_empty()),
        assistant_id: assistant_id.filter(|a| !a.is_empty()),
        conversation_id: Some(conversation_id.to_owned()),
    }))
}

impl SqliteKitRepository {
    async fn find_kit_any(&self, user_id: &str, id: &str) -> Result<Option<KitRow>, DbError> {
        let sql = format!("SELECT {KIT_COLUMNS} FROM kits WHERE user_id = ? AND id = ?");
        Ok(sqlx::query_as::<_, KitRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }
}

#[async_trait::async_trait]
impl IKitRepository for SqliteKitRepository {
    async fn list_kits(&self, user_id: &str) -> Result<Vec<KitRow>, DbError> {
        let sql = format!(
            "SELECT {KIT_COLUMNS} FROM kits WHERE user_id = ? AND deleted_at IS NULL \
             ORDER BY builtin_key IS NULL, CAST(SUBSTR(builtin_key, 2) AS INTEGER), created_at, rowid"
        );
        Ok(sqlx::query_as::<_, KitRow>(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn find_kit(&self, user_id: &str, id: &str) -> Result<Option<KitRow>, DbError> {
        Ok(self.find_kit_any(user_id, id).await?.filter(|k| k.deleted_at.is_none()))
    }

    async fn find_project_kit(&self, user_id: &str, project_id: &str) -> Result<Option<KitRow>, DbError> {
        let sql = format!(
            "SELECT {KIT_COLUMNS} FROM kits WHERE user_id = ? AND project_id = ? AND deleted_at IS NULL LIMIT 1"
        );
        Ok(sqlx::query_as::<_, KitRow>(&sql)
            .bind(user_id)
            .bind(project_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list_kit_items(&self, user_id: &str, kit_ids: Option<&[String]>) -> Result<Vec<KitItemRow>, DbError> {
        let mut q = QueryBuilder::<Sqlite>::new(
            "SELECT i.kit_id, i.user_id, i.kind, i.ref FROM kit_items i JOIN kits k ON k.id = i.kit_id \
             WHERE k.deleted_at IS NULL AND i.user_id = ",
        );
        q.push_bind(user_id);
        if let Some(ids) = kit_ids {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            q.push(" AND i.kit_id IN (");
            let mut sep = q.separated(", ");
            for id in ids {
                sep.push_bind(id);
            }
            sep.push_unseparated(")");
        }
        q.push(" ORDER BY i.kit_id, i.kind, i.ref");
        Ok(q.build_query_as().fetch_all(&self.pool).await?)
    }

    async fn create_kit(&self, p: &CreateKitParams<'_>) -> Result<KitRow, DbError> {
        let id = aionui_common::generate_prefixed_id("kit");
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO kits (id, user_id, name, description, builtin_key, project_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, NULL, ?, ?, ?)",
        )
        .bind(&id)
        .bind(p.user_id)
        .bind(p.name)
        .bind(p.description)
        .bind(p.project_id)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            // Two partial UNIQUE indexes can refuse the insert: name or project.
            let project = matches!(&e, sqlx::Error::Database(db) if db.message().contains("project_id"));
            map_conflict(
                e,
                if project {
                    KIT_PROJECT_HAS_KIT_MSG
                } else {
                    KIT_NAME_TAKEN_MSG
                },
            )
        })?;
        let m = p.members;
        for (kind, refs) in [
            ("skill", m.skills),
            ("mcp", m.mcp_instance_ids),
            ("template", m.mcp_templates),
        ] {
            let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
            insert_items(&mut tx, &id, p.user_id, kind, &refs).await?;
        }
        tx.commit().await?;
        self.find_kit(p.user_id, &id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("kit {id}")))
    }

    async fn update_kit(&self, user_id: &str, id: &str, p: &UpdateKitParams<'_>) -> Result<KitRow, DbError> {
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        let affected = sqlx::query(
            "UPDATE kits SET name = COALESCE(?, name), description = COALESCE(?, description), \
             updated_at = MAX(?, updated_at + 1) \
             WHERE user_id = ? AND id = ? AND deleted_at IS NULL AND (? IS NULL OR updated_at = ?)",
        )
        .bind(p.name)
        .bind(p.description)
        .bind(now)
        .bind(user_id)
        .bind(id)
        .bind(p.expected_updated_at)
        .bind(p.expected_updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_conflict(e, KIT_NAME_TAKEN_MSG))?
        .rows_affected();
        if affected == 0 {
            drop(tx);
            return match self.find_kit(user_id, id).await? {
                Some(_) => Err(DbError::Conflict(KIT_STALE_MSG.into())),
                None => Err(DbError::NotFound(format!("kit {id}"))),
            };
        }
        replace_items(&mut tx, id, user_id, "skill", p.skills).await?;
        replace_items(&mut tx, id, user_id, "mcp", p.mcp_instance_ids).await?;
        replace_items(&mut tx, id, user_id, "template", p.mcp_templates).await?;
        tx.commit().await?;
        self.find_kit(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("kit {id}")))
    }

    async fn delete_kit(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        let affected = sqlx::query(
            "UPDATE kits SET deleted_at = ?, updated_at = ? WHERE user_id = ? AND id = ? AND deleted_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected == 0 {
            return Ok(false);
        }
        sqlx::query("DELETE FROM kit_items WHERE user_id = ? AND kit_id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM kit_attachments WHERE user_id = ? AND item_kind = 'kit' AND item_ref = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn seed_kits(&self, user_id: &str, seeds: &[SeedKit<'_>]) -> Result<usize, DbError> {
        let now = aionui_common::now_ms();
        let mut inserted = 0;
        let mut tx = self.pool.begin().await?;
        for seed in seeds {
            let id = aionui_common::generate_prefixed_id("kit");
            // OR IGNORE: the builtin key (also a soft-deleted one) or a live
            // owner kit with the same name wins; the seed never overwrites.
            let done = sqlx::query(
                "INSERT OR IGNORE INTO kits (id, user_id, name, description, builtin_key, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(user_id)
            .bind(seed.name)
            .bind(seed.description)
            .bind(seed.builtin_key)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if done == 0 {
                continue;
            }
            insert_items(&mut tx, &id, user_id, "skill", seed.skills).await?;
            insert_items(&mut tx, &id, user_id, "template", seed.mcp_templates).await?;
            if seed.attach_globally {
                sqlx::query(
                    "INSERT OR IGNORE INTO kit_attachments \
                     (id, user_id, level, target_id, item_kind, item_ref, op, created_at, updated_at) \
                     VALUES (?, ?, 'global', '', 'kit', ?, 'add', ?, ?)",
                )
                .bind(aionui_common::generate_prefixed_id("kita"))
                .bind(user_id)
                .bind(&id)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
            inserted += 1;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    async fn run_global_kit_step_once(
        &self,
        user_id: &str,
        marker_key: &str,
        off_global_keys: &[&str],
        project_keys: &[&str],
        project_names: &[&str],
    ) -> Result<Option<GlobalKitStep>, DbError> {
        let now = aionui_common::now_ms();
        let mut tx = self.pool.begin().await?;
        // Claim the marker first: a concurrent second run inserts nothing and stops.
        let claimed = sqlx::query(
            "INSERT OR IGNORE INTO client_preferences (user_id, key, value, updated_at) VALUES (?, ?, '1', ?)",
        )
        .bind(user_id)
        .bind(marker_key)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if claimed == 0 {
            return Ok(None);
        }
        let mut step = GlobalKitStep::default();
        for key in off_global_keys {
            step.removed_global += sqlx::query(
                "DELETE FROM kit_attachments WHERE user_id = ? AND level = 'global' AND item_kind = 'kit' \
                 AND item_ref IN (SELECT id FROM kits WHERE user_id = ? AND builtin_key = ?)",
            )
            .bind(user_id)
            .bind(user_id)
            .bind(*key)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        for name in project_names {
            let projects: Vec<String> =
                sqlx::query_scalar("SELECT project_id FROM projects WHERE user_id = ? AND name = ? ORDER BY id")
                    .bind(user_id)
                    .bind(*name)
                    .fetch_all(&mut *tx)
                    .await?;
            for project_id in &projects {
                for key in project_keys {
                    let kit: Option<String> = sqlx::query_scalar(
                        "SELECT id FROM kits WHERE user_id = ? AND builtin_key = ? AND deleted_at IS NULL",
                    )
                    .bind(user_id)
                    .bind(*key)
                    .fetch_optional(&mut *tx)
                    .await?;
                    let Some(kit_id) = kit else {
                        continue;
                    };
                    step.attached_project += sqlx::query(
                        "INSERT OR IGNORE INTO kit_attachments \
                         (id, user_id, level, target_id, item_kind, item_ref, op, created_at, updated_at) \
                         VALUES (?, ?, 'project', ?, 'kit', ?, 'add', ?, ?)",
                    )
                    .bind(aionui_common::generate_prefixed_id("kita"))
                    .bind(user_id)
                    .bind(project_id)
                    .bind(&kit_id)
                    .bind(now)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                }
            }
        }
        tx.commit().await?;
        Ok(Some(step))
    }

    async fn list_attachments(
        &self,
        user_id: &str,
        level: Option<&str>,
        target_id: Option<&str>,
    ) -> Result<Vec<KitAttachmentRow>, DbError> {
        let sql = format!(
            "SELECT {ATTACHMENT_COLUMNS} FROM kit_attachments WHERE user_id = ? \
             AND (? IS NULL OR level = ?) AND (? IS NULL OR target_id = ?) \
             ORDER BY CASE level WHEN 'global' THEN 0 WHEN 'project' THEN 1 WHEN 'assistant' THEN 2 ELSE 3 END, \
             target_id, created_at, rowid"
        );
        Ok(sqlx::query_as::<_, KitAttachmentRow>(&sql)
            .bind(user_id)
            .bind(level)
            .bind(level)
            .bind(target_id)
            .bind(target_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn find_attachment(&self, user_id: &str, id: &str) -> Result<Option<KitAttachmentRow>, DbError> {
        let sql = format!("SELECT {ATTACHMENT_COLUMNS} FROM kit_attachments WHERE user_id = ? AND id = ?");
        Ok(sqlx::query_as::<_, KitAttachmentRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn upsert_attachment(&self, p: &AttachParams<'_>) -> Result<KitAttachmentRow, DbError> {
        let now = aionui_common::now_ms();
        let id = aionui_common::generate_prefixed_id("kita");
        sqlx::query(
            "INSERT INTO kit_attachments (id, user_id, level, target_id, item_kind, item_ref, op, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (user_id, level, target_id, item_kind, item_ref) \
             DO UPDATE SET op = excluded.op, updated_at = excluded.updated_at",
        )
        .bind(&id)
        .bind(p.user_id)
        .bind(p.level)
        .bind(p.target_id)
        .bind(p.item_kind)
        .bind(p.item_ref)
        .bind(p.op)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        let sql = format!(
            "SELECT {ATTACHMENT_COLUMNS} FROM kit_attachments WHERE user_id = ? AND level = ? AND target_id = ? \
             AND item_kind = ? AND item_ref = ?"
        );
        sqlx::query_as::<_, KitAttachmentRow>(&sql)
            .bind(p.user_id)
            .bind(p.level)
            .bind(p.target_id)
            .bind(p.item_kind)
            .bind(p.item_ref)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DbError::NotFound("kit attachment".into()))
    }

    async fn delete_attachment(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        let affected = sqlx::query("DELETE FROM kit_attachments WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    async fn load_layers(&self, user_id: &str, scope: &KitScope) -> Result<KitLayers, DbError> {
        load_layers(&self.pool, user_id, scope).await
    }

    async fn conversation_scope(&self, user_id: &str, conversation_id: &str) -> Result<Option<KitScope>, DbError> {
        conversation_scope(&self.pool, user_id, conversation_id).await
    }

    async fn project_info(&self, user_id: &str, project_id: &str) -> Result<Option<KitProjectInfo>, DbError> {
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT p.name, f.resource_uri FROM projects p \
             LEFT JOIN project_explorer pe ON pe.project_id = p.project_id AND pe.role = 'workspace' \
             LEFT JOIN folders f ON f.folder_id = pe.folder_id \
             WHERE p.user_id = ? AND p.project_id = ? LIMIT 1",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(name, workspace_uri)| KitProjectInfo {
            name,
            workspace_uri: workspace_uri.filter(|u| !u.is_empty()),
        }))
    }

    async fn add_kit_items(&self, user_id: &str, kit_id: &str, kind: &str, refs: &[String]) -> Result<u64, DbError> {
        let mut tx = self.pool.begin().await?;
        let live: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM kits WHERE user_id = ? AND id = ? AND deleted_at IS NULL")
                .bind(user_id)
                .bind(kit_id)
                .fetch_optional(&mut *tx)
                .await?;
        if live.is_none() {
            return Err(DbError::NotFound(format!("kit {kit_id}")));
        }
        let mut added = 0;
        for item in refs {
            added += sqlx::query("INSERT OR IGNORE INTO kit_items (kit_id, user_id, kind, ref) VALUES (?, ?, ?, ?)")
                .bind(kit_id)
                .bind(user_id)
                .bind(kind)
                .bind(item)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        if added > 0 {
            sqlx::query("UPDATE kits SET updated_at = MAX(?, updated_at + 1) WHERE user_id = ? AND id = ?")
                .bind(aionui_common::now_ms())
                .bind(user_id)
                .bind(kit_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(added)
    }

    async fn conversation_workspace(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, DbError> {
        let row: Option<Option<String>> = sqlx::query_scalar(
            "SELECT CASE WHEN json_valid(extra) THEN json_extract(extra, '$.workspace') END \
             FROM conversations WHERE user_id = ? AND id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.flatten().filter(|w| !w.is_empty()))
    }

    async fn conversation_skills(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ConversationSkills>, DbError> {
        let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT CASE WHEN json_valid(extra) THEN json_extract(extra, '$.skills') END, \
             CASE WHEN json_valid(extra) THEN json_extract(extra, '$.kit_base_skills') END \
             FROM conversations WHERE user_id = ? AND id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        let list = |v: Option<String>| v.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok());
        Ok(row.map(|(skills, base)| ConversationSkills {
            skills: list(skills).unwrap_or_default(),
            kit_base_skills: list(base),
        }))
    }
}
