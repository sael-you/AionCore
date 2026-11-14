//! Access boundary for the project board tables (migration 901).
//!
//! The board service owns every rule; this module owns SQL and transactions.
//! Reads go through [`IBoardStore`]. Every write runs inside one
//! `BEGIN IMMEDIATE` transaction obtained from [`IBoardStore::begin`] and
//! driven through [`IBoardTx`], so the service can run its checks (WIP,
//! blocked, versions) against the same snapshot it writes — dropping the
//! transaction without [`IBoardTx::commit`] rolls it back.
//!
//! Every method takes the acting `user_id` from the server principal and
//! filters on the owner column, so a user can neither see nor mutate another
//! user's board rows (R7b).

use async_trait::async_trait;

use crate::error::DbError;
use crate::models::{
    BoardCardRow, BoardColumnRow, BoardDepRow, BoardDeployJobRow, BoardEventRow, BoardIdempotencyRow, BoardLinkRow,
    BoardLinkViewRow, BoardQuestionRow, BoardReviewRow, BoardSettingsRow, BoardSupervisorCursorRow,
};

/// Spacing of card positions for fresh, head/tail and rebalanced placements.
pub const BOARD_POSITION_GAP: i64 = 1024;

/// The owning project of a board, as the board sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardProjectRow {
    pub project_id: String,
    pub name: String,
    /// `standard` | `temp`.
    pub kind: String,
}

/// A conversation as assign sees it (parent eligibility, link scope).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardConversationInfo {
    pub conversation_id: String,
    pub name: String,
    pub project_id: Option<String>,
    /// Nesting depth (root = 0).
    pub depth: i64,
    /// Existing children, archived included.
    pub child_count: i64,
    /// The conversation is a team member (`extra.team_id|teamId`).
    pub team_member: bool,
    pub archived: bool,
}

/// What the agent router needs to know about the caller's conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardCallerRow {
    pub conversation_id: String,
    pub project_id: Option<String>,
    /// Raw `conversations.extra` JSON.
    pub extra: String,
    /// `conversations.type`.
    pub conversation_type: String,
    /// `conversation_assistant_snapshots.assistant_id`, when snapshotted.
    pub assistant_id: Option<String>,
}

/// Which slice of `board_events` a page reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardEventScope<'a> {
    Project(&'a str),
    Card(&'a str),
    Conversation(&'a str),
}

/// Keyset page over `board_events` ordered by `seq` ascending.
#[derive(Debug, Clone, Copy)]
pub struct BoardEventQuery<'a> {
    pub scope: BoardEventScope<'a>,
    /// Exclusive lower bound.
    pub after_seq: i64,
    pub limit: i64,
    /// Restrict to these event kinds (e.g. `commented`).
    pub kinds: Option<&'a [&'a str]>,
}

/// Filters of the paged card list. `None` = no filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardCardFilter {
    pub column_key: Option<String>,
    pub assignee_ref: Option<String>,
    pub card_type: Option<String>,
    pub priority: Option<String>,
    /// Card carries this label (exact match).
    pub label: Option<String>,
    /// Card carries this tagged skill (exact match).
    pub skill: Option<String>,
    pub blocked: Option<bool>,
    pub include_archived: bool,
}

/// Stable keyset cursor of the paged card list: `(column_key, position, id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardCardCursor {
    pub column_key: String,
    pub position: i64,
    pub id: String,
}

/// Where a card lands inside its target column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardPlacement<'a> {
    /// After the last card (`last + 1024`, or `1024` in an empty column).
    Tail,
    /// Directly before the given card id.
    Before(&'a str),
    /// Directly after the given card id.
    After(&'a str),
}

/// Result of [`IBoardTx::place`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardPlacementOutcome {
    /// The position the moving card must be written with. Neighbours may have
    /// been renumbered (rebalance) inside the same transaction.
    Placed(i64),
    /// The anchor is not an active card of the target column.
    AnchorNotFound,
}

