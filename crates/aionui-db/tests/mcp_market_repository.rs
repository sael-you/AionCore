//! Migrations 904/905 and the MCP market repository: instances, secrets,
//! assignments (partial UNIQUE indexes), the shared `market_gate` predicate
//! and the read-only opener used by the launcher.

use std::sync::Arc;

use aionui_db::{
    CreateAssignmentParams, CreateMcpServerParams, DbError, IMcpMarketRepository, IMcpServerRepository,
    MarketInstanceParams, McpMarketScope, SqliteMcpMarketRepository, SqliteMcpServerRepository,
    UpdateMarketInstanceParams, init_database, init_database_memory, open_database_read_only,
};

const USER: &str = "system_default_user";

fn instance<'a>(name: &'a str, status: &'a str) -> MarketInstanceParams<'a> {
    MarketInstanceParams {
        user_id: USER,
        name,
        description: None,
        transport_type: "stdio",
        transport_config: r#"{"command":"aioncore-market-launcher","args":[]}"#,
        status,
        template_id: "paddle",
        template_json: r#"{"id":"paddle"}"#,
        environment: "production",
        tier: "read",
        enforcement: "server",
        clis_json: r#"["claude","codex"]"#,
        plain_env_json: "{}",
        secret_refs_json: "{}",
        tools_allow_json: Some(r#"["list_products"]"#),
        tools_block_json: "[]",
        approval_ref: None,
    }
}

async fn setup() -> (
    aionui_db::Database,
    SqliteMcpMarketRepository,
    SqliteMcpServerRepository,
) {
    let db = init_database_memory().await.unwrap();
    let market = SqliteMcpMarketRepository::new(db.pool().clone());
    let servers = SqliteMcpServerRepository::new(db.pool().clone());
    (db, market, servers)
}

async fn exec(db: &aionui_db::Database, sql: &str) {
    sqlx::query(sql).execute(db.pool()).await.unwrap();
}

async fn seed_project_conversation(db: &aionui_db::Database) {
    exec(
        db,
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) \
         VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)",
    )
    .await;
    exec(
        db,
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id) \
         VALUES ('conv-a', 'system_default_user', 'c', 'acp', 'pending', 1, 1, 'proj-a')",
    )
    .await;
}

#[tokio::test]
async fn migrations_904_905_create_market_schema() {
    let (db, _, _) = setup().await;
    let cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('mcp_servers')")
        .fetch_all(db.pool())
        .await
        .unwrap();
    for col in [
        "market_status",
        "market_template_id",
        "market_template",
        "market_tier",
        "market_tools_allow",
        "market_tools_block",
        "market_approval_ref",
        "market_secret_refs",
    ] {
        assert!(cols.iter().any(|c| c == col), "missing column {col}");
    }
    assert!(
        !cols.iter().any(|c| c == "status"),
        "legacy repair renames `status`; never reuse it"
    );
    let secret_cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('mcp_secrets')")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert!(secret_cols.iter().any(|c| c == "instance_id"));
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE version IN (904, 905) AND success = 1")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(versions, vec![904, 905]);
}

#[tokio::test]
async fn instance_rows_are_disabled_and_hidden_from_ordinary_fallback() {
    let (_db, market, servers) = setup().await;
    let row = market
        .create_instance(&instance("tb-paddle-read", "active"))
        .await
        .unwrap();
    assert!(row.is_market());
    let plain = servers.find_by_id(USER, &row.id).await.unwrap().unwrap();
    assert!(!plain.enabled, "R8: every market row is stored enabled = false");
    let dup = market
        .create_instance(&instance("tb-paddle-read", "active"))
        .await
        .unwrap_err();
    assert!(matches!(dup, DbError::Conflict(_)));
}

