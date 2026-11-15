//! MCP market service (card C15) against a real in-memory DB: every R-rule
//! that lives in core, secrets never leaving the service in clear, the
//! generic-route guards (R1 #10), mask round-trip (R1 #11), server-side test
//! transport (R1 #12) and the launcher's DB decision (R9, R1 #1–#3).

use std::collections::BTreeMap;
use std::sync::Arc;

use aionui_api_types::{
    BatchImportMcpServersRequest, CreateMcpMarketAssignmentRequest, CreateMcpMarketInstanceRequest,
    CreateMcpServerRequest, ImportMcpServerRequest, McpTransport, UpdateMcpMarketInstanceRequest,
    UpdateMcpServerRequest,
};
use aionui_db::{
    Database, IMcpMarketRepository, IMcpServerRepository, SqliteMcpMarketRepository, SqliteMcpServerRepository,
    init_database_memory,
};
use aionui_mcp::market::{LaunchRequest, LaunchScope, MarketError, resolve_launch, still_allowed};
use aionui_mcp::types::McpServerTransport;
use aionui_mcp::{McpConfigService, McpError, McpMarketService, StaticTemplateCatalog};
use serde_json::json;

const USER: &str = "system_default_user";
const KEY: [u8; 32] = [7u8; 32];
const FIXTURE_SECRET: &str = "pdl_live_FIXTURESECRETVALUE0123456789";
const LEGACY_SECRET: &str = "sk_live_LEGACYFIXTURE0123456789";

fn catalog() -> StaticTemplateCatalog {
    StaticTemplateCatalog::new(vec![
        json!({"id": "paddle", "name": "Paddle", "market": {
            "transport": "stdio",
            "command": {"command": "npx", "args": ["-y", "@paddle/paddle-mcp@0.1.0"]},
            "env": {
                "PADDLE_API_KEY": {"kind": "secret", "required": true},
                "PADDLE_ENVIRONMENT": {"kind": "plain", "required": true, "enum": ["production", "sandbox"]},
                "PADDLE_MCP_TOOLS": {"kind": "plain"}
            },
            "tiers": ["read", "write"],
            "tier_env": {"read": {"PADDLE_MCP_TOOLS": "read-only"}, "write": {"PADDLE_MCP_TOOLS": "non-destructive"}},
            "enforcement": "server",
            "clis": ["claude", "codex", "gemini"]
        }}),
        json!({"id": "context7", "name": "Context7", "market": {
            "transport": "http", "url": "https://mcp.context7.com/mcp",
            "tiers": ["read"], "enforcement": "server", "clis": ["claude", "codex", "gemini"]
        }}),
        json!({"id": "duck", "name": "Duck", "market": {
            "transport": "stdio", "command": {"command": "npx", "args": ["-y", "duck-mcp@1.0.0"]},
            "tiers": ["read"], "enforcement": "tools-only", "clis": ["claude"]
        }}),
        // Invalid template (secret-looking literal in args) is skipped, never served.
        json!({"id": "bad", "name": "Bad", "market": {
            "transport": "stdio", "command": {"command": "npx", "args": ["--token", "sk_live_ABCDEFGHIJKLMNOPQRSTUV"]},
            "tiers": ["read"], "enforcement": "server", "clis": ["claude"]
        }}),
    ])
}

struct Fx {
    db: Database,
    market: McpMarketService,
    config: McpConfigService,
    repo: Arc<dyn IMcpMarketRepository>,
}

async fn fx() -> Fx {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let market = McpMarketService::new(repo.clone(), servers.clone(), Arc::new(catalog()), KEY);
    let config = McpConfigService::new(servers).with_market_repo(repo.clone());
    for sql in [
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)",
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-b', 'system_default_user', 'B', 'standard', 1, 1)",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    Fx {
        db,
        market,
        config,
        repo,
    }
}

fn paddle_read(name: &str) -> CreateMcpMarketInstanceRequest {
    CreateMcpMarketInstanceRequest {
        name: name.to_owned(),
        template_id: "paddle".into(),
        description: None,
        environment: "production".into(),
        tier: "read".into(),
        status: None,
        clis: vec!["claude".into(), "codex".into()],
        plain_env: BTreeMap::from([("PADDLE_ENVIRONMENT".into(), "production".into())]),
        tools_allow: Some(vec!["list_products".into()]),
        tools_block: vec![],
    }
}

fn paddle_write(name: &str) -> CreateMcpMarketInstanceRequest {
    CreateMcpMarketInstanceRequest {
        tier: "write".into(),
        clis: vec!["claude".into()],
        tools_allow: None,
        tools_block: vec!["delete_product".into()],
        ..paddle_read(name)
    }
}

