//! SQLite implementation of [`IBoardStore`] (reads, delete, transaction
//! factory). The write transaction lives in `sqlite_board_tx.rs`; the query
//! helpers here are shared by both so a read inside a transaction is the
//! same SQL as the read outside one.

use sqlx::{Executor, QueryBuilder, Sqlite, SqlitePool};

use crate::error::DbError;
use crate::models::{
    BoardCardRow, BoardColumnRow, BoardDepRow, BoardEventRow, BoardIdempotencyRow, BoardLinkViewRow, BoardQuestionRow,
    BoardReviewRow, BoardSettingsRow, BoardSupervisorCursorRow,
};
use crate::repository::board::{
    BoardCallerRow, BoardCardCursor, BoardCardFilter, BoardConversationInfo, BoardEventQuery, BoardEventScope,
    BoardProjectRow, IBoardStore, IBoardTx,
};
use crate::repository::sqlite_board_tx::SqliteBoardTx;

pub(crate) const SETTINGS_COLS: &str =
    "project_id, user_id, enabled, template, next_number, version, settings, archived_at, keeper_conversation_id, \
     created_at, updated_at";
pub(crate) const COLUMN_COLS: &str = "id, project_id, user_id, key, name, category, kind, resolution, agent_move, \
     wip_limit, exit_rule, gate, position, archived";
pub(crate) const CARD_COLS: &str = "id, project_id, user_id, number, title, description, type, priority, size, tier, \
     column_key, position, labels, skills, routing, creator_kind, creator_ref, assignee_kind, assignee_ref, blocked, \
     blocked_reason, change_id, metadata, version, archived_at, created_at, updated_at";
pub(crate) const EVENT_COLS: &str =
    "seq, id, card_id, project_id, user_id, actor_kind, actor_ref, conversation_id, kind, payload, created_at";
pub(crate) const REVIEW_COLS: &str = "id, card_id, project_id, user_id, department, reviewer_ref, conversation_id, \
     verdict, summary, findings, recommendation, assumptions, confidence, tokens_used, version, created_at, updated_at";
pub(crate) const QUESTION_COLS: &str = "id, card_id, project_id, user_id, from_department, to_department, text, \
     answered_by_event, version, created_at, updated_at";
pub(crate) const DEP_COLS: &str = "card_id, depends_on_card_id, project_id, user_id, created_at";
pub(crate) const IDEMPOTENCY_COLS: &str = "user_id, project_id, key, request_hash, state, assignment_id, response, \
     claim_token, packet_message_id, created_at, updated_at";
/// The 901 index expression (lookups must use it verbatim to hit the index).
pub(crate) const ASSIGNMENT_EXPR: &str =
    "json_extract(extra, '$.board_assignment_id')";

pub(crate) async fn fetch_idempotency<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    project_id: &str,
    key: &str,
) -> Result<Option<BoardIdempotencyRow>, DbError> {
    Ok(sqlx::query_as::<_, BoardIdempotencyRow>(&format!(
        "SELECT {IDEMPOTENCY_COLS} FROM board_idempotency WHERE user_id = ? AND project_id = ? AND key = ?"
    ))
    .bind(user_id)
    .bind(project_id)
    .bind(key)
    .fetch_optional(executor)
    .await?)
}

pub(crate) async fn fetch_last_seq<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    project_id: &str,
) -> Result<i64, DbError> {
    Ok(
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM board_events WHERE user_id = ? AND project_id = ?")
            .bind(user_id)
            .bind(project_id)
            .fetch_one(executor)
            .await?,
    )
}

/// Every board table, child tables first; used by project deletion.
const BOARD_TABLES: [&str; 10] = [
    "board_events",
    "board_card_links",
    "board_card_questions",
    "board_card_reviews",
    "board_card_deps",
    "board_idempotency",
    "board_deploy_jobs",
    "board_supervisor_cursor",
    "board_columns",
    "board_settings",
];

/// SQLite-backed [`IBoardStore`].
#[derive(Clone, Debug)]
pub struct SqliteBoardStore {
    pool: SqlitePool,
}

impl SqliteBoardStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

