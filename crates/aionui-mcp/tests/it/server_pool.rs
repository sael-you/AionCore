//! Server access pools (card C36, TASK-130) against a real in-memory DB:
//! owner CRUD with a write-only key, host-key reset on a new destination,
//! `server` attachments through the kit levels (incl. the production/global
//! rule), the effective set, and the per-call grant used by the helper.

use std::sync::Arc;

use aionui_api_types::{
    AttachKitItemRequest, CreateServerRequest, KitEffectiveQuery, KitErrorCode, KitItemKind, KitLevel, KitOp,
    UpdateServerRequest,
};
use aionui_db::{
    IKitRepository, IMcpMarketRepository, IMcpServerRepository, IServerPoolRepository, SqliteKitRepository,
    SqliteMcpMarketRepository, SqliteMcpServerRepository, SqliteServerPoolRepository, init_database_memory,
};
use aionui_mcp::market::MarketError;
use aionui_mcp::server_pool::{ServerPoolError, ServerPoolService, granted_servers};
use aionui_mcp::{McpMarketService, StaticTemplateCatalog};

const USER: &str = "system_default_user";
const KEY: [u8; 32] = [7u8; 32];
const PRIVATE_KEY: &str =
    "-----BEGIN OPENSSH PRIVATE KEY-----\nFIXTUREKEYMATERIAL0123456789\n-----END OPENSSH PRIVATE KEY-----";
const HOST_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixture";

struct Fx {
    db: aionui_db::Database,
    pool: ServerPoolService,
    market: McpMarketService,
    kits: Arc<dyn IKitRepository>,
    mcp: Arc<dyn IMcpServerRepository>,
}

async fn fx() -> Fx {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IServerPoolRepository> = Arc::new(SqliteServerPoolRepository::new(db.pool().clone()));
    let pool = ServerPoolService::new(repo, KEY);
    let market_repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let mcp: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let kits: Arc<dyn IKitRepository> = Arc::new(SqliteKitRepository::new(db.pool().clone()));
    let market = McpMarketService::new(
        market_repo,
        mcp.clone(),
        Arc::new(StaticTemplateCatalog::new(vec![])),
        KEY,
    )
    .with_kits(kits.clone())
    .with_server_pool(pool.clone());
    for sql in [
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)",
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-b', 'system_default_user', 'B', 'standard', 1, 1)",
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) \
         VALUES ('conv-a', 'system_default_user', 'c', 'acp', 'pending', 1, 1, 'proj-a', '{}')",
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) \
         VALUES ('conv-b', 'system_default_user', 'c', 'acp', 'pending', 1, 1, 'proj-b', '{}')",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    Fx {
        db,
        pool,
        market,
        kits,
        mcp,
    }
}

fn create(name: &str, environment: &str) -> CreateServerRequest {
    CreateServerRequest {
        name: name.into(),
        host: "10.0.0.5".into(),
        port: None,
        ssh_user: "deploy".into(),
        private_key: PRIVATE_KEY.into(),
        host_key: Some(HOST_KEY.into()),
        labels: Some(vec!["web".into()]),
        environment: Some(environment.into()),
        tier: Some("write".into()),
        notes: None,
        enabled: None,
    }
}

fn attach(level: KitLevel, target: &str, id: &str, op: KitOp) -> AttachKitItemRequest {
    AttachKitItemRequest {
        level,
        target_id: (!target.is_empty()).then(|| target.to_owned()),
        item_kind: KitItemKind::Server,
        item_ref: id.to_owned(),
        op,
    }
}

