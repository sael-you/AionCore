//! `aioncore board recover` — copy board writes from a kept database back into
//! the current one after a binary rollback restored an older backup (SPEC §12).
//!
//! Runs on one connection: the kept file is ATTACHed read-only, then every
//! step runs inside one `BEGIN IMMEDIATE` transaction (rolled back on a dry
//! run, so a dry run reports exactly what a real run would do).
//!
//! Rules, in order:
//! - **Restore point** = the highest current `seq` whose event `id` also
//!   exists in the kept DB (clock-free). An entity is *touched* when a current
//!   event with `seq` > restore point references it; the touched set is
//!   computed before anything is inserted, and events written by recover
//!   itself never count.
//! - **Events** absent by `id` are appended in kept-`seq` order with a new
//!   `seq`, chained to their project (`prev_seq`, `last_event_seq`); the
//!   payload gains `recovered_from_seq`.
//! - **Settings/columns, cards, reviews, questions** absent by id are
//!   inserted; present in both, the kept row is applied only when its
//!   `version` is higher and the entity is untouched — a higher kept version
//!   on a touched entity is a conflict (never silently merged). A card whose
//!   `number` is taken gets the next free number and a `card_renumbered`
//!   event.
//! - **Links/deps** are replayed from the copied `*_added`/`*_removed` events
//!   as set operations, same untouched rule; a dep that would close a cycle
//!   is a conflict.
//! - `next_number` is raised to `max(number) + 1`; `gate_*` events and deploy
//!   jobs are listed, never re-executed; the supervisor cursor is advanced to
//!   the highest inserted `seq` so the supervisor never acts on recovered
//!   events.
//!
//! Idempotent: a second run finds every kept event id present and every kept
//! version applied.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use aionui_common::{generate_id, now_ms};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{BoardEventRow, BoardSettingsRow};
use crate::repository::board::NewBoardEvent;
use crate::repository::sqlite_board::{CARD_COLS, COLUMN_COLS, EVENT_COLS, QUESTION_COLS, REVIEW_COLS, SETTINGS_COLS};
use crate::repository::sqlite_board_tx::insert_chained_event;

/// Actor recorded on the events recover writes itself.
const RECOVER_ACTOR: &str = "recover";
/// Payload marker of every event recover writes itself (`card_renumbered`).
const RECOVER_MARKER: &str = "recovered_by";
/// Event kinds that change the settings/columns entity of a project.
const SETTINGS_EVENT_KINDS: [&str; 5] = [
    "board_created",
    "settings_changed",
    "columns_changed",
    "board_archived",
    "board_unarchived",
];
/// A kept row belongs to a project this DB knows (alias `k`).
const KNOWN_PROJECT: &str =
    "EXISTS (SELECT 1 FROM main.projects p WHERE p.project_id = k.project_id AND p.user_id = k.user_id)";
/// An event written by recover itself (alias `m`).
const RECOVERED_EVENT: &str = "(json_extract(m.payload, '$.recovered_from_seq') IS NOT NULL \
     OR json_extract(m.payload, '$.recovered_by') IS NOT NULL)";

/// Tables the kept DB must have for a recovery to make sense.
const REQUIRED_KEPT_TABLES: [&str; 8] = [
    "board_settings",
    "board_columns",
    "board_cards",
    "board_card_reviews",
    "board_card_questions",
    "board_card_links",
    "board_card_deps",
    "board_events",
];

/// One pair that recover refused to merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardRecoverConflict {
    /// `settings` | `card` | `review` | `question` | `link` | `dep`.
    pub entity: String,
    pub key: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<i64>,
}

/// A card that received a new number because its kept number was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardRecoverRenumber {
    pub card_id: String,
    pub project_id: String,
    pub old: i64,
    pub new: i64,
}

/// A kept event listed for the owner instead of being re-executed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardRecoverListedEvent {
    pub id: String,
    pub kind: String,
    pub card_id: Option<String>,
    pub kept_seq: i64,
}

/// The supervisor checkpoint recover moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardRecoverCursor {
    pub project_id: String,
    pub last_seq: i64,
}