/// A `board_events` row to append; `seq` is assigned by SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct NewBoardEvent {
    pub id: String,
    pub card_id: Option<String>,
    pub project_id: String,
    pub user_id: String,
    pub actor_kind: String,
    pub actor_ref: String,
    pub conversation_id: Option<String>,
    pub kind: String,
    /// JSON object.
    pub payload: String,
    pub created_at: i64,
}

/// Read side of the board tables plus the transaction factory.
#[async_trait]
pub trait IBoardStore: Send + Sync {
    /// The user's project (any kind); `None` when absent or foreign.
    async fn project(&self, user_id: &str, project_id: &str) -> Result<Option<BoardProjectRow>, DbError>;

    /// The user's conversation as the agent router needs it.
    async fn caller(&self, user_id: &str, conversation_id: &str) -> Result<Option<BoardCallerRow>, DbError>;

    /// The project a team is bound to; `None` when the team is absent,
    /// foreign, or unbound.
    async fn team_project_id(&self, user_id: &str, team_id: &str) -> Result<Option<String>, DbError>;

    /// Whether any non-archived conversation or team is bound to the project.
    async fn project_has_active_binding(&self, user_id: &str, project_id: &str) -> Result<bool, DbError>;

    async fn settings(&self, user_id: &str, project_id: &str) -> Result<Option<BoardSettingsRow>, DbError>;

    /// All columns (archived included) ordered by `position`.
    async fn columns(&self, user_id: &str, project_id: &str) -> Result<Vec<BoardColumnRow>, DbError>;

    /// All cards of a board ordered by `(column_key, position, id)`.
    async fn cards(&self, user_id: &str, project_id: &str, include_archived: bool)
    -> Result<Vec<BoardCardRow>, DbError>;

    /// One keyset page of cards ordered by `(column_key, position, id)`.
    async fn cards_page(
        &self,
        user_id: &str,
        project_id: &str,
        filter: &BoardCardFilter,
        after: Option<&BoardCardCursor>,
        limit: i64,
    ) -> Result<Vec<BoardCardRow>, DbError>;

    async fn card(&self, user_id: &str, card_id: &str) -> Result<Option<BoardCardRow>, DbError>;

    async fn card_by_number(
        &self,
        user_id: &str,
        project_id: &str,
        number: i64,
    ) -> Result<Option<BoardCardRow>, DbError>;

    async fn cards_by_ids(&self, user_id: &str, ids: &[String]) -> Result<Vec<BoardCardRow>, DbError>;

