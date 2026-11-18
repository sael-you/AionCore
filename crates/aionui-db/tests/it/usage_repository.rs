//! Migration 913 and the usage repository (card C42): a recorded turn lands
//! in `usage_turns` and `usage_daily` together, scoped to its owner, with the
//! project / backend / model read from the conversation; the read path sums
//! per day, model, project and conversation; the overview counts.

use aionui_db::{
    AttachParams, IAcpSessionRepository, IKitRepository, IUsageRepository, SqliteAcpSessionRepository,
    SqliteKitRepository, SqliteUsageRepository, UsageTurnParams, init_database_memory,
};

const USER: &str = "system_default_user";
/// 2026-11-18T10:00:00Z and the day before.
const T_18: i64 = 1_794_996_000_000;
const T_17: i64 = T_18 - 86_400_000;

async fn setup() -> (aionui_db::Database, SqliteUsageRepository) {
    let db = init_database_memory().await.unwrap();
    for sql in [
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'TB', 'standard', 1, 1)",
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) VALUES ('conv-a', 'system_default_user', 'Alpha', 'acp', 'pending', 1, 1, 'proj-a', '{}')",
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) VALUES ('conv-b', 'system_default_user', 'Beta', 'acp', 'pending', 1, 1, 'proj-a', '{}')",
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra, archived_at) VALUES ('conv-old', 'system_default_user', 'Old', 'acp', 'pending', 1, 1, 'proj-a', '{}', 5)",
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, extra) VALUES ('conv-free', 'system_default_user', 'Free', 'acp', 'pending', 1, 1, '{}')",
        "INSERT INTO acp_session (conversation_id, agent_source, agent_id, session_status, session_config) VALUES ('conv-a', 'builtin', (SELECT id FROM agent_metadata WHERE backend = 'claude' LIMIT 1), 'idle', '{\"runtime\":{\"current_model_id\":\"model-x\"}}')",
    ] {
        sqlx::query(sql)
            .execute(db.pool())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let repo = SqliteUsageRepository::new(db.pool().clone());
    (db, repo)
}