/// What a recovery did (or, on a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BoardRecoverReport {
    pub dry_run: bool,
    /// `false` when the kept DB has no board tables (nothing to recover).
    pub kept_has_board: bool,
    pub restore_point_seq: i64,
    pub events_inserted: u64,
    /// Highest `seq` recover inserted (0 when nothing was inserted).
    pub max_inserted_seq: i64,
    pub settings_inserted: u64,
    pub settings_updated: u64,
    pub cards_inserted: u64,
    pub cards_updated: u64,
    pub cards_renumbered: Vec<BoardRecoverRenumber>,
    pub reviews_inserted: u64,
    pub reviews_updated: u64,
    pub questions_inserted: u64,
    pub questions_updated: u64,
    pub links_added: u64,
    pub links_removed: u64,
    pub deps_added: u64,
    pub deps_removed: u64,
    /// Links whose conversation does not exist here (shown as "removed").
    pub links_to_missing_conversations: Vec<String>,
    pub conflicts: Vec<BoardRecoverConflict>,
    /// Kept `gate_*` / `deploy_forced` events copied as history only.
    pub gate_events: Vec<BoardRecoverListedEvent>,
    /// Kept deploy jobs absent here — listed, never re-created or re-run.
    pub deploy_jobs: Vec<String>,
    pub cursors: Vec<BoardRecoverCursor>,
    /// Kept projects that do not exist here: their rows are skipped.
    pub skipped_projects: Vec<String>,
    /// `(project_id, user_id)` of every board recover appended events to.
    #[serde(skip)]
    affected_projects: BTreeSet<(String, String)>,
}

/// Entities referenced by current events after the restore point.
#[derive(Default)]
struct TouchedSet {
    settings: HashSet<String>,
    cards: HashSet<String>,
    reviews: HashSet<String>,
    questions: HashSet<String>,
    links: HashSet<(String, String)>,
    deps: HashSet<(String, String)>,
}

/// Build the read-only SQLite URI for the kept file. `ATTACH` accepts URI
/// filenames because sqlx opens connections with `SQLITE_OPEN_URI`.
fn read_only_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut encoded = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '%' => encoded.push_str("%25"),
            '?' => encoded.push_str("%3F"),
            '#' => encoded.push_str("%23"),
            ' ' => encoded.push_str("%20"),
            other => encoded.push(other),
        }
    }
    format!("file:{encoded}?mode=ro")
}

/// Recover board writes from `kept_path` into the database behind `pool`.
///
/// The caller guarantees AionUi is stopped (no other writer). `dry_run`
/// performs every step and rolls back.
pub async fn recover_board(pool: &SqlitePool, kept_path: &Path, dry_run: bool) -> Result<BoardRecoverReport, DbError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("ATTACH DATABASE ? AS kept")
        .bind(read_only_uri(kept_path))
        .execute(&mut *conn)
        .await?;
    let result = recover_attached(&mut conn, dry_run).await;
    // Detach on every path so the pooled connection goes back clean.
    let detach = sqlx::query("DETACH DATABASE kept").execute(&mut *conn).await;
    let report = result?;
    detach?;
    Ok(report)
}

async fn recover_attached(conn: &mut SqliteConnection, dry_run: bool) -> Result<BoardRecoverReport, DbError> {
    let mut report = BoardRecoverReport {
        dry_run,
        ..Default::default()
    };
    let present: i64 = {
        let mut builder = sqlx::QueryBuilder::<Sqlite>::new(
            "SELECT COUNT(*) FROM kept.sqlite_master WHERE type = 'table' AND name IN (",
        );
        let mut separated = builder.separated(", ");
        for table in REQUIRED_KEPT_TABLES {
            separated.push_bind(table);
        }
        builder.push(")");
        builder.build_query_scalar().fetch_one(&mut *conn).await?
    };
    if present != REQUIRED_KEPT_TABLES.len() as i64 {
        return Ok(report);
    }
    report.kept_has_board = true;

    let mut tx = sqlx::Connection::begin_with(&mut *conn, "BEGIN IMMEDIATE").await?;
    // Events recover copied on an earlier run share their id with the kept DB
    // but are not shared history: they never move the restore point.
    let restore_point: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT MAX(m.seq) FROM main.board_events m \
         WHERE m.id IN (SELECT id FROM kept.board_events) AND NOT {RECOVERED_EVENT}"
    ))
    .fetch_one(&mut *tx)
    .await?;
    report.restore_point_seq = restore_point.unwrap_or(0);
    let touched = touched_set(&mut tx, report.restore_point_seq).await?;

    report.skipped_projects = sqlx::query_scalar(&format!(
        "SELECT DISTINCT k.project_id FROM (\
             SELECT project_id, user_id FROM kept.board_settings \
             UNION SELECT project_id, user_id FROM kept.board_cards \
             UNION SELECT project_id, user_id FROM kept.board_events\
         ) k WHERE NOT {KNOWN_PROJECT} ORDER BY k.project_id"
    ))
    .fetch_all(&mut *tx)
    .await?;
    let first_inserted_seq: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) + 1 FROM main.board_events")
        .fetch_one(&mut *tx)
        .await?;
    let inserted = copy_events(&mut tx, &mut report).await?;
    recover_settings(&mut tx, &touched, &mut report).await?;
    recover_cards(&mut tx, &touched, &mut report).await?;
    recover_reviews(&mut tx, &touched, &mut report).await?;
    recover_questions(&mut tx, &touched, &mut report).await?;
    replay_set_operations(&mut tx, &inserted, &touched, &mut report).await?;
    raise_next_numbers(&mut tx).await?;
    list_deploy_jobs(&mut tx, &mut report).await?;
    tag_conflicts(&mut tx, &report, first_inserted_seq).await?;
    advance_cursors(&mut tx, &mut report).await?;

    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(report)
}