pub(crate) async fn fetch_settings<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    project_id: &str,
) -> Result<Option<BoardSettingsRow>, DbError> {
    Ok(sqlx::query_as::<_, BoardSettingsRow>(&format!(
        "SELECT {SETTINGS_COLS} FROM board_settings WHERE user_id = ? AND project_id = ?"
    ))
    .bind(user_id)
    .bind(project_id)
    .fetch_optional(executor)
    .await?)
}

pub(crate) async fn fetch_columns<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    project_id: &str,
) -> Result<Vec<BoardColumnRow>, DbError> {
    Ok(sqlx::query_as::<_, BoardColumnRow>(&format!(
        "SELECT {COLUMN_COLS} FROM board_columns WHERE user_id = ? AND project_id = ? ORDER BY position ASC, key ASC"
    ))
    .bind(user_id)
    .bind(project_id)
    .fetch_all(executor)
    .await?)
}

pub(crate) async fn fetch_card<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    card_id: &str,
) -> Result<Option<BoardCardRow>, DbError> {
    Ok(sqlx::query_as::<_, BoardCardRow>(&format!(
        "SELECT {CARD_COLS} FROM board_cards WHERE user_id = ? AND id = ?"
    ))
    .bind(user_id)
    .bind(card_id)
    .fetch_optional(executor)
    .await?)
}

pub(crate) async fn fetch_cards_by_ids<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    ids: &[String],
) -> Result<Vec<BoardCardRow>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new(format!("SELECT {CARD_COLS} FROM board_cards WHERE user_id = "));
    builder.push_bind(user_id.to_owned()).push(" AND id IN (");
    let mut separated = builder.separated(", ");
    for id in ids {
        separated.push_bind(id.clone());
    }
    builder.push(") ORDER BY number ASC");
    Ok(builder.build_query_as::<BoardCardRow>().fetch_all(executor).await?)
}

pub(crate) async fn fetch_count_cards<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    project_id: &str,
    include_archived: bool,
) -> Result<i64, DbError> {
    let sql = if include_archived {
        "SELECT COUNT(*) FROM board_cards WHERE user_id = ? AND project_id = ?"
    } else {
        "SELECT COUNT(*) FROM board_cards WHERE user_id = ? AND project_id = ? AND archived_at IS NULL"
    };
    Ok(sqlx::query_scalar(sql)
        .bind(user_id)
        .bind(project_id)
        .fetch_one(executor)
        .await?)
}

pub(crate) async fn fetch_dependencies<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    card_id: &str,
) -> Result<Vec<BoardDepRow>, DbError> {
    Ok(sqlx::query_as::<_, BoardDepRow>(&format!(
        "SELECT {DEP_COLS} FROM board_card_deps WHERE user_id = ? AND card_id = ? \
         ORDER BY created_at ASC, depends_on_card_id ASC"
    ))
    .bind(user_id)
    .bind(card_id)
    .fetch_all(executor)
    .await?)
}

pub(crate) async fn fetch_project_dependencies<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    project_id: &str,
) -> Result<Vec<BoardDepRow>, DbError> {
    Ok(sqlx::query_as::<_, BoardDepRow>(&format!(
        "SELECT {DEP_COLS} FROM board_card_deps WHERE user_id = ? AND project_id = ? \
         ORDER BY card_id ASC, depends_on_card_id ASC"
    ))
    .bind(user_id)
    .bind(project_id)
    .fetch_all(executor)
    .await?)
}

pub(crate) async fn fetch_dependents<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    user_id: &str,
    card_id: &str,
) -> Result<Vec<BoardDepRow>, DbError> {
    Ok(sqlx::query_as::<_, BoardDepRow>(&format!(
        "SELECT {DEP_COLS} FROM board_card_deps WHERE user_id = ? AND depends_on_card_id = ? \
         ORDER BY created_at ASC, card_id ASC"
    ))
    .bind(user_id)
    .bind(card_id)
    .fetch_all(executor)
    .await?)
}