    async fn reviews(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardReviewRow>, DbError>;

    async fn questions(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardQuestionRow>, DbError>;

    /// Links of a card joined with the linked conversation (absent = removed).
    async fn links(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardLinkViewRow>, DbError>;

    /// Rows where `card_id` waits for another card.
    async fn dependencies(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardDepRow>, DbError>;

    /// Rows where another card waits for `card_id` (the derived `blocks`).
    async fn dependents(&self, user_id: &str, card_id: &str) -> Result<Vec<BoardDepRow>, DbError>;

    /// Every dependency edge of a project.
    async fn project_dependencies(&self, user_id: &str, project_id: &str) -> Result<Vec<BoardDepRow>, DbError>;

    /// One page of events after `query.after_seq`, ascending.
    async fn events(&self, user_id: &str, query: &BoardEventQuery<'_>) -> Result<Vec<BoardEventRow>, DbError>;

    /// The newest `limit` events of a card, returned in ascending `seq`.
    async fn recent_card_events(
        &self,
        user_id: &str,
        card_id: &str,
        limit: i64,
    ) -> Result<Vec<BoardEventRow>, DbError>;

    async fn supervisor_cursor(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<Option<BoardSupervisorCursorRow>, DbError>;

    /// Highest event `seq` of a project (0 when none).
    async fn last_seq(&self, user_id: &str, project_id: &str) -> Result<i64, DbError>;

    /// The user's conversation created for an assignment
    /// (`extra.board_assignment_id`, backed by the 901 unique index).
    async fn conversation_by_assignment(&self, user_id: &str, assignment_id: &str) -> Result<Option<String>, DbError>;

    async fn conversation_info(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<BoardConversationInfo>, DbError>;

    async fn idempotency(&self, user_id: &str, project_id: &str, key: &str)
    -> Result<Option<BoardIdempotencyRow>, DbError>;

    /// Id (`msg_id`, else row id) of the conversation's first user message.
    async fn first_user_message(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, DbError>;

    /// Count the board's cards (project-delete dry run).
    async fn count_cards(&self, user_id: &str, project_id: &str, include_archived: bool) -> Result<i64, DbError>;

    /// Hard-delete every board row of a project in one transaction (project
    /// deletion only). Returns the number of cards removed.
    async fn delete_board(&self, user_id: &str, project_id: &str) -> Result<u64, DbError>;

    /// Open a `BEGIN IMMEDIATE` write transaction.
    async fn begin(&self) -> Result<Box<dyn IBoardTx>, DbError>;
}

/// One board write transaction. Dropping it without [`commit`](Self::commit)
/// rolls back.
#[async_trait]
pub trait IBoardTx: Send {
    async fn settings(&mut self, user_id: &str, project_id: &str) -> Result<Option<BoardSettingsRow>, DbError>;

    async fn insert_settings(&mut self, row: &BoardSettingsRow) -> Result<(), DbError>;

    /// Write `enabled`, `template`, `settings`, `archived_at`, `version` and
    /// `updated_at` when the stored version equals `expected_version`.
    /// Returns `false` on a version mismatch (no row changed).
    async fn update_settings(&mut self, row: &BoardSettingsRow, expected_version: i64) -> Result<bool, DbError>;

    /// Claim the next TASK-N: `UPDATE … SET next_number = next_number + 1
    /// RETURNING next_number - 1`. `None` when the board does not exist.
    async fn claim_number(&mut self, user_id: &str, project_id: &str) -> Result<Option<i64>, DbError>;

    async fn columns(&mut self, user_id: &str, project_id: &str) -> Result<Vec<BoardColumnRow>, DbError>;

    /// Insert or update columns by `(project_id, key)`.
    async fn upsert_columns(&mut self, rows: &[BoardColumnRow]) -> Result<(), DbError>;

    async fn card(&mut self, user_id: &str, card_id: &str) -> Result<Option<BoardCardRow>, DbError>;

    async fn cards_by_ids(&mut self, user_id: &str, ids: &[String]) -> Result<Vec<BoardCardRow>, DbError>;

    async fn count_cards(&mut self, user_id: &str, project_id: &str, include_archived: bool) -> Result<i64, DbError>;

    /// Active (non-archived) cards in a column, excluding `exclude_card_id`.
    async fn count_column_cards(
        &mut self,
        user_id: &str,
        project_id: &str,
        column_key: &str,
        exclude_card_id: Option<&str>,
    ) -> Result<i64, DbError>;

    /// Compute the position for `moving_card_id` (or a new card when `None`)
    /// in `column_key`: integer midpoint between the neighbours, head/tail at
    /// `first − 1024` / `last + 1024`; when the gap is `< 2` the whole column
    /// is renumbered `1024, 2048, …` (ties broken by `(position, id)`) inside
    /// this transaction first. The moving card's own row is not written.
    async fn place(
        &mut self,
        user_id: &str,
        project_id: &str,
        column_key: &str,
        moving_card_id: Option<&str>,
        placement: BoardPlacement<'_>,
    ) -> Result<BoardPlacementOutcome, DbError>;

    async fn insert_card(&mut self, row: &BoardCardRow) -> Result<(), DbError>;

    /// Write every mutable column of `row` when the stored version equals
    /// `expected_version`. Returns `false` on a mismatch.
    async fn update_card(&mut self, row: &BoardCardRow, expected_version: i64) -> Result<bool, DbError>;

    async fn dependencies(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardDepRow>, DbError>;

    async fn dependents(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardDepRow>, DbError>;

    /// Every dependency edge of a project (cycle detection).
    async fn project_dependencies(&mut self, user_id: &str, project_id: &str) -> Result<Vec<BoardDepRow>, DbError>;

    /// Insert an edge; `false` when it already exists.
    async fn insert_dependency(&mut self, row: &BoardDepRow) -> Result<bool, DbError>;

    /// Delete an edge; `false` when it did not exist.
    async fn delete_dependency(
        &mut self,
        user_id: &str,
        card_id: &str,
        depends_on_card_id: &str,
    ) -> Result<bool, DbError>;

    async fn link(&mut self, user_id: &str, card_id: &str, conversation_id: &str)
    -> Result<Option<BoardLinkRow>, DbError>;

    async fn insert_link(&mut self, row: &BoardLinkRow) -> Result<(), DbError>;

    async fn question(
        &mut self,
        user_id: &str,
        card_id: &str,
        question_id: &str,
    ) -> Result<Option<BoardQuestionRow>, DbError>;

    /// Write `answered_by_event`, `version`, `updated_at` when the stored
    /// version equals `expected_version`.
    async fn update_question(&mut self, row: &BoardQuestionRow, expected_version: i64) -> Result<bool, DbError>;

    async fn reviews(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardReviewRow>, DbError>;

    /// Insert (`expected_version = None`) or update the current verdict of a
    /// department. Returns `false` on a version mismatch.
    async fn upsert_review(&mut self, row: &BoardReviewRow, expected_version: Option<i64>) -> Result<bool, DbError>;

    async fn questions(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardQuestionRow>, DbError>;

    async fn insert_question(&mut self, row: &BoardQuestionRow) -> Result<(), DbError>;

    async fn links(&mut self, user_id: &str, card_id: &str) -> Result<Vec<BoardLinkRow>, DbError>;

    async fn delete_link(&mut self, user_id: &str, card_id: &str, conversation_id: &str) -> Result<bool, DbError>;

    async fn idempotency(
        &mut self,
        user_id: &str,
        project_id: &str,
        key: &str,
    ) -> Result<Option<BoardIdempotencyRow>, DbError>;

    async fn insert_idempotency(&mut self, row: &BoardIdempotencyRow) -> Result<(), DbError>;

    /// Write `state`, `assignment_id`, `response`, `packet_message_id`,
    /// `updated_at` when the stored `claim_token` equals `claim_token`
    /// (fencing). Returns `false` when the claim was lost.
    async fn update_idempotency(&mut self, row: &BoardIdempotencyRow, claim_token: &str) -> Result<bool, DbError>;

    /// Take over a stale `pending` reservation: issue `new_token` when
    /// `updated_at < stale_before`. Returns `false` when it is still live.
    async fn take_over_idempotency(
        &mut self,
        user_id: &str,
        project_id: &str,
        key: &str,
        new_token: &str,
        stale_before: i64,
        now: i64,
    ) -> Result<bool, DbError>;

    /// The card's deploy job that is not terminal (`requested|running`).
    async fn active_deploy_job(&mut self, user_id: &str, card_id: &str) -> Result<Option<BoardDeployJobRow>, DbError>;

    async fn update_deploy_job_detail(&mut self, user_id: &str, job_id: &str, detail: &str, now: i64)
    -> Result<(), DbError>;

    /// Highest event `seq` of a project (0 when none).
    async fn last_seq(&mut self, user_id: &str, project_id: &str) -> Result<i64, DbError>;

    /// Append an event; returns the assigned `seq`.
    async fn insert_event(&mut self, event: &NewBoardEvent) -> Result<i64, DbError>;

    async fn commit(self: Box<Self>) -> Result<(), DbError>;
}