fn payload_str<'a>(payload: &'a Value, paths: &[&[&str]]) -> Option<&'a str> {
    paths.iter().find_map(|path| {
        path.iter()
            .try_fold(payload, |value, key| value.get(*key))
            .and_then(Value::as_str)
    })
}

async fn touched_set(tx: &mut Transaction<'_, Sqlite>, restore_point: i64) -> Result<TouchedSet, DbError> {
    let rows: Vec<(Option<String>, String, String, String)> = sqlx::query_as(
        "SELECT card_id, project_id, kind, payload FROM main.board_events WHERE seq > ? ORDER BY seq ASC",
    )
    .bind(restore_point)
    .fetch_all(&mut **tx)
    .await?;
    let mut touched = TouchedSet::default();
    for (card_id, project_id, kind, payload) in rows {
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        if payload.get("recovered_from_seq").is_some() || payload.get(RECOVER_MARKER).is_some() {
            continue;
        }
        if SETTINGS_EVENT_KINDS.contains(&kind.as_str()) {
            touched.settings.insert(project_id.clone());
        }
        if let Some(review) = payload_str(&payload, &[&["review", "id"]]) {
            touched.reviews.insert(review.to_owned());
        }
        if let Some(question) = payload_str(&payload, &[&["question", "id"]]) {
            touched.questions.insert(question.to_owned());
        }
        if let Some(card_id) = card_id {
            if let Some(conversation) = payload_str(&payload, &[&["link", "conversation_id"], &["conversation_id"]])
                && (kind.starts_with("link_") || kind == "card_assigned")
            {
                touched.links.insert((card_id.clone(), conversation.to_owned()));
            }
            if let Some(target) = payload_str(&payload, &[&["dep", "depends_on_card_id"], &["depends_on_card_id"]])
                && kind.starts_with("dep_")
            {
                touched.deps.insert((card_id.clone(), target.to_owned()));
            }
            touched.cards.insert(card_id);
        }
    }
    Ok(touched)
}

/// Append every kept event absent by id; returns them with their kept seq.
async fn copy_events(
    tx: &mut Transaction<'_, Sqlite>,
    report: &mut BoardRecoverReport,
) -> Result<Vec<BoardEventRow>, DbError> {
    let kept: Vec<BoardEventRow> = sqlx::query_as(&format!(
        "SELECT {EVENT_COLS} FROM kept.board_events k \
         WHERE {KNOWN_PROJECT} AND NOT EXISTS (SELECT 1 FROM main.board_events m WHERE m.id = k.id) ORDER BY k.seq ASC"
    ))
    .fetch_all(&mut **tx)
    .await?;
    for event in &kept {
        let mut payload: Value = serde_json::from_str(&event.payload).unwrap_or_else(|_| json!({}));
        if !payload.is_object() {
            payload = json!({ "value": payload });
        }
        payload["recovered_from_seq"] = json!(event.seq);
        let seq = insert_chained_event(
            tx,
            "main",
            &NewBoardEvent {
                id: event.id.clone(),
                card_id: event.card_id.clone(),
                project_id: event.project_id.clone(),
                user_id: event.user_id.clone(),
                actor_kind: event.actor_kind.clone(),
                actor_ref: event.actor_ref.clone(),
                conversation_id: event.conversation_id.clone(),
                kind: event.kind.clone(),
                payload: payload.to_string(),
                created_at: event.created_at,
            },
        )
        .await?
        .seq;
        report.events_inserted += 1;
        report.max_inserted_seq = report.max_inserted_seq.max(seq);
        report
            .affected_projects
            .insert((event.project_id.clone(), event.user_id.clone()));
        if event.kind.starts_with("gate_") || event.kind == "deploy_forced" {
            report.gate_events.push(BoardRecoverListedEvent {
                id: event.id.clone(),
                kind: event.kind.clone(),
                card_id: event.card_id.clone(),
                kept_seq: event.seq,
            });
        }
    }
    Ok(kept)
}

fn conflict(entity: &str, key: &str, reason: &str, kept: Option<i64>, current: Option<i64>) -> BoardRecoverConflict {
    BoardRecoverConflict {
        entity: entity.to_owned(),
        key: key.to_owned(),
        reason: reason.to_owned(),
        kept_version: kept,
        current_version: current,
    }
}

