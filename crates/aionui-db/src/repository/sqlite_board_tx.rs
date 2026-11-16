//! The board's `BEGIN IMMEDIATE` write transaction ([`IBoardTx`]).
//!
//! `BEGIN IMMEDIATE` claims the writer lock up front (pattern of
//! `sqlite_user_order.rs`), so the read-check-write sequences the board
//! service runs — TASK-N claim, WIP count, neighbour positions, version
//! compare — cannot interleave with another writer. The wrapped sqlx
//! `Transaction` rolls back when dropped without `commit`.

use sqlx::{Sqlite, SqliteConnection, Transaction};

use crate::error::DbError;
use crate::models::{
    BoardCardRow, BoardColumnRow, BoardDepRow, BoardDeployJobRow, BoardIdempotencyRow, BoardLinkRow, BoardQuestionRow,
    BoardReviewRow, BoardSettingsRow,
};
use crate::repository::board::{
    BOARD_POSITION_GAP, BoardCardCreator, BoardEventSeq, BoardPlacement, BoardPlacementOutcome, IBoardTx, NewBoardEvent,
};
use crate::repository::sqlite_board::{
    IDEMPOTENCY_COLS, QUESTION_COLS, REVIEW_COLS, fetch_card, fetch_card_creators, fetch_cards_by_ids, fetch_columns,
    fetch_count_cards, fetch_dependencies, fetch_dependents, fetch_idempotency, fetch_last_seq,
    fetch_project_dependencies, fetch_settings,
};

/// One open board write transaction.
pub(crate) struct SqliteBoardTx {
    tx: Transaction<'static, Sqlite>,
    /// Cards renumbered by rebalances in this transaction.
    rebalanced: Vec<(String, i64)>,
}

impl SqliteBoardTx {
    pub(crate) fn new(tx: Transaction<'static, Sqlite>) -> Self {
        Self {
            tx,
            rebalanced: Vec::new(),
        }
    }

    fn conn(&mut self) -> &mut SqliteConnection {
        &mut self.tx
    }
}

/// Append `event` to its project's chain in `schema` (`main` here; recover
/// writes through the same function): read the project's newest `seq`, insert
/// with it as `prev_seq`, move `board_settings.last_event_seq`. Callers hold a
/// `BEGIN IMMEDIATE` transaction, so the read-insert-update is atomic.
pub(crate) async fn insert_chained_event(
    conn: &mut SqliteConnection,
    schema: &str,
    event: &NewBoardEvent,
) -> Result<BoardEventSeq, DbError> {
    let prev_seq: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(MAX(seq), 0) FROM {schema}.board_events WHERE project_id = ?"
    ))
    .bind(&event.project_id)
    .fetch_one(&mut *conn)
    .await?;
    let seq: i64 = sqlx::query_scalar(&format!(
        "INSERT INTO {schema}.board_events \
         (id, card_id, project_id, user_id, actor_kind, actor_ref, conversation_id, kind, payload, prev_seq, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING seq"
    ))
    .bind(&event.id)
    .bind(&event.card_id)
    .bind(&event.project_id)
    .bind(&event.user_id)
    .bind(&event.actor_kind)
    .bind(&event.actor_ref)
    .bind(&event.conversation_id)
    .bind(&event.kind)
    .bind(&event.payload)
    .bind(prev_seq)
    .bind(event.created_at)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(&format!(
        "UPDATE {schema}.board_settings SET last_event_seq = ? WHERE project_id = ?"
    ))
    .bind(seq)
    .bind(&event.project_id)
    .execute(&mut *conn)
    .await?;
    Ok(BoardEventSeq { seq, prev_seq })
}

/// Where a placement lands among a column's current positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slot {
    /// Write the card at this position.
    At(i64),
    /// The neighbour gap is `< 2`: renumber the column first.
    Rebalance,
    /// The anchor is not in the column.
    AnchorMissing,
}

