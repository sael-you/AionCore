//! `board recover` (SPEC §12): inserts, seq collision, number collision,
//! untouched updates, update conflicts, review/question versions, link/dep
//! tombstones, cursor advance, gate/deploy listing, dry run, duplicate runs.

use std::path::Path;

use aionui_db::{Database, init_database, recover_board};
use serde_json::{Value, json};
use sqlx::SqlitePool;

const USER: &str = "system_default_user";
const PROJECT: &str = "proj-r";

async fn open(path: &Path) -> Database {
    init_database(path).await.unwrap()
}

async fn exec(pool: &SqlitePool, sql: &str) {
    sqlx::query(sql).execute(pool).await.unwrap();
}

async fn settings(pool: &SqlitePool, version: i64, next_number: i64) {
    sqlx::query(
        "INSERT INTO board_settings (project_id, user_id, enabled, template, next_number, version, settings, \
         created_at, updated_at) VALUES (?, ?, 1, 'studio', ?, ?, '{}', 1, 1) \
         ON CONFLICT (project_id) DO UPDATE SET version = excluded.version, next_number = excluded.next_number",
    )
    .bind(PROJECT)
    .bind(USER)
    .bind(next_number)
    .bind(version)
    .execute(pool)
    .await
    .unwrap();
    for (position, key) in ["backlog", "todo", "in_progress", "done"].iter().enumerate() {
        sqlx::query(
            "INSERT OR IGNORE INTO board_columns (id, project_id, user_id, key, category, kind, agent_move, position) \
             VALUES (?, ?, ?, ?, 'todo', 'normal', 'allow', ?)",
        )
        .bind(format!("col-{key}"))
        .bind(PROJECT)
        .bind(USER)
        .bind(key)
        .bind(position as i64)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn card(pool: &SqlitePool, id: &str, number: i64, title: &str, version: i64) {
    sqlx::query(
        "INSERT INTO board_cards (id, project_id, user_id, number, title, column_key, position, creator_kind, \
         creator_ref, version, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 'backlog', ?, 'user', ?, ?, 1, 1) \
         ON CONFLICT (id) DO UPDATE SET title = excluded.title, version = excluded.version",
    )
    .bind(id)
    .bind(PROJECT)
    .bind(USER)
    .bind(number)
    .bind(title)
    .bind(number * 1024)
    .bind(USER)
    .bind(version)
    .execute(pool)
    .await
    .unwrap();
}

async fn event(pool: &SqlitePool, id: &str, card_id: Option<&str>, kind: &str, payload: Value) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO board_events (id, card_id, project_id, user_id, actor_kind, actor_ref, kind, payload, created_at) \
         VALUES (?, ?, ?, ?, 'user', ?, ?, ?, 1) RETURNING seq",
    )
    .bind(id)
    .bind(card_id)
    .bind(PROJECT)
    .bind(USER)
    .bind(USER)
    .bind(kind)
    .bind(payload.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn scalar_i64(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn scalar_str(pool: &SqlitePool, sql: &str) -> String {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// Shared history (both DBs), then kept-only writes after the backup, then
/// current-only writes after the restore.
struct Fixture {
    _dir: tempfile::TempDir,
    current: Database,
    kept_path: std::path::PathBuf,
}

async fn shared_history(pool: &SqlitePool) {
    settings(pool, 1, 2).await;
    card(pool, "A", 1, "original", 1).await;
    event(pool, "e1", None, "board_created", json!({ "version": 1 })).await;
    event(pool, "e2", Some("A"), "card_created", json!({ "version": 1 })).await;
    // A link and a dependency target that the kept DB later removes.
    exec(
        pool,
        "INSERT INTO board_card_links (card_id, conversation_id, project_id, user_id, role, created_at) \
         VALUES ('A', 'conv-old', 'proj-r', 'system_default_user', 'other', 1)",
    )
    .await;
    event(
        pool,
        "e2b",
        Some("A"),
        "link_added",
        json!({ "link": { "card_id": "A", "conversation_id": "conv-old", "role": "other" } }),
    )
    .await;
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let kept_path = dir.path().join("backend-replaced-1.db");
    {
        let kept = open(&kept_path).await;
        let pool = kept.pool();
        shared_history(pool).await;
        // Kept-only: A edited (v2), B created as TASK-2, review/question on A,
        // link/dep add + a link tombstone, a gate decision, a deploy job.
        card(pool, "A", 1, "kept edit", 2).await;
        event(pool, "e3", Some("A"), "card_updated", json!({ "version": 2 })).await;
        card(pool, "B", 2, "kept card", 1).await;
        exec(pool, "UPDATE board_settings SET next_number = 3").await;
        event(pool, "e4", Some("B"), "card_created", json!({ "version": 1 })).await;
        exec(
            pool,
            "INSERT INTO board_card_reviews (id, card_id, project_id, user_id, department, reviewer_ref, verdict, \
             version, created_at, updated_at) VALUES ('R1', 'A', 'proj-r', 'system_default_user', 'qa', 'x', \
             'approve', 1, 1, 1)",
        )
        .await;
        exec(
            pool,
            "INSERT INTO board_card_questions (id, card_id, project_id, user_id, from_department, to_department, \
             text, version, created_at, updated_at) VALUES ('Q1', 'A', 'proj-r', 'system_default_user', 'qa', \
             'owner', 'why?', 1, 1, 1)",
        )
        .await;
        event(
            pool,
            "e5",
            Some("A"),
            "link_added",
            json!({ "link": { "card_id": "A", "conversation_id": "conv-gone", "role": "build", "created_at": 5 } }),
        )
        .await;
        event(
            pool,
            "e6",
            Some("B"),
            "dep_added",
            json!({ "dep": { "card_id": "B", "depends_on_card_id": "A", "created_at": 6 } }),
        )
        .await;
        event(
            pool,
            "e7",
            Some("A"),
            "link_removed",
            json!({ "card_id": "A", "conversation_id": "conv-old" }),
        )
        .await;
        event(
            pool,
            "e8",
            Some("B"),
            "gate_approved",
            json!({ "from": "decision", "to": "planned" }),
        )
        .await;
        exec(
            pool,
            "INSERT INTO board_deploy_jobs (id, card_id, project_id, user_id, idempotency_key, state, created_at, \
             updated_at) VALUES ('J1', 'B', 'proj-r', 'system_default_user', 'k1', 'requested', 1, 1)",
        )
        .await;
        kept.close().await;
    }
    let current = open(&dir.path().join("aionui-backend.db")).await;
    shared_history(current.pool()).await;
    Fixture {
        _dir: dir,
        current,
        kept_path,
    }
}

#[tokio::test]
async fn recover_copies_kept_writes_with_new_seqs_and_renumbers_collisions() {
    let fx = fixture().await;
    let pool = fx.current.pool();
    // After the restore the owner created TASK-2 here too: number collision,
    // and its seq (4) collides with a kept event's seq.
    card(pool, "C", 2, "made after restore", 1).await;
    exec(pool, "UPDATE board_settings SET next_number = 3").await;
    let c_seq = event(pool, "e9", Some("C"), "card_created", json!({ "version": 1 })).await;
    assert_eq!(c_seq, 4, "same seq as the kept e3");

    let report = recover_board(pool, &fx.kept_path, false).await.unwrap();
    assert!(report.kept_has_board);
    assert_eq!(report.restore_point_seq, 3, "e2b is the newest shared event");
    assert_eq!(report.events_inserted, 7, "e3–e8 copied + one card_renumbered");
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);

    // Copied events get new seqs after everything current, and keep their id.
    let copied: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT seq, id, payload FROM board_events WHERE id IN ('e3','e4','e8') ORDER BY seq")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(copied.len(), 3);
    assert!(copied.iter().all(|(seq, _, _)| *seq > c_seq));
    let e3: Value = serde_json::from_str(&copied[0].2).unwrap();
    assert_eq!(e3["recovered_from_seq"], 4, "payload records the kept seq");

    // Untouched A: the higher kept version is applied.
    assert_eq!(
        scalar_str(pool, "SELECT title FROM board_cards WHERE id = 'A'").await,
        "kept edit"
    );
    assert_eq!(report.cards_updated, 1);
    // B was absent; TASK-2 is taken by C → B becomes TASK-3 with an event.
    assert_eq!(report.cards_inserted, 1);
    assert_eq!(
        scalar_i64(pool, "SELECT number FROM board_cards WHERE id = 'B'").await,
        3
    );
    assert_eq!(report.cards_renumbered.len(), 1);
    assert_eq!((report.cards_renumbered[0].old, report.cards_renumbered[0].new), (2, 3));
    let renumbered: Value = serde_json::from_str(
        &scalar_str(pool, "SELECT payload FROM board_events WHERE kind = 'card_renumbered'").await,
    )
    .unwrap();
    assert_eq!(
        (renumbered["old"].as_i64(), renumbered["new"].as_i64()),
        (Some(2), Some(3))
    );
    assert_eq!(scalar_i64(pool, "SELECT next_number FROM board_settings").await, 4);

    // Review and question rows absent here are inserted.
    assert_eq!((report.reviews_inserted, report.questions_inserted), (1, 1));
    // Links/deps replayed as set operations, tombstone included.
    assert_eq!(report.links_added, 1);
    assert_eq!(report.links_removed, 1);
    assert_eq!(report.deps_added, 1);
    assert_eq!(report.links_to_missing_conversations, vec!["conv-gone".to_owned()]);
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(*) FROM board_card_links WHERE conversation_id = 'conv-old'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(*) FROM board_card_deps WHERE card_id = 'B' AND depends_on_card_id = 'A'"
        )
        .await,
        1
    );
    // Gate decisions and deploy jobs are listed, never re-executed.
    assert_eq!(report.gate_events.len(), 1);
    assert_eq!(report.gate_events[0].id, "e8");
    assert_eq!(report.deploy_jobs, vec!["J1".to_owned()]);
    assert_eq!(scalar_i64(pool, "SELECT COUNT(*) FROM board_deploy_jobs").await, 0);
    // The supervisor checkpoint skips everything recover inserted.
    assert_eq!(report.cursors.len(), 1);
    assert_eq!(report.cursors[0].last_seq, report.max_inserted_seq);
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT last_seq FROM board_supervisor_cursor WHERE project_id = 'proj-r'"
        )
        .await,
        report.max_inserted_seq
    );
    assert_eq!(
        report.max_inserted_seq,
        scalar_i64(pool, "SELECT MAX(seq) FROM board_events").await
    );
}