#[async_trait::async_trait]
impl IBoardStore for SqliteBoardStore {
    async fn project(&self, user_id: &str, project_id: &str) -> Result<Option<BoardProjectRow>, DbError> {
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT project_id, name, kind FROM projects WHERE user_id = ? AND project_id = ?")
                .bind(user_id)
                .bind(project_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(project_id, name, kind)| BoardProjectRow { project_id, name, kind }))
    }

    async fn caller(&self, user_id: &str, conversation_id: &str) -> Result<Option<BoardCallerRow>, DbError> {
        let row: Option<(String, Option<String>, String, String, Option<String>)> = sqlx::query_as(
            "SELECT c.id, c.project_id, c.extra, c.type, s.assistant_id \
             FROM conversations c \
             LEFT JOIN conversation_assistant_snapshots s ON s.conversation_id = c.id \
             WHERE c.user_id = ? AND c.id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(conversation_id, project_id, extra, conversation_type, assistant_id)| BoardCallerRow {
                conversation_id,
                project_id,
                extra,
                conversation_type,
                assistant_id,
            },
        ))
    }

    async fn team_project_id(&self, user_id: &str, team_id: &str) -> Result<Option<String>, DbError> {
        let row: Option<(Option<String>,)> = sqlx::query_as("SELECT project_id FROM teams WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(team_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|(project_id,)| project_id).filter(|id| !id.is_empty()))
    }

    async fn project_has_active_binding(&self, user_id: &str, project_id: &str) -> Result<bool, DbError> {
        let active: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE user_id = ? AND project_id = ? AND archived_at IS NULL) \
                 OR EXISTS(SELECT 1 FROM teams WHERE user_id = ? AND project_id = ? AND archived_at IS NULL)",
        )
        .bind(user_id)
        .bind(project_id)
        .bind(user_id)
        .bind(project_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(active != 0)
    }

    async fn settings(&self, user_id: &str, project_id: &str) -> Result<Option<BoardSettingsRow>, DbError> {
        fetch_settings(&self.pool, user_id, project_id).await
    }

    async fn columns(&self, user_id: &str, project_id: &str) -> Result<Vec<BoardColumnRow>, DbError> {
        fetch_columns(&self.pool, user_id, project_id).await
    }

    async fn cards(
        &self,
        user_id: &str,
        project_id: &str,
        include_archived: bool,
    ) -> Result<Vec<BoardCardRow>, DbError> {
        let archived = if include_archived { "" } else { " AND archived_at IS NULL" };
        Ok(sqlx::query_as::<_, BoardCardRow>(&format!(
            "SELECT {CARD_COLS} FROM board_cards WHERE user_id = ? AND project_id = ?{archived} \
             ORDER BY column_key ASC, position ASC, id ASC"
        ))
        .bind(user_id)
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn cards_page(
        &self,
        user_id: &str,
        project_id: &str,
        filter: &BoardCardFilter,
        after: Option<&BoardCardCursor>,
        limit: i64,
    ) -> Result<Vec<BoardCardRow>, DbError> {
        let mut builder = QueryBuilder::<Sqlite>::new(format!("SELECT {CARD_COLS} FROM board_cards WHERE user_id = "));
        builder.push_bind(user_id.to_owned());
        builder.push(" AND project_id = ").push_bind(project_id.to_owned());
        if !filter.include_archived {
            builder.push(" AND archived_at IS NULL");
        }
        if let Some(column) = &filter.column_key {
            builder.push(" AND column_key = ").push_bind(column.clone());
        }
        if let Some(assignee) = &filter.assignee_ref {
            builder.push(" AND assignee_ref = ").push_bind(assignee.clone());
        }
        if let Some(card_type) = &filter.card_type {
            builder.push(" AND type = ").push_bind(card_type.clone());
        }
        if let Some(priority) = &filter.priority {
            builder.push(" AND priority = ").push_bind(priority.clone());
        }
        if let Some(label) = &filter.label {
            builder
                .push(" AND EXISTS (SELECT 1 FROM json_each(board_cards.labels) WHERE value = ")
                .push_bind(label.clone())
                .push(")");
        }
        if let Some(skill) = &filter.skill {
            builder
                .push(" AND EXISTS (SELECT 1 FROM json_each(board_cards.skills) WHERE value = ")
                .push_bind(skill.clone())
                .push(")");
        }
        if let Some(blocked) = filter.blocked {
            builder.push(" AND blocked = ").push_bind(blocked);
        }
        if let Some(cursor) = after {
            // Expanded lexicographic form so the leading range can use
            // idx_board_cards_column_order.
            builder
                .push(" AND (column_key > ")
                .push_bind(cursor.column_key.clone())
                .push(" OR (column_key = ")
                .push_bind(cursor.column_key.clone())
                .push(" AND (position > ")
                .push_bind(cursor.position)
                .push(" OR (position = ")
                .push_bind(cursor.position)
                .push(" AND id > ")
                .push_bind(cursor.id.clone())
                .push(")))) ");
        }
        builder
            .push(" ORDER BY column_key ASC, position ASC, id ASC LIMIT ")
            .push_bind(limit);
        Ok(builder.build_query_as::<BoardCardRow>().fetch_all(&self.pool).await?)
    }

    async fn card(&self, user_id: &str, card_id: &str) -> Result<Option<BoardCardRow>, DbError> {
        fetch_card(&self.pool, user_id, card_id).await
    }

    async fn card_by_number(
        &self,
        user_id: &str,
        project_id: &str,
        number: i64,
    ) -> Result<Option<BoardCardRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardCardRow>(&format!(
            "SELECT {CARD_COLS} FROM board_cards WHERE user_id = ? AND project_id = ? AND number = ?"
        ))
        .bind(user_id)
        .bind(project_id)
        .bind(number)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn cards_by_ids(&self, user_id: &str, ids: &[String]) -> Result<Vec<BoardCardRow>, DbError> {
        fetch_cards_by_ids(&self.pool, user_id, ids).await
    }

    async fn reviews(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardReviewRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardReviewRow>(&format!(
            "SELECT {REVIEW_COLS} FROM board_card_reviews WHERE user_id = ? AND card_id = ? ORDER BY department ASC"
        ))
        .bind(user_id)
        .bind(card_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn questions(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardQuestionRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardQuestionRow>(&format!(
            "SELECT {QUESTION_COLS} FROM board_card_questions WHERE user_id = ? AND card_id = ? \
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(user_id)
        .bind(card_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn links(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardLinkViewRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardLinkViewRow>(
            "SELECT l.card_id, l.conversation_id, l.project_id, l.user_id, l.role, l.department, l.created_at, \
                    c.name AS conversation_name, c.status AS conversation_status, \
                    (c.id IS NOT NULL) AS conversation_exists, c.archived_at AS conversation_archived_at \
             FROM board_card_links l \
             LEFT JOIN conversations c ON c.id = l.conversation_id AND c.user_id = l.user_id \
             WHERE l.user_id = ? AND l.card_id = ? \
             ORDER BY l.created_at ASC, l.conversation_id ASC",
        )
        .bind(user_id)
        .bind(card_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn dependencies(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardDepRow>, DbError> {
        fetch_dependencies(&self.pool, user_id, card_id).await
    }

    async fn dependents(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardDepRow>, DbError> {
        fetch_dependents(&self.pool, user_id, card_id).await
    }

    async fn project_dependencies(&self, user_id: &str, project_id: &str) -> Result<Vec<BoardDepRow>, DbError> {
        fetch_project_dependencies(&self.pool, user_id, project_id).await
    }

    async fn events(&self, user_id: &str, query: &BoardEventQuery<'_>) -> Result<Vec<BoardEventRow>, DbError> {
        let mut builder = QueryBuilder::<Sqlite>::new(format!("SELECT {EVENT_COLS} FROM board_events WHERE user_id = "));
        builder.push_bind(user_id.to_owned());
        match query.scope {
            BoardEventScope::Project(id) => builder.push(" AND project_id = ").push_bind(id.to_owned()),
            BoardEventScope::Card(id) => builder.push(" AND card_id = ").push_bind(id.to_owned()),
            BoardEventScope::Conversation(id) => builder.push(" AND conversation_id = ").push_bind(id.to_owned()),
        };
        builder.push(" AND seq > ").push_bind(query.after_seq);
        if let Some(kinds) = query.kinds {
            if kinds.is_empty() {
                return Ok(Vec::new());
            }
            builder.push(" AND kind IN (");
            let mut separated = builder.separated(", ");
            for kind in kinds {
                separated.push_bind((*kind).to_owned());
            }
            builder.push(")");
        }
        builder.push(" ORDER BY seq ASC LIMIT ").push_bind(query.limit);
        Ok(builder.build_query_as::<BoardEventRow>().fetch_all(&self.pool).await?)
    }

    async fn recent_card_events(
        &self,
        user_id: &str,
        card_id: &str,
        limit: i64,
    ) -> Result<Vec<BoardEventRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardEventRow>(&format!(
            "SELECT {EVENT_COLS} FROM (\
                 SELECT {EVENT_COLS} FROM board_events WHERE user_id = ? AND card_id = ? ORDER BY seq DESC LIMIT ?\
             ) ORDER BY seq ASC"
        ))
        .bind(user_id)
        .bind(card_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn supervisor_cursor(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<Option<BoardSupervisorCursorRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardSupervisorCursorRow>(
            "SELECT project_id, user_id, last_seq, updated_at FROM board_supervisor_cursor \
             WHERE user_id = ? AND project_id = ?",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn count_cards(&self, user_id: &str, project_id: &str, include_archived: bool) -> Result<i64, DbError> {
        fetch_count_cards(&self.pool, user_id, project_id, include_archived).await
    }

    async fn last_seq(&self, user_id: &str, project_id: &str) -> Result<i64, DbError> {
        fetch_last_seq(&self.pool, user_id, project_id).await
    }

    async fn conversation_by_assignment(&self, user_id: &str, assignment_id: &str) -> Result<Option<String>, DbError> {
        Ok(sqlx::query_scalar(&format!(
            "SELECT id FROM conversations WHERE user_id = ? AND {ASSIGNMENT_EXPR} = ?"
        ))
        .bind(user_id)
        .bind(assignment_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn conversation_info(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<BoardConversationInfo>, DbError> {
        let row: Option<(String, String, Option<String>, i64, i64, bool, bool)> = sqlx::query_as(
            "SELECT c.id, c.name, c.project_id, c.depth, \
                    (SELECT COUNT(*) FROM conversations k WHERE k.user_id = c.user_id \
                       AND k.parent_conversation_id = c.id), \
                    COALESCE( \
                       NULLIF(json_extract(CASE WHEN json_valid(c.extra) THEN c.extra ELSE '{}' END, '$.team_id'), ''), \
                       NULLIF(json_extract(CASE WHEN json_valid(c.extra) THEN c.extra ELSE '{}' END, '$.teamId'), '') \
                    ) IS NOT NULL, \
                    c.archived_at IS NOT NULL \
             FROM conversations c WHERE c.user_id = ? AND c.id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(conversation_id, name, project_id, depth, child_count, team_member, archived)| BoardConversationInfo {
                conversation_id,
                name,
                project_id,
                depth,
                child_count,
                team_member,
                archived,
            },
        ))
    }

    async fn idempotency(
        &self,
        user_id: &str,
        project_id: &str,
        key: &str,
    ) -> Result<Option<BoardIdempotencyRow>, DbError> {
        fetch_idempotency(&self.pool, user_id, project_id, key).await
    }

    async fn first_user_message(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT COALESCE(m.msg_id, m.id) FROM messages m \
             JOIN conversations c ON c.id = m.conversation_id \
             WHERE c.user_id = ? AND m.conversation_id = ? AND m.position = 'right' \
             ORDER BY m.created_at ASC, m.id ASC LIMIT 1",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn delete_board(&self, user_id: &str, project_id: &str) -> Result<u64, DbError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let cards = sqlx::query("DELETE FROM board_cards WHERE user_id = ? AND project_id = ?")
            .bind(user_id)
            .bind(project_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        for table in BOARD_TABLES {
            sqlx::query(&format!("DELETE FROM {table} WHERE user_id = ? AND project_id = ?"))
                .bind(user_id)
                .bind(project_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(cards)
    }

    async fn begin(&self) -> Result<Box<dyn IBoardTx>, DbError> {
        let tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Ok(Box::new(SqliteBoardTx::new(tx)))
    }
}