async fn recover_settings(
    tx: &mut Transaction<'_, Sqlite>,
    touched: &TouchedSet,
    report: &mut BoardRecoverReport,
) -> Result<(), DbError> {
    let kept: Vec<BoardSettingsRow> = sqlx::query_as(&format!(
        "SELECT {SETTINGS_COLS} FROM kept.board_settings k WHERE {KNOWN_PROJECT}"
    ))
    .fetch_all(&mut **tx)
    .await?;
    for row in kept {
        let current: Option<i64> = sqlx::query_scalar("SELECT version FROM main.board_settings WHERE project_id = ?")
            .bind(&row.project_id)
            .fetch_optional(&mut **tx)
            .await?;
        match current {
            None => {
                sqlx::query(&format!(
                    "INSERT INTO main.board_settings ({SETTINGS_COLS}) \
                     SELECT {SETTINGS_COLS} FROM kept.board_settings WHERE project_id = ?"
                ))
                .bind(&row.project_id)
                .execute(&mut **tx)
                .await?;
                copy_columns(tx, &row.project_id).await?;
                report.settings_inserted += 1;
            }
            Some(version) if row.version > version => {
                if touched.settings.contains(&row.project_id) {
                    report.conflicts.push(conflict(
                        "settings",
                        &row.project_id,
                        "changed here after the restore point",
                        Some(row.version),
                        Some(version),
                    ));
                    continue;
                }
                sqlx::query(
                    "UPDATE main.board_settings SET (enabled, template, version, settings, archived_at, \
                     keeper_conversation_id, updated_at) = \
                     (SELECT enabled, template, version, settings, archived_at, keeper_conversation_id, updated_at \
                      FROM kept.board_settings WHERE project_id = ?) WHERE project_id = ?",
                )
                .bind(&row.project_id)
                .bind(&row.project_id)
                .execute(&mut **tx)
                .await?;
                copy_columns(tx, &row.project_id).await?;
                report.settings_updated += 1;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Make the project's columns equal the kept collection (upsert by key).
async fn copy_columns(tx: &mut Transaction<'_, Sqlite>, project_id: &str) -> Result<(), DbError> {
    sqlx::query(&format!(
        "INSERT INTO main.board_columns ({COLUMN_COLS}) \
         SELECT {COLUMN_COLS} FROM kept.board_columns WHERE project_id = ? \
         ON CONFLICT (project_id, key) DO UPDATE SET \
            name = excluded.name, category = excluded.category, kind = excluded.kind, \
            resolution = excluded.resolution, agent_move = excluded.agent_move, wip_limit = excluded.wip_limit, \
            exit_rule = excluded.exit_rule, gate = excluded.gate, position = excluded.position, \
            archived = excluded.archived"
    ))
    .bind(project_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Card columns that a kept snapshot overwrites (identity, owner, number and
/// creation stay as they are here).
const CARD_MUTABLE_COLS: &str = "title, description, type, priority, size, tier, column_key, position, labels, \
     skills, routing, assignee_kind, assignee_ref, assignee_conversation_id, blocked, blocked_reason, blocked_by, \
     change_id, metadata, version, archived_at, updated_at";

async fn recover_cards(
    tx: &mut Transaction<'_, Sqlite>,
    touched: &TouchedSet,
    report: &mut BoardRecoverReport,
) -> Result<(), DbError> {
    // A kept file written before 909 has no `assignee_conversation_id`: read NULL for it.
    let kept_has_conversation: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('board_cards', 'kept') WHERE name = 'assignee_conversation_id'",
    )
    .fetch_one(&mut **tx)
    .await?;
    let from_kept = |cols: &str| {
        if kept_has_conversation > 0 {
            cols.to_owned()
        } else {
            cols.replacen("assignee_conversation_id", "NULL", 1)
        }
    };
    let kept: Vec<(String, String, String, i64, i64)> = sqlx::query_as(&format!(
        "SELECT id, project_id, user_id, number, version FROM kept.board_cards k WHERE {KNOWN_PROJECT} \
             ORDER BY project_id, number"
    ))
    .fetch_all(&mut **tx)
    .await?;
    for (card_id, project_id, user_id, number, kept_version) in kept {
        let current: Option<i64> = sqlx::query_scalar("SELECT version FROM main.board_cards WHERE id = ?")
            .bind(&card_id)
            .fetch_optional(&mut **tx)
            .await?;
        match current {
            None => {
                let taken: i64 = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM main.board_cards WHERE project_id = ? AND number = ?)",
                )
                .bind(&project_id)
                .bind(number)
                .fetch_one(&mut **tx)
                .await?;
                let final_number = if taken != 0 {
                    next_free_number(tx, &project_id).await?
                } else {
                    number
                };
                let select_cols = from_kept(&CARD_COLS.replacen("number", "? AS number", 1));
                sqlx::query(&format!(
                    "INSERT INTO main.board_cards ({CARD_COLS}) SELECT {select_cols} FROM kept.board_cards WHERE id = ?"
                ))
                .bind(final_number)
                .bind(&card_id)
                .execute(&mut **tx)
                .await?;
                report.cards_inserted += 1;
                if final_number != number {
                    renumbered_event(tx, &card_id, &project_id, &user_id, number, final_number, report).await?;
                }
            }
            Some(version) if kept_version > version => {
                if touched.cards.contains(&card_id) {
                    report.conflicts.push(conflict(
                        "card",
                        &card_id,
                        "changed here after the restore point",
                        Some(kept_version),
                        Some(version),
                    ));
                    continue;
                }
                let kept_cols = from_kept(CARD_MUTABLE_COLS);
                sqlx::query(&format!(
                    "UPDATE main.board_cards SET ({CARD_MUTABLE_COLS}) = \
                     (SELECT {kept_cols} FROM kept.board_cards WHERE id = ?) WHERE id = ?"
                ))
                .bind(&card_id)
                .bind(&card_id)
                .execute(&mut **tx)
                .await?;
                report.cards_updated += 1;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

async fn next_free_number(tx: &mut Transaction<'_, Sqlite>, project_id: &str) -> Result<i64, DbError> {
    let next: i64 = sqlx::query_scalar(
        "SELECT MAX(COALESCE((SELECT MAX(number) FROM main.board_cards WHERE project_id = ?), 0) + 1, \
                    COALESCE((SELECT next_number FROM main.board_settings WHERE project_id = ?), 1))",
    )
    .bind(project_id)
    .bind(project_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(next)
}

async fn renumbered_event(
    tx: &mut Transaction<'_, Sqlite>,
    card_id: &str,
    project_id: &str,
    user_id: &str,
    old: i64,
    new: i64,
    report: &mut BoardRecoverReport,
) -> Result<(), DbError> {
    let payload = json!({ "card_id": card_id, "old": old, "new": new, RECOVER_MARKER: "board recover" });
    let seq = insert_chained_event(
        tx,
        "main",
        &NewBoardEvent {
            id: generate_id(),
            card_id: Some(card_id.to_owned()),
            project_id: project_id.to_owned(),
            user_id: user_id.to_owned(),
            actor_kind: "system".to_owned(),
            actor_ref: RECOVER_ACTOR.to_owned(),
            conversation_id: None,
            kind: "card_renumbered".to_owned(),
            payload: payload.to_string(),
            created_at: now_ms(),
        },
    )
    .await?
    .seq;
    report.events_inserted += 1;
    report.max_inserted_seq = report.max_inserted_seq.max(seq);
    report
        .affected_projects
        .insert((project_id.to_owned(), user_id.to_owned()));
    report.cards_renumbered.push(BoardRecoverRenumber {
        card_id: card_id.to_owned(),
        project_id: project_id.to_owned(),
        old,
        new,
    });
    Ok(())
}

/// Shared shape of the review and question recovery (both versioned rows of
/// a card, keyed by id).
struct VersionedTable {
    entity: &'static str,
    table: &'static str,
    cols: &'static str,
    mutable_cols: &'static str,
    /// Extra uniqueness check for an insert (reviews: one per department).
    duplicate_check: Option<&'static str>,
}

const REVIEWS: VersionedTable = VersionedTable {
    entity: "review",
    table: "board_card_reviews",
    cols: REVIEW_COLS,
    mutable_cols: "reviewer_ref, conversation_id, verdict, summary, findings, recommendation, assumptions, \
                   confidence, tokens_used, version, updated_at",
    duplicate_check: Some(
        "SELECT EXISTS(SELECT 1 FROM main.board_card_reviews m JOIN kept.board_card_reviews k \
         ON k.card_id = m.card_id AND k.department = m.department WHERE k.id = ?)",
    ),
};

const QUESTIONS: VersionedTable = VersionedTable {
    entity: "question",
    table: "board_card_questions",
    cols: QUESTION_COLS,
    mutable_cols: "answered_by_event, version, updated_at",
    duplicate_check: None,
};

async fn recover_versioned(
    tx: &mut Transaction<'_, Sqlite>,
    spec: &VersionedTable,
    touched: &HashSet<String>,
    report: &mut BoardRecoverReport,
) -> Result<(u64, u64), DbError> {
    let table = spec.table;
    let kept: Vec<(String, String, i64)> = sqlx::query_as(&format!(
        "SELECT id, card_id, version FROM kept.{table} k WHERE {KNOWN_PROJECT} ORDER BY created_at, id"
    ))
    .fetch_all(&mut **tx)
    .await?;
    let (mut inserted, mut updated) = (0u64, 0u64);
    for (id, card_id, kept_version) in kept {
        let current: Option<i64> = sqlx::query_scalar(&format!("SELECT version FROM main.{table} WHERE id = ?"))
            .bind(&id)
            .fetch_optional(&mut **tx)
            .await?;
        match current {
            None => {
                let card_exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM main.board_cards WHERE id = ?)")
                    .bind(&card_id)
                    .fetch_one(&mut **tx)
                    .await?;
                if card_exists == 0 {
                    report.conflicts.push(conflict(
                        spec.entity,
                        &id,
                        "its card does not exist here",
                        Some(kept_version),
                        None,
                    ));
                    continue;
                }
                if let Some(check) = spec.duplicate_check {
                    let duplicate: i64 = sqlx::query_scalar(check).bind(&id).fetch_one(&mut **tx).await?;
                    if duplicate != 0 {
                        report.conflicts.push(conflict(
                            spec.entity,
                            &id,
                            "another current row holds the same department",
                            Some(kept_version),
                            None,
                        ));
                        continue;
                    }
                }
                let cols = spec.cols;
                sqlx::query(&format!(
                    "INSERT INTO main.{table} ({cols}) SELECT {cols} FROM kept.{table} WHERE id = ?"
                ))
                .bind(&id)
                .execute(&mut **tx)
                .await?;
                inserted += 1;
            }
            Some(version) if kept_version > version => {
                if touched.contains(&id) {
                    report.conflicts.push(conflict(
                        spec.entity,
                        &id,
                        "changed here after the restore point",
                        Some(kept_version),
                        Some(version),
                    ));
                    continue;
                }
                let mutable = spec.mutable_cols;
                sqlx::query(&format!(
                    "UPDATE main.{table} SET ({mutable}) = (SELECT {mutable} FROM kept.{table} WHERE id = ?) \
                     WHERE id = ?"
                ))
                .bind(&id)
                .bind(&id)
                .execute(&mut **tx)
                .await?;
                updated += 1;
            }
            Some(_) => {}
        }
    }
    Ok((inserted, updated))
}

async fn recover_reviews(
    tx: &mut Transaction<'_, Sqlite>,
    touched: &TouchedSet,
    report: &mut BoardRecoverReport,
) -> Result<(), DbError> {
    let (inserted, updated) = recover_versioned(tx, &REVIEWS, &touched.reviews, report).await?;
    report.reviews_inserted += inserted;
    report.reviews_updated += updated;
    Ok(())
}

async fn recover_questions(
    tx: &mut Transaction<'_, Sqlite>,
    touched: &TouchedSet,
    report: &mut BoardRecoverReport,
) -> Result<(), DbError> {
    let (inserted, updated) = recover_versioned(tx, &QUESTIONS, &touched.questions, report).await?;
    report.questions_inserted += inserted;
    report.questions_updated += updated;
    Ok(())
}

/// Replay copied link/dep events as set operations, in kept-seq order.
async fn replay_set_operations(
    tx: &mut Transaction<'_, Sqlite>,
    copied: &[BoardEventRow],
    touched: &TouchedSet,
    report: &mut BoardRecoverReport,
) -> Result<(), DbError> {
    for event in copied {
        let Some(card_id) = event.card_id.as_deref() else {
            continue;
        };
        let payload: Value = serde_json::from_str(&event.payload).unwrap_or(Value::Null);
        match event.kind.as_str() {
            // An assignment's link rides on its `card_assigned` event.
            "link_added" | "link_removed" | "card_assigned" => {
                let Some(conversation_id) =
                    payload_str(&payload, &[&["link", "conversation_id"], &["conversation_id"]])
                else {
                    continue;
                };
                let key = (card_id.to_owned(), conversation_id.to_owned());
                if touched.links.contains(&key) {
                    report.conflicts.push(conflict(
                        "link",
                        &format!("{card_id}:{conversation_id}"),
                        "changed here after the restore point",
                        None,
                        None,
                    ));
                    continue;
                }
                if event.kind != "link_removed" {
                    replay_link_added(tx, event, card_id, conversation_id, &payload, report).await?;
                } else {
                    let removed =
                        sqlx::query("DELETE FROM main.board_card_links WHERE card_id = ? AND conversation_id = ?")
                            .bind(card_id)
                            .bind(conversation_id)
                            .execute(&mut **tx)
                            .await?
                            .rows_affected();
                    report.links_removed += removed;
                }
            }
            "dep_added" | "dep_removed" => {
                let Some(target) = payload_str(&payload, &[&["dep", "depends_on_card_id"], &["depends_on_card_id"]])
                else {
                    continue;
                };
                let key = (card_id.to_owned(), target.to_owned());
                if touched.deps.contains(&key) {
                    report.conflicts.push(conflict(
                        "dep",
                        &format!("{card_id}:{target}"),
                        "changed here after the restore point",
                        None,
                        None,
                    ));
                    continue;
                }
                if event.kind == "dep_added" {
                    replay_dep_added(tx, event, card_id, target, report).await?;
                } else {
                    let removed =
                        sqlx::query("DELETE FROM main.board_card_deps WHERE card_id = ? AND depends_on_card_id = ?")
                            .bind(card_id)
                            .bind(target)
                            .execute(&mut **tx)
                            .await?
                            .rows_affected();
                    report.deps_removed += removed;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn replay_link_added(
    tx: &mut Transaction<'_, Sqlite>,
    event: &BoardEventRow,
    card_id: &str,
    conversation_id: &str,
    payload: &Value,
    report: &mut BoardRecoverReport,
) -> Result<(), DbError> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM main.board_card_links WHERE card_id = ? AND conversation_id = ?)",
    )
    .bind(card_id)
    .bind(conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    if exists != 0 {
        return Ok(());
    }
    let role = payload_str(payload, &[&["link", "role"]]).unwrap_or("other");
    if role == "lead" {
        let lead: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM main.board_card_links WHERE card_id = ? AND role = 'lead')",
        )
        .bind(card_id)
        .fetch_one(&mut **tx)
        .await?;
        if lead != 0 {
            report.conflicts.push(conflict(
                "link",
                &format!("{card_id}:{conversation_id}"),
                "the card already has a lead link here",
                None,
                None,
            ));
            return Ok(());
        }
    }
    let department = payload_str(payload, &[&["link", "department"]]);
    let created_at = payload
        .get("link")
        .and_then(|link| link.get("created_at"))
        .and_then(Value::as_i64)
        .unwrap_or(event.created_at);
    sqlx::query(
        "INSERT INTO main.board_card_links (card_id, conversation_id, project_id, user_id, role, department, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(card_id)
    .bind(conversation_id)
    .bind(&event.project_id)
    .bind(&event.user_id)
    .bind(role)
    .bind(department)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    report.links_added += 1;
    let conversation_exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM main.conversations WHERE id = ?)")
        .bind(conversation_id)
        .fetch_one(&mut **tx)
        .await?;
    if conversation_exists == 0 {
        report.links_to_missing_conversations.push(conversation_id.to_owned());
    }
    Ok(())
}

async fn replay_dep_added(
    tx: &mut Transaction<'_, Sqlite>,
    event: &BoardEventRow,
    card_id: &str,
    target: &str,
    report: &mut BoardRecoverReport,
) -> Result<(), DbError> {
    let key = format!("{card_id}:{target}");
    let both: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM main.board_cards WHERE id IN (?, ?) AND project_id = ?")
        .bind(card_id)
        .bind(target)
        .bind(&event.project_id)
        .fetch_one(&mut **tx)
        .await?;
    if both != 2 {
        report.conflicts.push(conflict(
            "dep",
            &key,
            "a card of the dependency does not exist here",
            None,
            None,
        ));
        return Ok(());
    }
    let edges: Vec<(String, String)> =
        sqlx::query_as("SELECT card_id, depends_on_card_id FROM main.board_card_deps WHERE project_id = ?")
            .bind(&event.project_id)
            .fetch_all(&mut **tx)
            .await?;
    if edges.iter().any(|(from, to)| from == card_id && to == target) {
        return Ok(());
    }
    if reaches(&edges, target, card_id) {
        report
            .conflicts
            .push(conflict("dep", &key, "it would create a dependency cycle", None, None));
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO main.board_card_deps (card_id, depends_on_card_id, project_id, user_id, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(card_id)
    .bind(target)
    .bind(&event.project_id)
    .bind(&event.user_id)
    .bind(event.created_at)
    .execute(&mut **tx)
    .await?;
    report.deps_added += 1;
    Ok(())
}

/// Whether `from` reaches `to` along `card → depends_on` edges.
fn reaches(edges: &[(String, String)], from: &str, to: &str) -> bool {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for (card, target) in edges {
        adjacency.entry(card.as_str()).or_default().push(target.as_str());
    }
    let mut stack = vec![from];
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if seen.insert(node)
            && let Some(next) = adjacency.get(node)
        {
            stack.extend(next.iter().copied());
        }
    }
    false
}

/// Mark the events this run copied that belong to a conflicted entity
/// (`recover_conflict: true`), so the owner sees which history did not apply.
async fn tag_conflicts(
    tx: &mut Transaction<'_, Sqlite>,
    report: &BoardRecoverReport,
    first_inserted_seq: i64,
) -> Result<(), DbError> {
    for conflict in &report.conflicts {
        let (filter, keys): (&str, Vec<String>) = match conflict.entity.as_str() {
            "card" => ("card_id = ?", vec![conflict.key.clone()]),
            "review" => ("json_extract(payload, '$.review.id') = ?", vec![conflict.key.clone()]),
            "question" => ("json_extract(payload, '$.question.id') = ?", vec![conflict.key.clone()]),
            "settings" => (
                "project_id = ? AND kind IN ('board_created', 'settings_changed', 'columns_changed', \
                 'board_archived', 'board_unarchived')",
                vec![conflict.key.clone()],
            ),
            "link" | "dep" => match conflict.key.split_once(':') {
                Some((card, other)) => (
                    "card_id = ? AND (COALESCE(json_extract(payload, '$.link.conversation_id'), \
                     json_extract(payload, '$.conversation_id'), json_extract(payload, '$.dep.depends_on_card_id'), \
                     json_extract(payload, '$.depends_on_card_id')) = ?)",
                    vec![card.to_owned(), other.to_owned()],
                ),
                None => continue,
            },
            _ => continue,
        };
        let sql = format!(
            "UPDATE main.board_events SET payload = json_set(payload, '$.recover_conflict', json('true')) \
             WHERE seq >= ? AND json_extract(payload, '$.recovered_from_seq') IS NOT NULL AND {filter}"
        );
        let mut query = sqlx::query(&sql).bind(first_inserted_seq);
        for key in keys {
            query = query.bind(key);
        }
        query.execute(&mut **tx).await?;
    }
    Ok(())
}

async fn raise_next_numbers(tx: &mut Transaction<'_, Sqlite>) -> Result<(), DbError> {
    // Settings copied from the kept DB carry the kept chain head; the head is
    // this DB's newest event of the project.
    sqlx::query(
        "UPDATE main.board_settings SET last_event_seq = \
            COALESCE((SELECT MAX(seq) FROM main.board_events e WHERE e.project_id = main.board_settings.project_id), 0)",
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE main.board_settings SET next_number = MAX(next_number, \
            COALESCE((SELECT MAX(number) FROM main.board_cards c WHERE c.project_id = main.board_settings.project_id), 0) + 1)",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn list_deploy_jobs(tx: &mut Transaction<'_, Sqlite>, report: &mut BoardRecoverReport) -> Result<(), DbError> {
    let has_jobs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kept.sqlite_master WHERE type = 'table' AND name = 'board_deploy_jobs'",
    )
    .fetch_one(&mut **tx)
    .await?;
    if has_jobs == 0 {
        return Ok(());
    }
    report.deploy_jobs = sqlx::query_scalar(
        &format!("SELECT k.id FROM kept.board_deploy_jobs k \
         WHERE {KNOWN_PROJECT} AND NOT EXISTS (SELECT 1 FROM main.board_deploy_jobs m WHERE m.id = k.id) ORDER BY k.created_at, k.id"),
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(())
}

/// Move every affected project's supervisor checkpoint to the highest seq
/// recover inserted, so the supervisor never acts on recovered events.
async fn advance_cursors(tx: &mut Transaction<'_, Sqlite>, report: &mut BoardRecoverReport) -> Result<(), DbError> {
    if report.max_inserted_seq == 0 {
        return Ok(());
    }
    let projects: Vec<(String, String)> = report.affected_projects.iter().cloned().collect();
    let now = now_ms();
    for (project_id, user_id) in projects {
        let last_seq: i64 = sqlx::query_scalar(
            "INSERT INTO main.board_supervisor_cursor (project_id, user_id, last_seq, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (project_id) DO UPDATE SET last_seq = MAX(last_seq, excluded.last_seq), \
                updated_at = excluded.updated_at \
             RETURNING last_seq",
        )
        .bind(&project_id)
        .bind(&user_id)
        .bind(report.max_inserted_seq)
        .bind(now)
        .fetch_one(&mut **tx)
        .await?;
        report.cursors.push(BoardRecoverCursor { project_id, last_seq });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_uri_escapes_uri_metacharacters() {
        assert_eq!(
            read_only_uri(Path::new("/tmp/a b?#%.db")),
            "file:/tmp/a%20b%3F%23%25.db?mode=ro"
        );
    }

    #[test]
    fn reachability_follows_dependency_edges() {
        let edges = vec![("a".to_owned(), "b".to_owned()), ("b".to_owned(), "c".to_owned())];
        assert!(reaches(&edges, "a", "c"));
        assert!(!reaches(&edges, "c", "a"));
    }
}