/// Position for a placement among `rows` (the column's active cards ordered
/// by `(position, id)`, the moving card excluded).
pub(crate) fn compute_position(rows: &[(String, i64)], placement: BoardPlacement<'_>) -> Slot {
    let midpoint = |left: i64, right: i64| {
        if right - left >= 2 {
            Slot::At(left + (right - left) / 2)
        } else {
            Slot::Rebalance
        }
    };
    let anchor_index = |anchor: &str| rows.iter().position(|(id, _)| id == anchor);
    match placement {
        BoardPlacement::Tail => Slot::At(
            rows.last()
                .map(|(_, position)| position + BOARD_POSITION_GAP)
                .unwrap_or(BOARD_POSITION_GAP),
        ),
        BoardPlacement::Before(anchor) => {
            let Some(index) = anchor_index(anchor) else {
                return Slot::AnchorMissing;
            };
            let right = rows[index].1;
            match index.checked_sub(1).map(|left| rows[left].1) {
                None => Slot::At(right - BOARD_POSITION_GAP),
                Some(left) => midpoint(left, right),
            }
        }
        BoardPlacement::After(anchor) => {
            let Some(index) = anchor_index(anchor) else {
                return Slot::AnchorMissing;
            };
            let left = rows[index].1;
            match rows.get(index + 1) {
                None => Slot::At(left + BOARD_POSITION_GAP),
                Some((_, right)) => midpoint(left, *right),
            }
        }
    }
}

async fn column_positions(
    conn: &mut SqliteConnection,
    user_id: &str,
    project_id: &str,
    column_key: &str,
    moving_card_id: Option<&str>,
) -> Result<Vec<(String, i64)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id, position FROM board_cards \
         WHERE user_id = ? AND project_id = ? AND column_key = ? AND archived_at IS NULL AND id != ? \
         ORDER BY position ASC, id ASC",
    )
    .bind(user_id)
    .bind(project_id)
    .bind(column_key)
    .bind(moving_card_id.unwrap_or(""))
    .fetch_all(&mut *conn)
    .await?)
}

#[async_trait::async_trait]
impl IBoardTx for SqliteBoardTx {
    async fn settings(&mut self, user_id: &str, project_id: &str) -> Result<Option<BoardSettingsRow>, DbError> {
        fetch_settings(self.conn(), user_id, project_id).await
    }

