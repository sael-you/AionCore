//! Migration 901 (project board): a database at 044 + 900 upgrades to 901
//! through the real startup path; a later out-of-order 045 still applies on
//! top; the partial unique index on `conversations` works.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
}

fn version_of(name: &str) -> i64 {
    name.split('_').next().unwrap().parse().unwrap()
}

/// Copy the shipped migrations whose version satisfies `keep` into `dir`.
fn stage(dir: &Path, keep: impl Fn(i64) -> bool) {
    for entry in std::fs::read_dir(migrations_dir()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".sql") && keep(version_of(&name)) {
            std::fs::copy(entry.path(), dir.join(&name)).unwrap();
        }
    }
}

async fn pool(path: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap()
}

/// Run a staged migrator with the connection PRAGMAs `run_migrations` uses
/// (table rebuilds in 030 need them).
async fn migrate(pool: &SqlitePool, dir: &Path) {
    sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
        .execute(pool)
        .await
        .unwrap();
    Migrator::new(dir).await.unwrap().run(pool).await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON; PRAGMA legacy_alter_table = OFF")
        .execute(pool)
        .await
        .unwrap();
}

async fn applied_versions(pool: &SqlitePool) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn table_exists(pool: &SqlitePool, name: &str) -> bool {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    count == 1
}

async fn insert_conversation(pool: &SqlitePool, id: &str, user: &str, extra: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES (?, ?, 'c', 'acp', ?, 'finished', 1, 1)",
    )
    .bind(id)
    .bind(user)
    .bind(extra)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn a_database_at_044_and_900_upgrades_to_901_through_the_startup_path() {
    let staged = tempfile::tempdir().unwrap();
    stage(staged.path(), |version| version <= 900);
    let data = tempfile::tempdir().unwrap();
    let db_path = data.path().join("aionui-backend.db");
    {
        let pool = pool(&db_path).await;
        migrate(&pool, staged.path()).await;
        let versions = applied_versions(&pool).await;
        assert!(versions.contains(&44) && versions.contains(&900) && !versions.contains(&901));
        assert!(!table_exists(&pool, "board_cards").await);
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('u', 'u', 'x', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // An existing assignment-free conversation is unaffected by 901.
        insert_conversation(&pool, "legacy", "u", "{}").await.unwrap();
        pool.close().await;
    }

    let db = aionui_db::init_database(&db_path)
        .await
        .expect("901 applies on top of 900");
    let versions = applied_versions(db.pool()).await;
    // 901 applied on top of 900; later youssef migrations (904/905, MCP
    // market) may follow it, so assert membership, not "last".
    assert!(versions.contains(&901), "{versions:?}");
    assert_eq!(versions.last().copied(), aionui_db::latest_known_migration_version());
    for table in [
        "board_settings",
        "board_columns",
        "board_cards",
        "board_card_reviews",
        "board_card_deps",
        "board_idempotency",
        "board_deploy_jobs",
        "board_supervisor_cursor",
        "board_card_questions",
        "board_card_links",
        "board_events",
    ] {
        assert!(table_exists(db.pool(), table).await, "{table} created");
    }
    // S4 fields exist from day one (901 is frozen once deployed).
    let columns: Vec<String> = sqlx::query("PRAGMA table_info(board_idempotency)")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();
    assert!(columns.contains(&"claim_token".to_owned()));
    assert!(columns.contains(&"packet_message_id".to_owned()));
    // seq is AUTOINCREMENT (never reused).
    let sql: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'board_events'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(sql.contains("AUTOINCREMENT"));
    db.close().await;
}

#[tokio::test]
async fn a_later_out_of_order_045_still_applies_after_901() {
    let staged = tempfile::tempdir().unwrap();
    stage(staged.path(), |version| version <= 901);
    let data = tempfile::tempdir().unwrap();
    let pool = pool(&data.path().join("db.sqlite")).await;
    migrate(&pool, staged.path()).await;
    assert_eq!(applied_versions(&pool).await.last(), Some(&901));

    // An upstream merge later brings 045 (below our 9xx range).
    std::fs::write(
        staged.path().join("045_upstream_probe.sql"),
        "CREATE TABLE IF NOT EXISTS upstream_probe (id INTEGER PRIMARY KEY);",
    )
    .unwrap();
    migrate(&pool, staged.path()).await;
    let versions = applied_versions(&pool).await;
    assert!(versions.contains(&45), "out-of-order 045 applied: {versions:?}");
    assert!(table_exists(&pool, "upstream_probe").await);
    assert!(table_exists(&pool, "board_events").await, "901 untouched");
    pool.close().await;
}

#[tokio::test]
async fn the_board_assignment_index_is_a_per_user_find_or_create_guard() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let pool = db.pool();
    for user in ["u1", "u2"] {
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
            .bind(user)
            .bind(user)
            .execute(pool)
            .await
            .unwrap();
    }
    let assigned = r#"{"board_assignment_id":"asg-1"}"#;
    insert_conversation(pool, "a", "u1", assigned).await.unwrap();
    let duplicate = insert_conversation(pool, "b", "u1", assigned).await.unwrap_err();
    assert!(
        duplicate.to_string().to_lowercase().contains("unique"),
        "second conversation for the same assignment is refused: {duplicate}"
    );
    insert_conversation(pool, "c", "u2", assigned)
        .await
        .expect("scoped per user");
    insert_conversation(pool, "d", "u1", "{}")
        .await
        .expect("no assignment id → unconstrained");
    insert_conversation(pool, "e", "u1", "{}").await.unwrap();

    // The lookup expression hits the index.
    let plan: Vec<String> = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id FROM conversations WHERE user_id = 'u1' AND \
         json_extract(extra, '$.board_assignment_id') = 'asg-1'",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<String, _>("detail"))
    .collect();
    assert!(
        plan.iter()
            .any(|detail| detail.contains("idx_conversations_board_assignment")),
        "{plan:?}"
    );
}