#[tokio::test]
async fn a_second_run_is_a_no_op() {
    let fx = fixture().await;
    let pool = fx.current.pool();
    let first = recover_board(pool, &fx.kept_path, false).await.unwrap();
    assert!(first.events_inserted > 0);
    let events = scalar_i64(pool, "SELECT COUNT(*) FROM board_events").await;

    let second = recover_board(pool, &fx.kept_path, false).await.unwrap();
    assert_eq!(second.events_inserted, 0);
    assert_eq!(second.cards_inserted + second.cards_updated, 0);
    assert_eq!(second.reviews_inserted + second.questions_inserted, 0);
    assert_eq!(second.links_added + second.deps_added, 0);
    assert!(second.conflicts.is_empty(), "{:?}", second.conflicts);
    assert!(second.cursors.is_empty(), "nothing inserted → cursor untouched");
    assert_eq!(scalar_i64(pool, "SELECT COUNT(*) FROM board_events").await, events);
}

#[tokio::test]
async fn touched_entities_with_a_higher_kept_version_are_conflicts() {
    let fx = fixture().await;
    let pool = fx.current.pool();
    // After the restore the owner edited A here (v2) and changed settings.
    card(pool, "A", 1, "current edit", 2).await;
    event(pool, "e10", Some("A"), "card_updated", json!({ "version": 2 })).await;
    settings(pool, 2, 2).await;
    event(pool, "e11", None, "settings_changed", json!({ "version": 2 })).await;
    // The kept DB has A at v3 and settings at v3.
    {
        let kept = open(&fx.kept_path).await;
        card(kept.pool(), "A", 1, "kept v3", 3).await;
        exec(kept.pool(), "UPDATE board_settings SET version = 3").await;
        kept.close().await;
    }

    let report = recover_board(pool, &fx.kept_path, false).await.unwrap();
    let entities: Vec<(&str, &str)> = report
        .conflicts
        .iter()
        .map(|c| (c.entity.as_str(), c.key.as_str()))
        .collect();
    assert!(entities.contains(&("card", "A")), "{entities:?}");
    assert!(entities.contains(&("settings", PROJECT)), "{entities:?}");
    let card_conflict = report.conflicts.iter().find(|c| c.entity == "card").unwrap();
    assert_eq!(
        (card_conflict.kept_version, card_conflict.current_version),
        (Some(3), Some(2))
    );
    assert_eq!(
        scalar_str(pool, "SELECT title FROM board_cards WHERE id = 'A'").await,
        "current edit",
        "no silent merge"
    );
    // The untouched, absent B still arrives.
    assert_eq!(report.cards_inserted, 1);
}