#[tokio::test]
async fn revive_resets_market_columns_and_drops_assignments_and_secrets() {
    let (db, market, _) = setup().await;
    seed_project_conversation(&db).await;
    let row = market.create_instance(&instance("tb-x", "active")).await.unwrap();
    market
        .upsert_secret(USER, &row.id, "TB_X__API_KEY", "cipher")
        .await
        .unwrap();
    market
        .create_assignment(&CreateAssignmentParams {
            user_id: USER,
            instance_id: &row.id,
            project_id: Some("proj-a"),
            assistant_id: None,
            cli: "claude",
        })
        .await
        .unwrap();
    market.delete_instance(USER, &row.id).await.unwrap();
    assert!(market.find_instance(USER, &row.id).await.unwrap().is_none());
    assert!(market.list_assignments(USER, None).await.unwrap().is_empty());

    let mut again = instance("tb-x", "dormant");
    again.tier = "write";
    let revived = market.revive_instance(USER, &row.id, &again).await.unwrap();
    assert_eq!(revived.id, row.id);
    assert_eq!(revived.market_status.as_deref(), Some("dormant"));
    assert_eq!(revived.market_tier.as_deref(), Some("write"));
    assert!(revived.deleted_at.is_none());
    assert!(market.list_instance_secrets(USER, &row.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn secrets_are_owned_by_one_instance() {
    let (_db, market, _) = setup().await;
    let a = market.create_instance(&instance("inst-a", "active")).await.unwrap();
    let b = market.create_instance(&instance("inst-b", "active")).await.unwrap();
    let first = market.upsert_secret(USER, &a.id, "INST_A__KEY", "c1").await.unwrap();
    let replaced = market
        .upsert_secret(USER, &a.id, "INST_A__KEY", "CIPHERTEXT-SENTINEL")
        .await
        .unwrap();
    assert_eq!(first.id, replaced.id);
    assert_eq!(replaced.value_encrypted, "CIPHERTEXT-SENTINEL");
    let stolen = market
        .upsert_secret(USER, &b.id, "INST_A__KEY", "c3")
        .await
        .unwrap_err();
    assert!(matches!(stolen, DbError::Conflict(_)));
    assert!(!market.delete_secret(USER, &b.id, "INST_A__KEY").await.unwrap());
    assert!(market.delete_secret(USER, &a.id, "INST_A__KEY").await.unwrap());
    assert!(
        !format!("{replaced:?}").contains("CIPHERTEXT-SENTINEL"),
        "Debug never prints the ciphertext"
    );
}

#[tokio::test]
async fn assignment_partial_unique_indexes() {
    let (db, market, _) = setup().await;
    seed_project_conversation(&db).await;
    let row = market.create_instance(&instance("inst-u", "active")).await.unwrap();
    let mk =
        |project: Option<&'static str>, assistant: Option<&'static str>, cli: &'static str| CreateAssignmentParams {
            user_id: USER,
            instance_id: "",
            project_id: project,
            assistant_id: assistant,
            cli,
        };
    for (project, assistant) in [(None, None), (Some("proj-a"), None), (Some("proj-a"), Some("asst-1"))] {
        let mut p = mk(project, assistant, "claude");
        p.instance_id = &row.id;
        market.create_assignment(&p).await.unwrap();
        let err = market.create_assignment(&p).await.unwrap_err();
        assert!(
            matches!(err, DbError::Conflict(_)),
            "duplicate {project:?}/{assistant:?}"
        );
        let mut other_cli = mk(project, assistant, "codex");
        other_cli.instance_id = &row.id;
        market.create_assignment(&other_cli).await.unwrap();
    }
    assert!(
        market
            .has_assignment(USER, &row.id, None, None, "claude")
            .await
            .unwrap()
    );
    assert!(
        market
            .has_assignment(USER, &row.id, Some("proj-a"), None, "claude")
            .await
            .unwrap()
    );
    assert!(
        market
            .has_assignment(USER, &row.id, Some("proj-a"), Some("asst-1"), "codex")
            .await
            .unwrap()
    );
    assert!(
        !market
            .has_assignment(USER, &row.id, Some("proj-a"), Some("asst-2"), "claude")
            .await
            .unwrap()
    );
    assert!(
        !market
            .has_assignment(USER, &row.id, Some("proj-b"), None, "claude")
            .await
            .unwrap()
    );
    assert_eq!(
        market.delete_instance_assignments(USER, &row.id).await.unwrap().len(),
        6
    );
}

#[tokio::test]
async fn market_gate_status_and_assignment_predicate() {
    let (db, market, servers) = setup().await;
    seed_project_conversation(&db).await;
    let active = market
        .create_instance(&instance("gate-active", "active"))
        .await
        .unwrap();
    let dormant = market
        .create_instance(&instance("gate-dormant", "dormant"))
        .await
        .unwrap();
    let revoked = market
        .create_instance(&instance("gate-revoked", "revoked"))
        .await
        .unwrap();
    let plain = servers
        .create(CreateMcpServerParams {
            user_id: USER,
            name: "plain",
            description: None,
            enabled: true,
            transport_type: "stdio",
            transport_config: r#"{"command":"node"}"#,
            tools: None,
            original_json: None,
            builtin: false,
        })
        .await
        .unwrap();
    for id in [&active.id, &dormant.id, &revoked.id] {
        market
            .create_assignment(&CreateAssignmentParams {
                user_id: USER,
                instance_id: id,
                project_id: Some("proj-a"),
                assistant_id: Some("asst-1"),
                cli: "claude",
            })
            .await
            .unwrap();
    }
    let ids = vec![
        active.id.clone(),
        dormant.id.clone(),
        revoked.id.clone(),
        plain.id.clone(),
    ];
    let rows = servers.list_by_ids_any(USER, &ids).await.unwrap();

    // Snapshot time: status only.
    let gated = servers
        .market_gate(USER, &McpMarketScope::default(), rows.clone())
        .await
        .unwrap();
    let names: Vec<_> = gated.iter().map(|g| g.row.name.as_str()).collect();
    assert_eq!(names, vec!["gate-active", "plain"]);
    assert!(gated[0].market.is_some() && gated[1].market.is_none());

    // Spawn time: assistant-channel assignment for the conversation's project.
    let scope = |assistant, cli| McpMarketScope {
        conversation_id: Some("conv-a"),
        assistant_id: assistant,
        cli,
    };
    let gated = servers
        .market_gate(USER, &scope(Some("asst-1"), Some("claude")), rows.clone())
        .await
        .unwrap();
    assert_eq!(gated.len(), 2);
    let delivery = gated[0].market.as_ref().unwrap();
    assert_eq!(delivery.project_id.as_deref(), Some("proj-a"));
    assert_eq!(delivery.assistant_id.as_deref(), Some("asst-1"));
    assert_eq!(delivery.tools_allow.as_deref(), Some(r#"["list_products"]"#));
    for (assistant, cli) in [
        (Some("asst-2"), Some("claude")),
        (Some("asst-1"), Some("codex")),
        (None, Some("claude")),
    ] {
        let gated = servers
            .market_gate(USER, &scope(assistant, cli), rows.clone())
            .await
            .unwrap();
        assert_eq!(gated.len(), 1, "{assistant:?}/{cli:?} must drop the market row");
        assert_eq!(gated[0].row.name, "plain");
    }

    // Soft-deleted market row stays out even when explicitly selected.
    market.delete_instance(USER, &active.id).await.unwrap();
    let rows = servers.list_by_ids_any(USER, &ids).await.unwrap();
    let gated = servers
        .market_gate(USER, &McpMarketScope::default(), rows)
        .await
        .unwrap();
    assert_eq!(gated.len(), 1);

    // Whole-project assignments are native-file only: never an AionUi delivery.
    let other = market
        .create_instance(&instance("gate-project", "active"))
        .await
        .unwrap();
    market
        .create_assignment(&CreateAssignmentParams {
            user_id: USER,
            instance_id: &other.id,
            project_id: Some("proj-a"),
            assistant_id: None,
            cli: "claude",
        })
        .await
        .unwrap();
    let rows = servers
        .list_by_ids_any(USER, std::slice::from_ref(&other.id))
        .await
        .unwrap();
    let gated = servers
        .market_gate(USER, &scope(Some("asst-1"), Some("claude")), rows)
        .await
        .unwrap();
    assert!(gated.is_empty());
}

/// TASK-71: a conditional write refuses a row changed since it was read, and
/// a same-millisecond write still moves `updated_at` forward.
#[tokio::test]
async fn update_instance_with_a_stale_updated_at_is_a_conflict() {
    let (_db, market, _) = setup().await;
    let row = market.create_instance(&instance("stale", "active")).await.unwrap();
    let revoked = market
        .update_instance(
            USER,
            &row.id,
            &UpdateMarketInstanceParams {
                status: Some("revoked"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(revoked.updated_at > row.updated_at);
    let stale = market
        .update_instance(
            USER,
            &row.id,
            &UpdateMarketInstanceParams {
                tier: Some("write"),
                expected_updated_at: Some(row.updated_at),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(stale, DbError::Conflict(_)), "{stale:?}");
    let current = market.find_instance(USER, &row.id).await.unwrap().unwrap();
    assert_eq!(current.market_status.as_deref(), Some("revoked"));
    assert_eq!(current.market_tier.as_deref(), Some("read"));
    market
        .update_instance(
            USER,
            &row.id,
            &UpdateMarketInstanceParams {
                tier: Some("write"),
                expected_updated_at: Some(current.updated_at),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn update_instance_keeps_and_clears_fields() {
    let (_db, market, _) = setup().await;
    let row = market.create_instance(&instance("upd", "dormant")).await.unwrap();
    let updated = market
        .update_instance(
            USER,
            &row.id,
            &UpdateMarketInstanceParams {
                status: Some("active"),
                approval_ref: Some(Some("OA-20261115-paddle")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.market_status.as_deref(), Some("active"));
    assert_eq!(updated.market_tier.as_deref(), Some("read"));
    assert_eq!(updated.market_approval_ref.as_deref(), Some("OA-20261115-paddle"));
    let cleared = market
        .update_instance(
            USER,
            &row.id,
            &UpdateMarketInstanceParams {
                approval_ref: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(cleared.market_approval_ref.is_none());
    let missing = market
        .update_instance(USER, "mcp_missing", &UpdateMarketInstanceParams::default())
        .await
        .unwrap_err();
    assert!(matches!(missing, DbError::NotFound(_)));
}

#[tokio::test]
async fn conversation_project_lookup() {
    let (db, market, _) = setup().await;
    seed_project_conversation(&db).await;
    assert_eq!(
        market.conversation_project_id(USER, "conv-a").await.unwrap().as_deref(),
        Some("proj-a")
    );
    assert!(
        market
            .conversation_project_id("other", "conv-a")
            .await
            .unwrap()
            .is_none()
    );
    assert!(market.project_exists("proj-a").await.unwrap());
    assert!(!market.project_exists("proj-z").await.unwrap());
}

#[tokio::test]
async fn read_only_open_refuses_writes_missing_files_and_old_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    assert!(matches!(
        open_database_read_only(&path, &[904, 905]).await,
        Err(DbError::Init(_))
    ));
    assert!(!path.exists(), "the read-only opener never creates the file");

    let db = init_database(&path).await.unwrap();
    let market = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    market.create_instance(&instance("ro-check", "active")).await.unwrap();

    // While the writer pool is still open (WAL), a read-only pool reads.
    let ro = open_database_read_only(&path, &[904, 905]).await.unwrap();
    let ro_market = SqliteMcpMarketRepository::new(ro.pool().clone());
    assert_eq!(ro_market.list_instances(USER).await.unwrap().len(), 1);
    let write = ro_market.create_instance(&instance("ro-write", "active")).await;
    assert!(write.is_err(), "a read-only connection must refuse writes");

    assert!(matches!(
        open_database_read_only(&path, &[904, 905, 999_999]).await,
        Err(DbError::Init(msg)) if msg.contains("999999")
    ));
    drop(ro_market);
    ro.close().await;
    db.close().await;

    // Server not running (writer closed, WAL checkpointed): still readable.
    let ro = open_database_read_only(&path, &[904, 905]).await.unwrap();
    let ro_market = SqliteMcpMarketRepository::new(ro.pool().clone());
    assert_eq!(ro_market.list_instances(USER).await.unwrap().len(), 1);
    ro.close().await;
}