    async fn insert_settings(&mut self, row: &BoardSettingsRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO board_settings \
             (project_id, user_id, enabled, template, next_number, version, settings, archived_at, \
              keeper_conversation_id, last_event_seq, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.project_id)
        .bind(&row.user_id)
        .bind(row.enabled)
        .bind(&row.template)
        .bind(row.next_number)
        .bind(row.version)
        .bind(&row.settings)
        .bind(row.archived_at)
        .bind(&row.keeper_conversation_id)
        .bind(row.last_event_seq)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn update_settings(&mut self, row: &BoardSettingsRow, expected_version: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE board_settings SET enabled = ?, template = ?, settings = ?, archived_at = ?, \
             keeper_conversation_id = ?, version = ?, updated_at = ? \
             WHERE user_id = ? AND project_id = ? AND version = ?",
        )
        .bind(row.enabled)
        .bind(&row.template)
        .bind(&row.settings)
        .bind(row.archived_at)
        .bind(&row.keeper_conversation_id)
        .bind(row.version)
        .bind(row.updated_at)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(expected_version)
        .execute(self.conn())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn claim_number(&mut self, user_id: &str, project_id: &str) -> Result<Option<i64>, DbError> {
        Ok(sqlx::query_scalar(
            "UPDATE board_settings SET next_number = next_number + 1 \
             WHERE user_id = ? AND project_id = ? RETURNING next_number - 1",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_optional(self.conn())
        .await?)
    }

    async fn columns(&mut self, user_id: &str, project_id: &str) -> Result<Vec<BoardColumnRow>, DbError> {
        fetch_columns(self.conn(), user_id, project_id).await
    }

    async fn upsert_columns(&mut self, rows: &[BoardColumnRow]) -> Result<(), DbError> {
        for row in rows {
            sqlx::query(
                "INSERT INTO board_columns \
                 (id, project_id, user_id, key, name, category, kind, resolution, agent_move, wip_limit, exit_rule, \
                  gate, position, archived) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (project_id, key) DO UPDATE SET \
                    name = excluded.name, category = excluded.category, kind = excluded.kind, \
                    resolution = excluded.resolution, agent_move = excluded.agent_move, \
                    wip_limit = excluded.wip_limit, exit_rule = excluded.exit_rule, gate = excluded.gate, \
                    position = excluded.position, archived = excluded.archived \
                 WHERE board_columns.user_id = excluded.user_id",
            )
            .bind(&row.id)
            .bind(&row.project_id)
            .bind(&row.user_id)
            .bind(&row.key)
            .bind(&row.name)
            .bind(&row.category)
            .bind(&row.kind)
            .bind(&row.resolution)
            .bind(&row.agent_move)
            .bind(row.wip_limit)
            .bind(&row.exit_rule)
            .bind(&row.gate)
            .bind(row.position)
            .bind(row.archived)
            .execute(self.conn())
            .await?;
        }
        Ok(())
    }

    async fn card(&mut self, user_id: &str, card_id: &str) -> Result<Option<BoardCardRow>, DbError> {
        fetch_card(self.conn(), user_id, card_id).await
    }

    async fn cards_by_ids(&mut self, user_id: &str, ids: &[String]) -> Result<Vec<BoardCardRow>, DbError> {
        fetch_cards_by_ids(self.conn(), user_id, ids).await
    }

    async fn card_creators(&mut self, user_id: &str, card_ids: &[String]) -> Result<Vec<BoardCardCreator>, DbError> {
        fetch_card_creators(self.conn(), user_id, card_ids).await
    }

    async fn count_cards(&mut self, user_id: &str, project_id: &str, include_archived: bool) -> Result<i64, DbError> {
        fetch_count_cards(self.conn(), user_id, project_id, include_archived).await
    }

    async fn count_column_cards(
        &mut self,
        user_id: &str,
        project_id: &str,
        column_key: &str,
        exclude_card_id: Option<&str>,
    ) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM board_cards \
             WHERE user_id = ? AND project_id = ? AND column_key = ? AND archived_at IS NULL AND id != ?",
        )
        .bind(user_id)
        .bind(project_id)
        .bind(column_key)
        .bind(exclude_card_id.unwrap_or(""))
        .fetch_one(self.conn())
        .await?)
    }

    async fn place(
        &mut self,
        user_id: &str,
        project_id: &str,
        column_key: &str,
        moving_card_id: Option<&str>,
        placement: BoardPlacement<'_>,
    ) -> Result<BoardPlacementOutcome, DbError> {
        let conn = self.conn();
        let rows = column_positions(&mut *conn, user_id, project_id, column_key, moving_card_id).await?;
        match compute_position(&rows, placement) {
            Slot::AnchorMissing => Ok(BoardPlacementOutcome::AnchorNotFound),
            Slot::At(position) => Ok(BoardPlacementOutcome::Placed(position)),
            Slot::Rebalance => {
                // Gap exhausted: renumber the column 1024, 2048, … in its
                // current (position, id) order, then recompute — after the
                // rebalance every neighbour gap is 1024, so it always fits.
                let mut renumbered = Vec::with_capacity(rows.len());
                for (index, (id, _)) in rows.into_iter().enumerate() {
                    let position = (index as i64 + 1) * BOARD_POSITION_GAP;
                    sqlx::query("UPDATE board_cards SET position = ? WHERE user_id = ? AND id = ?")
                        .bind(position)
                        .bind(user_id)
                        .bind(&id)
                        .execute(&mut *conn)
                        .await?;
                    renumbered.push((id, position));
                }
                self.rebalanced.extend(renumbered.iter().cloned());
                match compute_position(&renumbered, placement) {
                    Slot::At(position) => Ok(BoardPlacementOutcome::Placed(position)),
                    Slot::Rebalance => Err(DbError::Conflict("board column rebalance left no gap".into())),
                    Slot::AnchorMissing => Ok(BoardPlacementOutcome::AnchorNotFound),
                }
            }
        }
    }

    async fn insert_card(&mut self, row: &BoardCardRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO board_cards \
             (id, project_id, user_id, number, title, description, type, priority, size, tier, column_key, position, \
              labels, skills, routing, creator_kind, creator_ref, assignee_kind, assignee_ref, blocked, blocked_reason, \
              blocked_by, \
              change_id, metadata, version, archived_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.project_id)
        .bind(&row.user_id)
        .bind(row.number)
        .bind(&row.title)
        .bind(&row.description)
        .bind(&row.card_type)
        .bind(&row.priority)
        .bind(&row.size)
        .bind(&row.tier)
        .bind(&row.column_key)
        .bind(row.position)
        .bind(&row.labels)
        .bind(&row.skills)
        .bind(&row.routing)
        .bind(&row.creator_kind)
        .bind(&row.creator_ref)
        .bind(&row.assignee_kind)
        .bind(&row.assignee_ref)
        .bind(row.blocked)
        .bind(&row.blocked_reason)
        .bind(&row.blocked_by)
        .bind(&row.change_id)
        .bind(&row.metadata)
        .bind(row.version)
        .bind(row.archived_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn update_card(&mut self, row: &BoardCardRow, expected_version: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE board_cards SET title = ?, description = ?, type = ?, priority = ?, size = ?, tier = ?, \
             column_key = ?, position = ?, labels = ?, skills = ?, routing = ?, assignee_kind = ?, assignee_ref = ?, \
             blocked = ?, blocked_reason = ?, blocked_by = ?, change_id = ?, metadata = ?, version = ?, archived_at = ?, \
             updated_at = ? \
             WHERE user_id = ? AND id = ? AND version = ?",
        )
        .bind(&row.title)
        .bind(&row.description)
        .bind(&row.card_type)
        .bind(&row.priority)
        .bind(&row.size)
        .bind(&row.tier)
        .bind(&row.column_key)
        .bind(row.position)
        .bind(&row.labels)
        .bind(&row.skills)
        .bind(&row.routing)
        .bind(&row.assignee_kind)
        .bind(&row.assignee_ref)
        .bind(row.blocked)
        .bind(&row.blocked_reason)
        .bind(&row.blocked_by)
        .bind(&row.change_id)
        .bind(&row.metadata)
        .bind(row.version)
        .bind(row.archived_at)
        .bind(row.updated_at)
        .bind(&row.user_id)
        .bind(&row.id)
        .bind(expected_version)
        .execute(self.conn())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn dependencies(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardDepRow>, DbError> {
        fetch_dependencies(self.conn(), user_id, card_id).await
    }

    async fn dependents(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardDepRow>, DbError> {
        fetch_dependents(self.conn(), user_id, card_id).await
    }

    async fn project_dependencies(&mut self, user_id: &str, project_id: &str) -> Result<Vec<BoardDepRow>, DbError> {
        fetch_project_dependencies(self.conn(), user_id, project_id).await
    }

    async fn insert_dependency(&mut self, row: &BoardDepRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO board_card_deps (card_id, depends_on_card_id, project_id, user_id, created_at) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT (card_id, depends_on_card_id) DO NOTHING",
        )
        .bind(&row.card_id)
        .bind(&row.depends_on_card_id)
        .bind(&row.project_id)
        .bind(&row.user_id)
        .bind(row.created_at)
        .execute(self.conn())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn delete_dependency(
        &mut self,
        user_id: &str,
        card_id: &str,
        depends_on_card_id: &str,
    ) -> Result<bool, DbError> {
        let result =
            sqlx::query("DELETE FROM board_card_deps WHERE user_id = ? AND card_id = ? AND depends_on_card_id = ?")
                .bind(user_id)
                .bind(card_id)
                .bind(depends_on_card_id)
                .execute(self.conn())
                .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn link(
        &mut self,
        user_id: &str,
        card_id: &str,
        conversation_id: &str,
    ) -> Result<Option<BoardLinkRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardLinkRow>(
            "SELECT card_id, conversation_id, project_id, user_id, role, department, created_at \
             FROM board_card_links WHERE user_id = ? AND card_id = ? AND conversation_id = ?",
        )
        .bind(user_id)
        .bind(card_id)
        .bind(conversation_id)
        .fetch_optional(self.conn())
        .await?)
    }

    async fn insert_link(&mut self, row: &BoardLinkRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO board_card_links (card_id, conversation_id, project_id, user_id, role, department, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.card_id)
        .bind(&row.conversation_id)
        .bind(&row.project_id)
        .bind(&row.user_id)
        .bind(&row.role)
        .bind(&row.department)
        .bind(row.created_at)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn question(
        &mut self,
        user_id: &str,
        card_id: &str,
        question_id: &str,
    ) -> Result<Option<BoardQuestionRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardQuestionRow>(&format!(
            "SELECT {QUESTION_COLS} FROM board_card_questions WHERE user_id = ? AND card_id = ? AND id = ?"
        ))
        .bind(user_id)
        .bind(card_id)
        .bind(question_id)
        .fetch_optional(self.conn())
        .await?)
    }

    async fn update_question(&mut self, row: &BoardQuestionRow, expected_version: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE board_card_questions SET answered_by_event = ?, version = ?, updated_at = ? \
             WHERE user_id = ? AND id = ? AND version = ?",
        )
        .bind(&row.answered_by_event)
        .bind(row.version)
        .bind(row.updated_at)
        .bind(&row.user_id)
        .bind(&row.id)
        .bind(expected_version)
        .execute(self.conn())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn reviews(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardReviewRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardReviewRow>(&format!(
            "SELECT {REVIEW_COLS} FROM board_card_reviews WHERE user_id = ? AND card_id = ? ORDER BY department ASC"
        ))
        .bind(user_id)
        .bind(card_id)
        .fetch_all(self.conn())
        .await?)
    }

    async fn upsert_review(&mut self, row: &BoardReviewRow, expected_version: Option<i64>) -> Result<bool, DbError> {
        let result =
            match expected_version {
                None => {
                    sqlx::query(&format!(
                        "INSERT INTO board_card_reviews ({REVIEW_COLS}) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (card_id, department) DO NOTHING"
                    ))
                    .bind(&row.id)
                    .bind(&row.card_id)
                    .bind(&row.project_id)
                    .bind(&row.user_id)
                    .bind(&row.department)
                    .bind(&row.reviewer_ref)
                    .bind(&row.conversation_id)
                    .bind(&row.verdict)
                    .bind(&row.summary)
                    .bind(&row.findings)
                    .bind(&row.recommendation)
                    .bind(&row.assumptions)
                    .bind(row.confidence)
                    .bind(row.tokens_used)
                    .bind(row.version)
                    .bind(row.created_at)
                    .bind(row.updated_at)
                    .execute(self.conn())
                    .await?
                }
                Some(expected) => sqlx::query(
                    "UPDATE board_card_reviews SET reviewer_ref = ?, conversation_id = ?, verdict = ?, summary = ?, \
                     findings = ?, recommendation = ?, assumptions = ?, confidence = ?, tokens_used = ?, version = ?, \
                     updated_at = ? WHERE user_id = ? AND id = ? AND version = ?",
                )
                .bind(&row.reviewer_ref)
                .bind(&row.conversation_id)
                .bind(&row.verdict)
                .bind(&row.summary)
                .bind(&row.findings)
                .bind(&row.recommendation)
                .bind(&row.assumptions)
                .bind(row.confidence)
                .bind(row.tokens_used)
                .bind(row.version)
                .bind(row.updated_at)
                .bind(&row.user_id)
                .bind(&row.id)
                .bind(expected)
                .execute(self.conn())
                .await?,
            };
        Ok(result.rows_affected() == 1)
    }

    async fn questions(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardQuestionRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardQuestionRow>(&format!(
            "SELECT {QUESTION_COLS} FROM board_card_questions WHERE user_id = ? AND card_id = ? \
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(user_id)
        .bind(card_id)
        .fetch_all(self.conn())
        .await?)
    }

    async fn insert_question(&mut self, row: &BoardQuestionRow) -> Result<(), DbError> {
        sqlx::query(&format!(
            "INSERT INTO board_card_questions ({QUESTION_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(&row.id)
        .bind(&row.card_id)
        .bind(&row.project_id)
        .bind(&row.user_id)
        .bind(&row.from_department)
        .bind(&row.to_department)
        .bind(&row.text)
        .bind(&row.answered_by_event)
        .bind(row.version)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn links(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardLinkRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardLinkRow>(
            "SELECT card_id, conversation_id, project_id, user_id, role, department, created_at \
             FROM board_card_links WHERE user_id = ? AND card_id = ? ORDER BY created_at ASC, conversation_id ASC",
        )
        .bind(user_id)
        .bind(card_id)
        .fetch_all(self.conn())
        .await?)
    }

    async fn delete_link(&mut self, user_id: &str, card_id: &str, conversation_id: &str) -> Result<bool, DbError> {
        let result =
            sqlx::query("DELETE FROM board_card_links WHERE user_id = ? AND card_id = ? AND conversation_id = ?")
                .bind(user_id)
                .bind(card_id)
                .bind(conversation_id)
                .execute(self.conn())
                .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn idempotency(
        &mut self,
        user_id: &str,
        project_id: &str,
        key: &str,
    ) -> Result<Option<BoardIdempotencyRow>, DbError> {
        fetch_idempotency(self.conn(), user_id, project_id, key).await
    }

    async fn insert_idempotency(&mut self, row: &BoardIdempotencyRow) -> Result<(), DbError> {
        sqlx::query(&format!(
            "INSERT INTO board_idempotency ({IDEMPOTENCY_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.key)
        .bind(&row.request_hash)
        .bind(&row.state)
        .bind(&row.assignment_id)
        .bind(&row.response)
        .bind(&row.claim_token)
        .bind(&row.packet_message_id)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn update_idempotency(&mut self, row: &BoardIdempotencyRow, claim_token: &str) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE board_idempotency SET state = ?, assignment_id = ?, response = ?, packet_message_id = ?, \
             updated_at = ? WHERE user_id = ? AND project_id = ? AND key = ? AND claim_token = ?",
        )
        .bind(&row.state)
        .bind(&row.assignment_id)
        .bind(&row.response)
        .bind(&row.packet_message_id)
        .bind(row.updated_at)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.key)
        .bind(claim_token)
        .execute(self.conn())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn take_over_idempotency(
        &mut self,
        user_id: &str,
        project_id: &str,
        key: &str,
        new_token: &str,
        stale_before: i64,
        now: i64,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE board_idempotency SET claim_token = ?, updated_at = ? \
             WHERE user_id = ? AND project_id = ? AND key = ? AND state = 'pending' AND updated_at < ?",
        )
        .bind(new_token)
        .bind(now)
        .bind(user_id)
        .bind(project_id)
        .bind(key)
        .bind(stale_before)
        .execute(self.conn())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn active_deploy_job(&mut self, user_id: &str, card_id: &str) -> Result<Option<BoardDeployJobRow>, DbError> {
        Ok(sqlx::query_as::<_, BoardDeployJobRow>(
            "SELECT id, card_id, project_id, user_id, idempotency_key, state, detail, created_at, updated_at \
             FROM board_deploy_jobs WHERE user_id = ? AND card_id = ? AND state IN ('requested', 'running') \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(card_id)
        .fetch_optional(self.conn())
        .await?)
    }

    async fn update_deploy_job_detail(
        &mut self,
        user_id: &str,
        job_id: &str,
        detail: &str,
        now: i64,
    ) -> Result<(), DbError> {
        sqlx::query("UPDATE board_deploy_jobs SET detail = ?, updated_at = ? WHERE user_id = ? AND id = ?")
            .bind(detail)
            .bind(now)
            .bind(user_id)
            .bind(job_id)
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn last_seq(&mut self, user_id: &str, project_id: &str) -> Result<i64, DbError> {
        fetch_last_seq(self.conn(), user_id, project_id).await
    }

    async fn insert_event(&mut self, event: &NewBoardEvent) -> Result<BoardEventSeq, DbError> {
        insert_chained_event(self.conn(), "main", event).await
    }

    fn take_rebalanced(&mut self) -> Vec<(String, i64)> {
        std::mem::take(&mut self.rebalanced)
    }

    async fn commit(self: Box<Self>) -> Result<(), DbError> {
        self.tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(positions: &[(&str, i64)]) -> Vec<(String, i64)> {
        positions.iter().map(|(id, p)| ((*id).to_owned(), *p)).collect()
    }

    #[test]
    fn tail_of_an_empty_column_starts_at_one_gap() {
        assert_eq!(compute_position(&[], BoardPlacement::Tail), Slot::At(1024));
        assert_eq!(
            compute_position(&rows(&[("a", 1024), ("b", 2048)]), BoardPlacement::Tail),
            Slot::At(3072)
        );
    }

    #[test]
    fn head_insert_is_first_minus_one_gap() {
        assert_eq!(
            compute_position(&rows(&[("a", 1024)]), BoardPlacement::Before("a")),
            Slot::At(0)
        );
    }

    #[test]
    fn between_neighbours_is_the_integer_midpoint() {
        let column = rows(&[("a", 1024), ("b", 2048), ("c", 2051)]);
        assert_eq!(compute_position(&column, BoardPlacement::After("a")), Slot::At(1536));
        assert_eq!(compute_position(&column, BoardPlacement::Before("b")), Slot::At(1536));
        assert_eq!(compute_position(&column, BoardPlacement::After("b")), Slot::At(2049));
        assert_eq!(compute_position(&column, BoardPlacement::After("c")), Slot::At(3075));
    }

    #[test]
    fn a_gap_below_two_asks_for_a_rebalance() {
        let column = rows(&[("a", 10), ("b", 11), ("c", 11)]);
        assert_eq!(compute_position(&column, BoardPlacement::After("a")), Slot::Rebalance);
        assert_eq!(compute_position(&column, BoardPlacement::Before("c")), Slot::Rebalance);
    }

    #[test]
    fn an_unknown_anchor_is_reported() {
        assert_eq!(
            compute_position(&rows(&[("a", 1)]), BoardPlacement::After("x")),
            Slot::AnchorMissing
        );
    }
}