#[tokio::test]
async fn review_and_question_versions_follow_the_untouched_rule() {
    let fx = fixture().await;
    let pool = fx.current.pool();
    // Both rows exist here at v1 (shared), the kept DB has them at v2.
    exec(
        pool,
        "INSERT INTO board_card_reviews (id, card_id, project_id, user_id, department, reviewer_ref, verdict, \
         version, created_at, updated_at) VALUES ('R1', 'A', 'proj-r', 'system_default_user', 'qa', 'x', \
         'pending', 1, 1, 1)",
    )
    .await;
    exec(
        pool,
        "INSERT INTO board_card_questions (id, card_id, project_id, user_id, from_department, to_department, text, \
         version, created_at, updated_at) VALUES ('Q1', 'A', 'proj-r', 'system_default_user', 'qa', 'owner', \
         'why?', 1, 1, 1)",
    )
    .await;
    // The question was answered here after the restore (touched).
    event(
        pool,
        "e12",
        Some("A"),
        "commented",
        json!({ "question": { "id": "Q1" } }),
    )
    .await;
    {
        let kept = open(&fx.kept_path).await;
        exec(
            kept.pool(),
            "UPDATE board_card_reviews SET verdict = 'reject', version = 2",
        )
        .await;
        exec(
            kept.pool(),
            "UPDATE board_card_questions SET answered_by_event = 'eX', version = 2",
        )
        .await;
        kept.close().await;
    }
    let report = recover_board(pool, &fx.kept_path, false).await.unwrap();
    assert_eq!(report.reviews_updated, 1, "untouched review takes the kept v2");
    assert_eq!(
        scalar_str(pool, "SELECT verdict FROM board_card_reviews").await,
        "reject"
    );
    assert_eq!(report.questions_updated, 0);
    assert!(
        report.conflicts.iter().any(|c| c.entity == "question" && c.key == "Q1"),
        "{:?}",
        report.conflicts
    );
}