fn invalid(err: MarketError) -> String {
    match err {
        MarketError::Invalid(msg) => msg,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

async fn activate(f: &Fx, id: &str) {
    f.market
        .set_secret(USER, id, "PADDLE_API_KEY", FIXTURE_SECRET)
        .await
        .unwrap();
    f.market
        .update_instance(
            USER,
            id,
            UpdateMcpMarketInstanceRequest {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

async fn assign(
    f: &Fx,
    id: &str,
    project: Option<&str>,
    assistant: Option<&str>,
    clis: &[&str],
) -> Result<(), MarketError> {
    f.market
        .create_assignments(
            USER,
            CreateMcpMarketAssignmentRequest {
                instance_id: id.into(),
                project_id: project.map(str::to_owned),
                assistant_id: assistant.map(str::to_owned),
                clis: clis.iter().map(|c| c.to_string()).collect(),
            },
        )
        .await
        .map(|_| ())
}

// -- templates ---------------------------------------------------------------

#[tokio::test]
async fn invalid_templates_are_not_served() {
    let f = fx().await;
    let ids: Vec<String> = f
        .market
        .list_templates(USER)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec!["context7", "duck", "paddle"]);
}

// -- R1, R11, R1 #14 ------------------------------------------------------------

#[tokio::test]
async fn r1_names() {
    let f = fx().await;
    for bad in [
        "Tb-Paddle",
        "tb_paddle",
        "paddle",
        "aionui-team",
        "playwright-x",
        "github",
        "a-name-longer-than-24-chars",
    ] {
        let err = f.market.create_instance(USER, paddle_read(bad)).await.unwrap_err();
        assert!(matches!(err, MarketError::Invalid(_)), "{bad}");
    }
    // context7 may equal its template id (global ids).
    let ctx = CreateMcpMarketInstanceRequest {
        name: "context7".into(),
        template_id: "context7".into(),
        description: None,
        environment: "production".into(),
        tier: "read".into(),
        status: Some("active".into()),
        clis: vec!["claude".into(), "codex".into(), "gemini".into()],
        plain_env: BTreeMap::new(),
        tools_allow: Some(vec!["resolve-library-id".into()]),
        tools_block: vec![],
    };
    f.market.create_instance(USER, ctx).await.unwrap();
    // Builtin / ordinary names are taken.
    f.config
        .add_server(
            USER,
            CreateMcpServerRequest {
                name: "plain-one".into(),
                description: None,
                transport: McpTransport::Stdio {
                    command: "node".into(),
                    args: vec![],
                    env: Default::default(),
                },
                original_json: None,
                builtin: false,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        f.market
            .create_instance(USER, paddle_read("plain-one"))
            .await
            .unwrap_err(),
        MarketError::Conflict(_)
    ));
}

// -- R2 ---------------------------------------------------------------------------

#[tokio::test]
async fn r2_only_two_globals() {
    let f = fx().await;
    let inst = f
        .market
        .create_instance(USER, paddle_read("tb-paddle-read"))
        .await
        .unwrap();
    let msg = invalid(assign(&f, &inst.id, None, None, &["claude"]).await.unwrap_err());
    assert!(msg.contains("R2"), "{msg}");
}

// -- R3 ---------------------------------------------------------------------------

#[tokio::test]
async fn r3_write_is_claude_only_never_global_and_prod_needs_approval() {
    let f = fx().await;
    let mut req = paddle_write("tb-paddle-write");
    req.clis = vec!["claude".into(), "codex".into()];
    assert!(invalid(f.market.create_instance(USER, req).await.unwrap_err()).contains("Claude-only"));

    let inst = f
        .market
        .create_instance(USER, paddle_write("tb-paddle-write"))
        .await
        .unwrap();
    assert_eq!(inst.plain_env["PADDLE_MCP_TOOLS"], "non-destructive", "tier_env forced");
    f.market
        .set_secret(USER, &inst.id, "PADDLE_API_KEY", FIXTURE_SECRET)
        .await
        .unwrap();
    let err = f
        .market
        .update_instance(
            USER,
            &inst.id,
            UpdateMcpMarketInstanceRequest {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(invalid(err).contains("owner decision 3"));
    assert!(
        invalid(
            f.market
                .set_approval(USER, &inst.id, Some("approved".into()))
                .await
                .unwrap_err()
        )
        .contains("OA-")
    );
    f.market
        .set_approval(USER, &inst.id, Some("OA-20261115-paddle-write".into()))
        .await
        .unwrap();
    activate(&f, &inst.id).await;
    // Write assignments: Claude only, never global.
    assert!(
        invalid(
            assign(&f, &inst.id, Some("proj-a"), None, &["codex"])
                .await
                .unwrap_err()
        )
        .contains("R3")
    );
    assign(&f, &inst.id, Some("proj-a"), None, &["claude"]).await.unwrap();
    // Clearing the record of an active prod write instance makes it dormant.
    let back = f.market.set_approval(USER, &inst.id, None).await.unwrap();
    assert_eq!(back.status, "dormant");
}

// -- R4 / R5 -----------------------------------------------------------------------

#[tokio::test]
async fn r4_no_secret_values_outside_mcp_secrets() {
    let f = fx().await;
    let mut req = paddle_read("tb-r4");
    req.plain_env.insert("PADDLE_API_KEY".into(), FIXTURE_SECRET.into());
    assert!(invalid(f.market.create_instance(USER, req).await.unwrap_err()).contains("secrets route"));
    let mut req = paddle_read("tb-r4");
    req.plain_env.insert("PADDLE_ENVIRONMENT".into(), FIXTURE_SECRET.into());
    assert!(invalid(f.market.create_instance(USER, req).await.unwrap_err()).contains("credential"));
    let mut req = paddle_read("tb-r4");
    req.plain_env.insert("UNDECLARED".into(), "x".into());
    assert!(invalid(f.market.create_instance(USER, req).await.unwrap_err()).contains("not declared"));
}

#[tokio::test]
async fn r5_secrets_are_write_only_and_encrypted() {
    let f = fx().await;
    let inst = f
        .market
        .create_instance(USER, paddle_read("tb-paddle-read"))
        .await
        .unwrap();
    assert_eq!(inst.missing, vec!["PADDLE_API_KEY"]);
    assert!(
        invalid(
            f.market
                .update_instance(
                    USER,
                    &inst.id,
                    UpdateMcpMarketInstanceRequest {
                        status: Some("active".into()),
                        ..Default::default()
                    }
                )
                .await
                .unwrap_err()
        )
        .contains("required env PADDLE_API_KEY")
    );
    let status = f
        .market
        .set_secret(USER, &inst.id, "PADDLE_API_KEY", FIXTURE_SECRET)
        .await
        .unwrap();
    assert!(status.set);
    assert_eq!(status.name, "TB_PADDLE_READ__PADDLE_API_KEY");
    activate(&f, &inst.id).await;
    assign(&f, &inst.id, Some("proj-a"), None, &["claude"]).await.unwrap();

    // Nothing the service returns carries the value.
    let mut dumps = vec![
        serde_json::to_string(&f.market.list_instances(USER).await.unwrap()).unwrap(),
        serde_json::to_string(&f.market.get_instance(USER, &inst.id).await.unwrap()).unwrap(),
        serde_json::to_string(&f.market.list_secret_status(USER).await.unwrap()).unwrap(),
        serde_json::to_string(&f.market.status(USER).await.unwrap()).unwrap(),
        f.market
            .snapshot(USER, Some("proj-a"), false)
            .await
            .unwrap()
            .to_string(),
        serde_json::to_string(&f.config.list_servers(USER).await.unwrap()).unwrap(),
    ];
    dumps.push(format!("{:?}", f.repo.list_secrets(USER).await.unwrap()));
    for dump in &dumps {
        assert!(!dump.contains("FIXTURESECRET"), "secret leaked: {dump}");
    }
    // Stored encrypted.
    let raw: Vec<String> = sqlx::query_scalar("SELECT value_encrypted FROM mcp_secrets")
        .fetch_all(f.db.pool())
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    assert!(!raw[0].contains("FIXTURESECRET"));
    assert_eq!(aionui_common::decrypt_string(&raw[0], &KEY).unwrap(), FIXTURE_SECRET);
    let all_rows: String = sqlx::query_scalar("SELECT group_concat(transport_config || coalesce(market_plain_env,'') || coalesce(market_secret_refs,'')) FROM mcp_servers")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert!(!all_rows.contains("FIXTURESECRET"));

    // Undeclared keys and bad values.
    assert!(matches!(
        f.market.set_secret(USER, &inst.id, "OTHER_KEY", "x").await.unwrap_err(),
        MarketError::NotFound(_)
    ));
    assert!(matches!(
        f.market
            .set_secret(USER, &inst.id, "PADDLE_API_KEY", "")
            .await
            .unwrap_err(),
        MarketError::Invalid(_)
    ));
    // Clearing a required secret of an active instance makes it dormant.
    let cleared = f.market.clear_secret(USER, &inst.id, "PADDLE_API_KEY").await.unwrap();
    assert!(!cleared.set);
    assert_eq!(f.market.get_instance(USER, &inst.id).await.unwrap().status, "dormant");
}

// -- R6 --------------------------------------------------------------------------------

#[tokio::test]
async fn r6_allowlist_and_enforcement() {
    let f = fx().await;
    let mut req = paddle_read("tb-r6");
    req.tools_allow = None;
    assert!(invalid(f.market.create_instance(USER, req).await.unwrap_err()).contains("allowlist"));
    let mut req = paddle_write("tb-r6w");
    req.tools_allow = Some(vec!["x".into()]);
    assert!(invalid(f.market.create_instance(USER, req).await.unwrap_err()).contains("tools_block"));
    let duck = CreateMcpMarketInstanceRequest {
        name: "research-duck".into(),
        template_id: "duck".into(),
        description: None,
        environment: "production".into(),
        tier: "read".into(),
        status: None,
        clis: vec!["claude".into()],
        plain_env: BTreeMap::new(),
        tools_allow: Some(vec!["search".into()]),
        tools_block: vec![],
    };
    assert!(invalid(f.market.create_instance(USER, duck.clone()).await.unwrap_err()).contains("server|credential"));
    f.market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                environment: "local".into(),
                ..duck
            },
        )
        .await
        .unwrap();
    // Tool-name length (R1 #14).
    let mut req = paddle_read("tb-r6-long-name");
    req.tools_allow = Some(vec!["a_really_long_tool_name_for_the_limit_check_x".into()]);
    assert!(invalid(f.market.create_instance(USER, req).await.unwrap_err()).contains("chars"));
}

// -- R7 ------------------------------------------------------------------------------------

#[tokio::test]
async fn r7_one_channel_per_project_instance_cli() {
    let f = fx().await;
    let inst = f.market.create_instance(USER, paddle_read("tb-r7")).await.unwrap();
    assign(&f, &inst.id, Some("proj-a"), None, &["claude"]).await.unwrap();
    assert!(
        invalid(
            assign(&f, &inst.id, Some("proj-a"), Some("asst-1"), &["claude"])
                .await
                .unwrap_err()
        )
        .contains("R7")
    );
    // Another CLI or project is fine.
    assign(&f, &inst.id, Some("proj-a"), Some("asst-1"), &["codex"])
        .await
        .unwrap();
    assign(&f, &inst.id, Some("proj-b"), Some("asst-1"), &["claude"])
        .await
        .unwrap();
    assert!(matches!(
        assign(&f, &inst.id, Some("proj-a"), None, &["claude"])
            .await
            .unwrap_err(),
        MarketError::Conflict(_)
    ));
    assert!(matches!(
        assign(&f, &inst.id, Some("proj-zzz"), None, &["claude"])
            .await
            .unwrap_err(),
        MarketError::NotFound(_)
    ));
    assert!(
        invalid(
            assign(&f, &inst.id, Some("proj-a"), None, &["gemini"])
                .await
                .unwrap_err()
        )
        .contains("not enabled")
    );
    // Dropping a CLI that still has assignments is refused.
    let err = f
        .market
        .update_instance(
            USER,
            &inst.id,
            UpdateMcpMarketInstanceRequest {
                clis: Some(vec!["claude".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(invalid(err).contains("codex"));
}

// -- R8, R1 #10 ---------------------------------------------------------------------------

#[tokio::test]
async fn r8_and_r1_10_generic_routes_refuse_market_names() {
    let f = fx().await;
    let inst = f.market.create_instance(USER, paddle_read("tb-guard")).await.unwrap();
    let generic = CreateMcpServerRequest {
        name: "tb-guard".into(),
        description: None,
        transport: McpTransport::Stdio {
            command: "/bin/sh".into(),
            args: vec![],
            env: Default::default(),
        },
        original_json: None,
        builtin: false,
    };
    assert!(matches!(
        f.config.add_server(USER, generic).await.unwrap_err(),
        McpError::Conflict(_)
    ));
    assert!(matches!(
        f.config.toggle_server(USER, &inst.id).await.unwrap_err(),
        McpError::Conflict(_)
    ));
    assert!(matches!(
        f.config
            .edit_server(
                USER,
                &inst.id,
                UpdateMcpServerRequest {
                    name: None,
                    description: None,
                    transport: None,
                    original_json: None,
                    builtin: None
                }
            )
            .await
            .unwrap_err(),
        McpError::Conflict(_)
    ));
    assert!(matches!(
        f.config.delete_server(USER, &inst.id).await.unwrap_err(),
        McpError::Conflict(_)
    ));
    let import = BatchImportMcpServersRequest {
        servers: vec![ImportMcpServerRequest {
            name: "tb-guard".into(),
            description: None,
            transport: McpTransport::Http {
                url: "https://example.com".into(),
                headers: Default::default(),
            },
            original_json: None,
            builtin: false,
            enabled: Some(true),
        }],
    };
    assert!(matches!(
        f.config.batch_import(USER, import).await.unwrap_err(),
        McpError::Conflict(_)
    ));

    // Soft-deleted market rows stay protected from the generic upsert.
    activate(&f, &inst.id).await;
    assign(&f, &inst.id, Some("proj-a"), None, &["claude"]).await.unwrap();
    f.market.delete_instance(USER, &inst.id).await.unwrap();
    let generic = CreateMcpServerRequest {
        name: "tb-guard".into(),
        description: None,
        transport: McpTransport::Stdio {
            command: "/bin/sh".into(),
            args: vec![],
            env: Default::default(),
        },
        original_json: None,
        builtin: false,
    };
    assert!(matches!(
        f.config.add_server(USER, generic).await.unwrap_err(),
        McpError::Conflict(_)
    ));

    // Market revive resets the market columns and drops old assignments/secrets.
    let revived = f.market.create_instance(USER, paddle_write("tb-guard")).await.unwrap();
    assert_eq!(revived.id, inst.id);
    assert_eq!(revived.tier, "write");
    assert_eq!(revived.status, "dormant");
    assert!(revived.secrets.iter().all(|s| !s.set));
    assert!(
        f.market
            .list_assignments(USER, None, Some(&inst.id))
            .await
            .unwrap()
            .is_empty()
    );
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM mcp_servers WHERE id = ?")
        .bind(&inst.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert!(!enabled, "R8");
}

#[tokio::test]
async fn duplicate_copies_settings_not_secrets() {
    let f = fx().await;
    let inst = f.market.create_instance(USER, paddle_read("tb-src")).await.unwrap();
    activate(&f, &inst.id).await;
    let dup = f.market.duplicate_instance(USER, &inst.id, "tb-copy").await.unwrap();
    assert_eq!(dup.status, "dormant");
    assert_eq!(dup.tools_allow, Some(vec!["list_products".to_owned()]));
    assert_eq!(dup.secrets[0].name, "TB_COPY__PADDLE_API_KEY");
    assert!(!dup.secrets[0].set);
}

// -- R1 #11 / #12 -------------------------------------------------------------------------

#[tokio::test]
async fn r1_11_legacy_rows_are_masked_and_round_trip() {
    let f = fx().await;
    let created = f
        .config
        .add_server(
            USER,
            CreateMcpServerRequest {
                name: "legacy".into(),
                description: None,
                transport: McpTransport::Stdio {
                    command: "node".into(),
                    args: vec!["srv.js".into()],
                    env: [
                        ("STRIPE_API_KEY".to_owned(), LEGACY_SECRET.to_owned()),
                        ("MODE".to_owned(), "x".to_owned()),
                    ]
                    .into_iter()
                    .collect(),
                },
                original_json: Some(format!(r#"{{"env":{{"STRIPE_API_KEY":"{LEGACY_SECRET}"}}}}"#)),
                builtin: false,
            },
        )
        .await
        .unwrap();
    let dump = serde_json::to_string(&created).unwrap();
    assert!(!dump.contains("LEGACYFIXTURE"), "{dump}");
    let listed = serde_json::to_string(&f.config.list_servers(USER).await.unwrap()).unwrap();
    assert!(!listed.contains("LEGACYFIXTURE"));
    // PUT sends the masked transport back: the stored value is kept.
    let edited = f
        .config
        .edit_server(
            USER,
            &created.id,
            UpdateMcpServerRequest {
                name: None,
                description: None,
                transport: Some(created.transport.clone()),
                original_json: Some(created.original_json.clone()),
                builtin: None,
            },
        )
        .await
        .unwrap();
    assert!(!serde_json::to_string(&edited).unwrap().contains("LEGACYFIXTURE"));
    let stored = f.config.stored_transport(USER, &created.id).await.unwrap().unwrap();
    match stored {
        McpServerTransport::Stdio { env, .. } => assert_eq!(env["STRIPE_API_KEY"], LEGACY_SECRET),
        other => panic!("{other:?}"),
    }
    let raw: Option<String> = sqlx::query_scalar("SELECT original_json FROM mcp_servers WHERE id = ?")
        .bind(&created.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert!(
        raw.unwrap().contains("LEGACYFIXTURE"),
        "masked original_json sent back keeps the stored text"
    );
    // Creating/importing a masked value is refused.
    let err = f
        .config
        .add_server(
            USER,
            CreateMcpServerRequest {
                name: "masked".into(),
                description: None,
                transport: created.transport.clone(),
                original_json: None,
                builtin: false,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::MaskedValue(_)), "{err:?}");
}

#[tokio::test]
async fn r1_12_test_transport_is_resolved_server_side() {
    let f = fx().await;
    let inst = f.market.create_instance(USER, paddle_read("tb-test")).await.unwrap();
    f.market
        .set_secret(USER, &inst.id, "PADDLE_API_KEY", FIXTURE_SECRET)
        .await
        .unwrap();
    match f.market.test_transport(USER, &inst.id).await.unwrap() {
        McpServerTransport::Stdio { command, args, env } => {
            assert_eq!(command, "npx");
            assert_eq!(args, vec!["-y", "@paddle/paddle-mcp@0.1.0"]);
            assert_eq!(env["PADDLE_API_KEY"], FIXTURE_SECRET);
            assert_eq!(env["PADDLE_MCP_TOOLS"], "read-only");
        }
        other => panic!("{other:?}"),
    }
    assert!(f.market.is_market_row(USER, &inst.id).await.unwrap());
}

// -- launcher decision (R9, R1 #1) ----------------------------------------------------------

fn req<'a>(instance: &'a str, scope: LaunchScope<'a>, cli: &'a str) -> LaunchRequest<'a> {
    LaunchRequest {
        user_id: USER,
        instance,
        scope,
        cli,
    }
}

fn project(p: &str) -> LaunchScope<'_> {
    LaunchScope::Project {
        project_id: p,
        assistant_id: None,
    }
}

fn forbidden(err: MarketError) -> String {
    match err {
        MarketError::Forbidden(msg) => msg,
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

#[tokio::test]
async fn launcher_refuses_unless_active_and_assigned_now() {
    let f = fx().await;
    let inst = f.market.create_instance(USER, paddle_read("tb-launch")).await.unwrap();
    f.market
        .set_secret(USER, &inst.id, "PADDLE_API_KEY", FIXTURE_SECRET)
        .await
        .unwrap();
    let r = req("tb-launch", project("proj-a"), "claude");
    // Dormant.
    assert!(forbidden(resolve_launch(f.repo.as_ref(), &KEY, &r).await.unwrap_err()).contains("dormant"));
    activate(&f, &inst.id).await;
    // Active but unassigned.
    assert!(forbidden(resolve_launch(f.repo.as_ref(), &KEY, &r).await.unwrap_err()).contains("not assigned"));
    assign(&f, &inst.id, Some("proj-a"), None, &["claude"]).await.unwrap();
    let plan = resolve_launch(f.repo.as_ref(), &KEY, &r).await.unwrap();
    assert_eq!(plan.command, "npx", "the template's pinned command");
    assert_eq!(plan.args, vec!["-y", "@paddle/paddle-mcp@0.1.0"]);
    assert!(
        plan.env
            .contains(&("PADDLE_API_KEY".to_owned(), FIXTURE_SECRET.to_owned()))
    );
    assert!(plan.policy.permits("list_products") && !plain_permits(&plan, "create_refund"));
    assert!(
        !format!("{plan:?}").contains("FIXTURESECRET"),
        "Debug prints key names only"
    );
    assert!(still_allowed(f.repo.as_ref(), &r).await);

    // Other project / CLI / channel / global: refused.
    for other in [
        req("tb-launch", project("proj-b"), "claude"),
        req("tb-launch", project("proj-a"), "codex"),
        req(
            "tb-launch",
            LaunchScope::Project {
                project_id: "proj-a",
                assistant_id: Some("asst-1"),
            },
            "claude",
        ),
        req("tb-launch", LaunchScope::Global, "claude"),
        req("not-an-instance", project("proj-a"), "claude"),
    ] {
        assert!(
            resolve_launch(f.repo.as_ref(), &KEY, &other).await.is_err(),
            "{other:?}"
        );
    }
    // Revoked.
    f.market.revoke_instance(USER, &inst.id).await.unwrap();
    assert!(forbidden(resolve_launch(f.repo.as_ref(), &KEY, &r).await.unwrap_err()).contains("revoked"));
    assert!(!still_allowed(f.repo.as_ref(), &r).await);
    // Re-activated, then deleted.
    f.market
        .update_instance(
            USER,
            &inst.id,
            UpdateMcpMarketInstanceRequest {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(still_allowed(f.repo.as_ref(), &r).await);
    f.market.delete_instance(USER, &inst.id).await.unwrap();
    assert!(forbidden(resolve_launch(f.repo.as_ref(), &KEY, &r).await.unwrap_err()).contains("deleted"));
    // A non-market row is never launched.
    let plain = f
        .config
        .add_server(
            USER,
            CreateMcpServerRequest {
                name: "plain-row".into(),
                description: None,
                transport: McpTransport::Stdio {
                    command: "node".into(),
                    args: vec![],
                    env: Default::default(),
                },
                original_json: None,
                builtin: false,
            },
        )
        .await
        .unwrap();
    assert!(!plain.enabled);
    assert!(
        resolve_launch(f.repo.as_ref(), &KEY, &req("plain-row", project("proj-a"), "claude"))
            .await
            .is_err()
    );
}

fn plain_permits(plan: &aionui_mcp::market::LaunchPlan, tool: &str) -> bool {
    plan.policy.permits(tool)
}

#[tokio::test]
async fn launcher_global_only_for_global_ids_and_refuses_write_on_codex() {
    let f = fx().await;
    let ctx = f
        .market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                name: "context7".into(),
                template_id: "context7".into(),
                description: None,
                environment: "production".into(),
                tier: "read".into(),
                status: Some("active".into()),
                clis: vec!["claude".into(), "codex".into()],
                plain_env: BTreeMap::new(),
                tools_allow: Some(vec!["resolve-library-id".into()]),
                tools_block: vec![],
            },
        )
        .await
        .unwrap();
    assign(&f, &ctx.id, None, None, &["codex"]).await.unwrap();
    // HTTP template without a stdio bridge: no launch command.
    let err = resolve_launch(f.repo.as_ref(), &KEY, &req("context7", LaunchScope::Global, "codex"))
        .await
        .unwrap_err();
    assert!(forbidden(err).contains("no stdio command"));

    let w = f.market.create_instance(USER, paddle_write("tb-w")).await.unwrap();
    f.market
        .set_approval(USER, &w.id, Some("OA-20261115-tb-w".into()))
        .await
        .unwrap();
    activate(&f, &w.id).await;
    assign(&f, &w.id, Some("proj-a"), None, &["claude"]).await.unwrap();
    // Even if someone inserted a codex row behind the service's back, the launcher refuses.
    sqlx::query("INSERT INTO mcp_assignments (id, user_id, mcp_server_id, project_id, assistant_id, cli, created_at, updated_at) VALUES ('x', ?, ?, 'proj-a', NULL, 'codex', 1, 1)")
        .bind(USER)
        .bind(&w.id)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(
        resolve_launch(f.repo.as_ref(), &KEY, &req("tb-w", project("proj-a"), "codex"))
            .await
            .is_err()
    );
    let plan = resolve_launch(f.repo.as_ref(), &KEY, &req("tb-w", project("proj-a"), "claude"))
        .await
        .unwrap();
    assert!(
        plan.policy.permits("create_refund") && !plan.policy.permits("delete_product"),
        "write = blocklist"
    );
}

// -- snapshot = the studio sync lock input ---------------------------------------------------

#[tokio::test]
async fn snapshot_matches_the_generator_lock_shape() {
    let f = fx().await;
    let inst = f
        .market
        .create_instance(USER, paddle_read("tb-paddle-read"))
        .await
        .unwrap();
    activate(&f, &inst.id).await;
    assign(&f, &inst.id, Some("proj-a"), None, &["claude", "codex"])
        .await
        .unwrap();
    assign(&f, &inst.id, Some("proj-b"), Some("asst-1"), &["claude"])
        .await
        .unwrap();
    let lock = f.market.snapshot(USER, Some("proj-a"), false).await.unwrap();
    assert_eq!(lock["scope"], "project");
    assert_eq!(lock["project"]["id"], "proj-a");
    let i = &lock["instances"][0];
    assert_eq!(i["id"], "tb-paddle-read");
    assert_eq!(i["clis"], json!(["claude", "codex"]));
    assert_eq!(i["env"]["PADDLE_API_KEY"], "${TB_PADDLE_READ__PADDLE_API_KEY}");
    assert_eq!(i["command"], "npx");
    assert_eq!(i["tools"]["allow"], json!(["list_products"]));
    // proj-b only has an assistant row: the AionUi channel, not the native lock.
    let lock_b = f.market.snapshot(USER, Some("proj-b"), false).await.unwrap();
    assert_eq!(lock_b["instances"], json!([]));

    // Cross-check with the real generator validator when it is installed here.
    let schema = "/root/projects/clauterm-studio/tools/studio/lib/schema.js";
    if std::path::Path::new(schema).exists() && which_node().is_some() {
        let script = format!(
            "const s=require({schema:?}); const lock=JSON.parse(process.argv[1]); try {{ s.validate(lock,{{scope:'project'}}); console.log('ok'); }} catch(e) {{ console.log(JSON.stringify(e.problems||e.message)); process.exit(1); }}"
        );
        let out = std::process::Command::new(which_node().unwrap())
            .args(["-e", &script, &lock.to_string()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "generator rejected the snapshot: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

fn which_node() -> Option<String> {
    ["/usr/local/bin/node", "/usr/bin/node"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(str::to_owned)
}

// -- template refresh (R10) -------------------------------------------------------------

struct SwappableCatalog(std::sync::Mutex<Vec<serde_json::Value>>);

#[async_trait::async_trait]
impl aionui_mcp::IMcpTemplateCatalog for SwappableCatalog {
    async fn entries(&self, _user_id: &str) -> Result<Vec<serde_json::Value>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
}

fn paddle_version(version: &str, pkg: &str, env: serde_json::Value) -> serde_json::Value {
    json!({"id": "paddle", "name": "Paddle", "market": {
        "version": version, "transport": "stdio",
        "command": {"command": "npx", "args": ["-y", pkg]},
        "env": env, "tiers": ["read"], "enforcement": "server", "clis": ["claude"]
    }})
}

#[tokio::test]
async fn template_refresh_keeps_secrets_and_assignments_and_needs_new_secrets_first() {
    let db = init_database_memory().await.unwrap();
    sqlx::query("INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)")
        .execute(db.pool())
        .await
        .unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let v1 = paddle_version(
        "0.1.0",
        "@paddle/paddle-mcp@0.1.0",
        json!({"PADDLE_API_KEY": {"kind": "secret", "required": true}, "OLD_PLAIN": {"kind": "plain"}}),
    );
    let catalog = Arc::new(SwappableCatalog(std::sync::Mutex::new(vec![v1])));
    let market = McpMarketService::new(repo.clone(), servers, catalog.clone(), KEY);
    let mut req = paddle_read("tb-refresh");
    req.clis = vec!["claude".into()];
    req.plain_env = BTreeMap::from([("OLD_PLAIN".into(), "x".into())]);
    let inst = market.create_instance(USER, req).await.unwrap();
    assert_eq!(inst.template_version.as_deref(), Some("0.1.0"));
    assert_eq!(inst.template_current_version.as_deref(), Some("0.1.0"));
    assert!(inst.template_in_catalogue && !inst.template_update_available);
    market
        .set_secret(USER, &inst.id, "PADDLE_API_KEY", FIXTURE_SECRET)
        .await
        .unwrap();
    market
        .update_instance(
            USER,
            &inst.id,
            UpdateMcpMarketInstanceRequest {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    market
        .create_assignments(
            USER,
            CreateMcpMarketAssignmentRequest {
                instance_id: inst.id.clone(),
                project_id: Some("proj-a".into()),
                assistant_id: None,
                clis: vec!["claude".into()],
            },
        )
        .await
        .unwrap();

    // v2: OLD_PLAIN removed, NEW_SECRET (required) and NEW_MODE (default) added, new package pin.
    *catalog.0.lock().unwrap() = vec![paddle_version(
        "0.2.0",
        "@paddle/paddle-mcp@0.2.0",
        json!({"PADDLE_API_KEY": {"kind": "secret", "required": true},
               "NEW_SECRET": {"kind": "secret", "required": true},
               "NEW_MODE": {"kind": "plain", "default": "safe"}}),
    )];
    // List and detail now report the pending update.
    let detail = market.get_instance(USER, &inst.id).await.unwrap();
    let listed = market.list_instances(USER).await.unwrap().remove(0);
    for view in [&detail, &listed] {
        assert_eq!(view.template_version.as_deref(), Some("0.1.0"));
        assert_eq!(view.template_current_version.as_deref(), Some("0.2.0"));
        assert!(view.template_in_catalogue && view.template_update_available);
    }
    let launch = LaunchRequest {
        user_id: USER,
        instance: "tb-refresh",
        scope: project("proj-a"),
        cli: "claude",
    };

    // A required new secret is missing: refused, the instance keeps v1.
    let err = market.refresh_template(USER, &inst.id).await.unwrap_err();
    assert!(
        matches!(&err, MarketError::TemplateSecretMissing(m) if m.contains("NEW_SECRET")),
        "{err:?}"
    );
    let plan = resolve_launch(repo.as_ref(), &KEY, &launch).await.unwrap();
    assert_eq!(plan.args, vec!["-y", "@paddle/paddle-mcp@0.1.0"]);

    // The new secret can be set before the refresh; the running v1 never receives it.
    market
        .set_secret(USER, &inst.id, "NEW_SECRET", "new_secret_value_FIXTURE_v2")
        .await
        .unwrap();
    let plan = resolve_launch(repo.as_ref(), &KEY, &launch).await.unwrap();
    assert!(!plan.env.iter().any(|(k, _)| k == "NEW_SECRET"));

    let refreshed = market.refresh_template(USER, &inst.id).await.unwrap();
    assert!(refreshed.changed);
    assert_eq!(refreshed.from_version.as_deref(), Some("0.1.0"));
    assert_eq!(refreshed.to_version.as_deref(), Some("0.2.0"));
    assert_eq!(refreshed.added_keys, vec!["NEW_MODE", "NEW_SECRET"]);
    assert_eq!(refreshed.removed_keys, vec!["OLD_PLAIN"]);
    assert_eq!(refreshed.instance.status, "active");
    assert!(!refreshed.instance.plain_env.contains_key("OLD_PLAIN"));
    assert_eq!(refreshed.instance.plain_env["NEW_MODE"], "safe");
    assert!(refreshed.instance.secrets.iter().all(|s| s.set), "secrets kept");
    assert_eq!(
        market.list_assignments(USER, None, Some(&inst.id)).await.unwrap().len(),
        1,
        "assignments kept"
    );
    let plan = resolve_launch(repo.as_ref(), &KEY, &launch).await.unwrap();
    assert_eq!(plan.args, vec!["-y", "@paddle/paddle-mcp@0.2.0"]);
    assert!(
        plan.env
            .contains(&("PADDLE_API_KEY".to_owned(), FIXTURE_SECRET.to_owned()))
    );
    assert!(plan.env.iter().any(|(k, _)| k == "NEW_SECRET"));
    assert!(!serde_json::to_string(&refreshed).unwrap().contains("FIXTURE"));

    assert_eq!(refreshed.instance.template_version.as_deref(), Some("0.2.0"));
    assert!(!refreshed.instance.template_update_available);
    assert!(!market.get_instance(USER, &inst.id).await.unwrap().template_update_available);

    // Refreshing again is a no-op.
    let again = market.refresh_template(USER, &inst.id).await.unwrap();
    assert!(!again.changed && again.added_keys.is_empty() && again.removed_keys.is_empty());

    // Template withdrawn from the catalogue: the instance still lists, no update offered.
    catalog.0.lock().unwrap().clear();
    let listed = market.list_instances(USER).await.unwrap().remove(0);
    assert!(!listed.template_in_catalogue && !listed.template_update_available);
    assert_eq!(listed.template_version.as_deref(), Some("0.2.0"));
    assert!(listed.template_current_version.is_none());
}