#[tokio::test]
async fn key_is_write_only_and_encrypted_at_rest() {
    let f = fx().await;
    let s = f.pool.create(USER, create("web-1", "production")).await.unwrap();
    assert!(s.has_key);
    assert!(s.crucial_commands_ask, "production + write");
    assert!(!s.tier_note.is_empty());
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains("FIXTUREKEYMATERIAL"), "response never carries the key");

    let stored: String = sqlx::query_scalar("SELECT value_encrypted FROM server_secrets WHERE server_id = ?")
        .bind(&s.id)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert!(!stored.contains("FIXTUREKEYMATERIAL"), "ciphertext at rest");
    assert!(
        f.pool
            .private_key(USER, &s.id)
            .await
            .unwrap()
            .unwrap()
            .contains("FIXTUREKEYMATERIAL"),
        "the helper can decrypt it"
    );

    // Bad input is refused with field messages.
    let mut bad = create("web-2", "production");
    bad.host = "-oProxyCommand=sh".into();
    assert!(matches!(f.pool.create(USER, bad).await, Err(ServerPoolError::Invalid(m)) if m.starts_with("host")));
    assert!(matches!(
        f.pool.create(USER, create("web-1", "staging")).await,
        Err(ServerPoolError::NameTaken(_))
    ));
}

#[tokio::test]
async fn new_destination_drops_the_old_host_key_but_partial_updates_keep_fields() {
    let f = fx().await;
    let s = f.pool.create(USER, create("web-1", "staging")).await.unwrap();
    let off = f
        .pool
        .update(
            USER,
            &s.id,
            UpdateServerRequest {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!off.enabled);
    assert_eq!(off.host_key, HOST_KEY);
    assert_eq!(off.labels, vec!["web"]);

    let moved = f
        .pool
        .update(
            USER,
            &s.id,
            UpdateServerRequest {
                host: Some("10.0.0.6".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.host_key, "", "the old host key is never trusted for a new host");
    assert!(moved.has_key);
    assert!(matches!(
        f.pool.update(USER, "srv_missing", UpdateServerRequest::default()).await,
        Err(ServerPoolError::NotFound(_))
    ));
}

#[tokio::test]
async fn attachments_grant_per_conversation_and_follow_changes() {
    let f = fx().await;
    let s = f.pool.create(USER, create("web-1", "production")).await.unwrap();
    let granted = |conv: &'static str| {
        let kits = f.kits.clone();
        let pool = f.pool.clone();
        async move {
            granted_servers(kits.as_ref(), pool.repo().as_ref(), USER, conv)
                .await
                .unwrap()
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
        }
    };
    assert!(
        granted("conv-a").await.is_empty(),
        "nothing is delivered without an add"
    );

    // A production server is never global.
    let err = f
        .market
        .attach_kit_item(USER, attach(KitLevel::Global, "", &s.id, KitOp::Add))
        .await
        .unwrap_err();
    assert!(matches!(err, MarketError::Kit(KitErrorCode::GlobalNotAllowed, _)));

    let a = f
        .market
        .attach_kit_item(USER, attach(KitLevel::Project, "proj-a", &s.id, KitOp::Add))
        .await
        .unwrap();
    assert_eq!(a.item_kind, KitItemKind::Server);
    assert_eq!(a.item_name, "web-1");
    assert_eq!(granted("conv-a").await, vec!["web-1"]);
    assert!(granted("conv-b").await.is_empty(), "other project");

    let eff = f
        .market
        .kit_effective_set(
            USER,
            KitEffectiveQuery {
                conversation_id: Some("conv-a".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(eff.servers.len(), 1);
    assert!(eff.servers[0].included);

    // A conversation-level remove applies at the next call.
    f.market
        .attach_kit_item(USER, attach(KitLevel::Conversation, "conv-a", &s.id, KitOp::Remove))
        .await
        .unwrap();
    assert!(granted("conv-a").await.is_empty());

    // Unknown server ids are refused.
    let err = f
        .market
        .attach_kit_item(USER, attach(KitLevel::Project, "proj-a", "srv_nope", KitOp::Add))
        .await
        .unwrap_err();
    assert!(matches!(err, MarketError::Kit(KitErrorCode::InvalidItem, _)));
}

#[tokio::test]
async fn sessions_get_the_server_mcps_only_when_an_enabled_server_exists() {
    let f = fx().await;
    assert!(!f.mcp.server_pool_enabled(USER).await.unwrap());
    let s = f.pool.create(USER, create("web-1", "staging")).await.unwrap();
    assert!(f.mcp.server_pool_enabled(USER).await.unwrap());
    f.pool
        .update(
            USER,
            &s.id,
            UpdateServerRequest {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!f.mcp.server_pool_enabled(USER).await.unwrap());
}