fn turn(conversation: &'static str, at: i64, input: i64, output: i64, cost: Option<i64>) -> UsageTurnParams<'static> {
    UsageTurnParams {
        user_id: USER,
        conversation_id: conversation,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 100,
        cache_write_tokens: 10,
        thought_tokens: 1,
        cost_micro_usd: cost,
        at,
    }
}

#[tokio::test]
async fn a_turn_lands_in_both_tables_with_project_backend_and_model() {
    let (db, repo) = setup().await;
    assert_eq!(repo.usage_since(USER, Some("proj-a")).await.unwrap(), None);

    assert!(
        repo.record_usage_turn(&turn("conv-a", T_18, 5, 7, Some(1500)))
            .await
            .unwrap()
    );
    assert!(
        repo.record_usage_turn(&turn("conv-a", T_18 + 60_000, 1, 2, None))
            .await
            .unwrap()
    );

    let (project, backend, model): (String, String, String) =
        sqlx::query_as("SELECT project_id, backend, model FROM usage_turns LIMIT 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        (project.as_str(), backend.as_str(), model.as_str()),
        ("proj-a", "claude", "model-x")
    );
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_turns")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(turns, 2);

    let days = repo
        .usage_by_day(USER, Some("proj-a"), "2026-11-18", "2026-11-18")
        .await
        .unwrap();
    assert_eq!(days.len(), 1);
    assert_eq!(days[0].day, "2026-11-18");
    let s = days[0].sums;
    assert_eq!(
        (
            s.input_tokens,
            s.output_tokens,
            s.cache_read_tokens,
            s.cache_write_tokens,
            s.thought_tokens,
            s.turns
        ),
        (6, 9, 200, 20, 2, 2)
    );
    assert_eq!(
        s.cost_micro_usd,
        Some(1500),
        "a turn without cost adds nothing but keeps the sum"
    );
    assert_eq!(repo.usage_since(USER, Some("proj-a")).await.unwrap(), Some(T_18));
}

#[tokio::test]
async fn empty_reports_and_foreign_conversations_record_nothing() {
    let (db, repo) = setup().await;
    let empty = UsageTurnParams {
        user_id: USER,
        conversation_id: "conv-a",
        at: T_18,
        ..Default::default()
    };
    assert!(!repo.record_usage_turn(&empty).await.unwrap());
    let mut foreign = turn("conv-a", T_18, 5, 5, None);
    foreign.user_id = "someone-else";
    assert!(!repo.record_usage_turn(&foreign).await.unwrap());
    assert!(
        !repo
            .record_usage_turn(&turn("no-such-conversation", T_18, 5, 5, None))
            .await
            .unwrap()
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM usage_turns) + (SELECT COUNT(*) FROM usage_daily)")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn reads_are_scoped_and_grouped() {
    let (_db, repo) = setup().await;
    repo.record_usage_turn(&turn("conv-a", T_17, 10, 10, Some(100)))
        .await
        .unwrap();
    repo.record_usage_turn(&turn("conv-a", T_18, 30, 30, Some(300)))
        .await
        .unwrap();
    repo.record_usage_turn(&turn("conv-b", T_18, 100, 100, None))
        .await
        .unwrap();
    repo.record_usage_turn(&turn("conv-free", T_18, 1, 1, None))
        .await
        .unwrap();

    // Per day, project scope vs all.
    let project = repo
        .usage_by_day(USER, Some("proj-a"), "2026-11-17", "2026-11-18")
        .await
        .unwrap();
    assert_eq!(
        project
            .iter()
            .map(|d| (d.day.as_str(), d.sums.input_tokens))
            .collect::<Vec<_>>(),
        vec![("2026-11-17", 10), ("2026-11-18", 130)]
    );
    let all = repo.usage_by_day(USER, None, "2026-11-18", "2026-11-18").await.unwrap();
    assert_eq!(all[0].sums.input_tokens, 131);
    let outside = repo
        .usage_by_day(USER, Some(""), "2026-11-18", "2026-11-18")
        .await
        .unwrap();
    assert_eq!(
        outside[0].sums.input_tokens, 1,
        "'' = conversations outside any project"
    );
    assert!(
        repo.usage_by_day("someone-else", None, "2026-11-01", "2026-11-30")
            .await
            .unwrap()
            .is_empty()
    );

    // By model: conv-a has a session (claude / model-x), conv-b has none.
    let models = repo
        .usage_by_model(USER, Some("proj-a"), "2026-11-17", "2026-11-18")
        .await
        .unwrap();
    assert_eq!(
        models
            .iter()
            .map(|m| (m.model.as_str(), m.backend.as_str(), m.sums.input_tokens))
            .collect::<Vec<_>>(),
        vec![("", "", 100), ("model-x", "claude", 40)]
    );

    // By project, largest first, with names.
    let projects = repo.usage_by_project(USER, "2026-11-17", "2026-11-18").await.unwrap();
    assert_eq!(
        projects
            .iter()
            .map(|p| (p.project_id.as_str(), p.name.as_str(), p.sums.turns))
            .collect::<Vec<_>>(),
        vec![("proj-a", "TB", 3), ("", "", 1)]
    );

    // Top conversations of the project since a point in time.
    let top = repo.usage_by_conversation(USER, "proj-a", T_17, 10).await.unwrap();
    assert_eq!(
        top.iter()
            .map(|c| (c.name.as_str(), c.sums.input_tokens, c.sums.turns, c.last_at))
            .collect::<Vec<_>>(),
        vec![("Beta", 100, 1, T_18), ("Alpha", 40, 2, T_18)]
    );
    assert_eq!(top[1].sums.cost_micro_usd, Some(400));
    assert_eq!(
        repo.usage_by_conversation(USER, "proj-a", T_18, 1).await.unwrap().len(),
        1,
        "limit"
    );
    assert_eq!(
        repo.usage_by_conversation(USER, "proj-a", T_18 + 1, 10)
            .await
            .unwrap()
            .len(),
        0,
        "range"
    );
}

/// The session pump only holds `IAcpSessionRepository`: its method writes the same rows.
#[tokio::test]
async fn the_session_repository_records_into_the_same_ledger() {
    let (db, repo) = setup().await;
    let sessions = SqliteAcpSessionRepository::new(db.pool().clone());
    assert!(
        sessions
            .record_usage_turn(&turn("conv-a", T_18, 3, 4, None))
            .await
            .unwrap()
    );
    let days = repo
        .usage_by_day(USER, Some("proj-a"), "2026-11-18", "2026-11-18")
        .await
        .unwrap();
    assert_eq!((days[0].sums.input_tokens, days[0].sums.output_tokens), (3, 4));
}

#[tokio::test]
async fn project_counts_for_the_overview() {
    let (db, repo) = setup().await;
    let kits = SqliteKitRepository::new(db.pool().clone());
    for (kind, item, op) in [
        ("skill", "pdf", "add"),
        ("skill", "old", "remove"),
        ("rule", "rule_1", "add"),
        ("server", "srv_1", "add"),
    ] {
        kits.upsert_attachment(&AttachParams {
            user_id: USER,
            level: "project",
            target_id: "proj-a",
            item_kind: kind,
            item_ref: item,
            op,
        })
        .await
        .unwrap();
    }
    for sql in [
        "INSERT INTO board_columns (id, project_id, user_id, key, category, position) VALUES ('c1', 'proj-a', 'system_default_user', 'todo', 'todo', 1)",
        "INSERT INTO board_columns (id, project_id, user_id, key, category, position) VALUES ('c2', 'proj-a', 'system_default_user', 'done', 'done', 2)",
        "INSERT INTO board_cards (id, project_id, user_id, number, title, column_key, position, creator_kind, creator_ref, created_at, updated_at) VALUES ('k1', 'proj-a', 'system_default_user', 1, 'a', 'todo', 1, 'user', 'u', 1, 1)",
        "INSERT INTO board_cards (id, project_id, user_id, number, title, column_key, position, creator_kind, creator_ref, created_at, updated_at) VALUES ('k2', 'proj-a', 'system_default_user', 2, 'b', 'done', 1, 'user', 'u', 1, 1)",
    ] {
        sqlx::query(sql)
            .execute(db.pool())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let counts = repo.project_counts(USER, "proj-a").await.unwrap();
    assert_eq!((counts.conversations_active, counts.conversations_total), (2, 3));
    assert_eq!(counts.open_cards, 1);
    assert_eq!(
        (counts.kit_skills, counts.kit_mcps, counts.kit_rules, counts.kit_servers),
        (1, 0, 1, 1),
        "only included decisions count"
    );
    let other = repo.project_counts("someone-else", "proj-a").await.unwrap();
    assert_eq!((other.conversations_total, other.open_cards), (0, 0));
}

/// Staging finding (TASK-169): a turn was dropped with "database is locked".
/// The ledger write read first and wrote second inside a deferred
/// transaction; when another connection committed in between, SQLite refused
/// the lock upgrade at once (busy_timeout does not apply to a stale read
/// snapshot). On a file-backed WAL database with other writers running,
/// every report must still be recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_never_drop_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let db = aionui_db::init_database(&dir.path().join("usage.db")).await.unwrap();
    for sql in [
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'TB', 'standard', 1, 1)",
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) VALUES ('conv-a', 'system_default_user', 'Alpha', 'acp', 'pending', 1, 1, 'proj-a', '{}')",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    const TURNS: i64 = 80;
    let mut tasks = Vec::new();
    for i in 0..TURNS {
        let repo = SqliteUsageRepository::new(db.pool().clone());
        tasks.push(tokio::spawn(async move {
            repo.record_usage_turn(&turn("conv-a", T_18 + i, 1, 1, Some(10)))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }));
        // What the server does around a turn: other statements writing the same database.
        let pool = db.pool().clone();
        tasks.push(tokio::spawn(async move {
            sqlx::query("UPDATE conversations SET updated_at = updated_at + 1 WHERE id = 'conv-a'")
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }));
    }
    let mut errors = Vec::new();
    for task in tasks {
        if let Err(e) = task.await.unwrap() {
            errors.push(e);
        }
    }
    assert!(
        errors.is_empty(),
        "{} writes failed, first: {:?}",
        errors.len(),
        errors.first()
    );
    let repo = SqliteUsageRepository::new(db.pool().clone());
    let days = repo
        .usage_by_day(USER, Some("proj-a"), "2026-11-18", "2026-11-18")
        .await
        .unwrap();
    assert_eq!((days[0].sums.turns, days[0].sums.input_tokens), (TURNS, TURNS));
    assert_eq!(days[0].sums.cost_micro_usd, Some(TURNS * 10));
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_turns")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, TURNS);
    db.close().await;
}