#[tokio::test]
async fn a_dependency_that_would_close_a_cycle_is_a_conflict() {
    let fx = fixture().await;
    let pool = fx.current.pool();
    // Here B exists already and A waits for B; the kept dep_added B → A would
    // close a cycle.
    card(pool, "B", 2, "kept card", 1).await;
    exec(
        pool,
        "INSERT INTO board_card_deps (card_id, depends_on_card_id, project_id, user_id, created_at) \
         VALUES ('A', 'B', 'proj-r', 'system_default_user', 1)",
    )
    .await;
    let report = recover_board(pool, &fx.kept_path, false).await.unwrap();
    assert!(
        report
            .conflicts
            .iter()
            .any(|c| c.entity == "dep" && c.reason.contains("cycle")),
        "{:?}",
        report.conflicts
    );
    assert_eq!(scalar_i64(pool, "SELECT COUNT(*) FROM board_card_deps").await, 1);
}

#[tokio::test]
async fn a_dry_run_reports_without_writing() {
    let fx = fixture().await;
    let pool = fx.current.pool();
    let events_before = scalar_i64(pool, "SELECT COUNT(*) FROM board_events").await;
    let report = recover_board(pool, &fx.kept_path, true).await.unwrap();
    assert!(report.dry_run);
    assert_eq!(report.cards_inserted, 1);
    assert!(report.events_inserted > 0);
    assert_eq!(
        scalar_i64(pool, "SELECT COUNT(*) FROM board_events").await,
        events_before
    );
    assert_eq!(scalar_i64(pool, "SELECT COUNT(*) FROM board_cards").await, 1);
    assert_eq!(
        scalar_i64(pool, "SELECT COUNT(*) FROM board_supervisor_cursor").await,
        0
    );
    // The pooled connection is detached again and usable.
    assert_eq!(
        scalar_i64(pool, "SELECT COUNT(*) FROM pragma_database_list WHERE name = 'kept'").await,
        0
    );
}

#[tokio::test]
async fn a_kept_database_without_board_tables_recovers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let kept_path = dir.path().join("old.db");
    {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&kept_path)
            .create_if_missing(true);
        let kept = SqlitePool::connect_with(options).await.unwrap();
        exec(&kept, "CREATE TABLE something (id INTEGER)").await;
        kept.close().await;
    }
    let current = open(&dir.path().join("current.db")).await;
    let report = recover_board(current.pool(), &kept_path, false).await.unwrap();
    assert!(!report.kept_has_board);
    assert_eq!(report.events_inserted, 0);
}
