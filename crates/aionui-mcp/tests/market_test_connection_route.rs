//! TASK-70 (train-2 review C2 #3): `POST /api/mcp/test-connection {id}` of an
//! MCP market row starts the instance with its decrypted secrets, so it
//! - spawns a stdio child with the launcher's env only (no `AIONUI_*`),
//! - refuses a revoked instance,
//! - refuses agents (runtime-token channel, or the header in local identity).

use std::collections::BTreeMap;
use std::sync::Arc;

use aionui_api_types::CreateMcpMarketInstanceRequest;
use aionui_auth::{AuthChannel, CurrentUser};
use aionui_db::{
    IMcpMarketRepository, IMcpServerRepository, SqliteMcpMarketRepository, SqliteMcpServerRepository,
    SqliteOAuthTokenRepository, init_database_memory,
};
use aionui_mcp::{
    McpConfigService, McpConnectionTestService, McpMarketService, McpOAuthService, McpRouterState, McpSyncService,
    StaticTemplateCatalog, mcp_routes,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Extension, Router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const USER: &str = "system_default_user";
const LEAK: &str = "AIONUI_TASK70_PARENT_SECRET";

struct Fx {
    market: McpMarketService,
    state: McpRouterState,
    id: String,
    out: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn fx() -> Fx {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    // A template whose "server" only writes its environment to $OUT and exits.
    let catalog = StaticTemplateCatalog::new(vec![json!({"id": "envdump", "name": "Env dump", "market": {
        "transport": "stdio",
        "command": {"command": "sh", "args": ["-c", "env > \"$OUT\""]},
        "env": {"OUT": {"kind": "plain", "required": true}},
        "tiers": ["read"], "enforcement": "tools-only", "clis": ["claude"]
    }})]);
    let market = McpMarketService::new(repo.clone(), servers.clone(), Arc::new(catalog), [3u8; 32]);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.txt");
    let created = market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                name: "env-dump".into(),
                template_id: "envdump".into(),
                description: None,
                environment: "sandbox".into(),
                tier: "read".into(),
                status: None,
                clis: vec!["claude".into()],
                plain_env: BTreeMap::from([("OUT".into(), out.to_string_lossy().into_owned())]),
                tools_allow: Some(vec!["dump".into()]),
                tools_block: vec![],
            },
        )
        .await
        .unwrap();
    let state = McpRouterState {
        config_service: McpConfigService::new(servers.clone()).with_market_repo(repo),
        sync_service: McpSyncService::new(servers, vec![]),
        connection_test_service: McpConnectionTestService::new(
            reqwest::Client::new(),
            Arc::new(aionui_realtime::BroadcastEventBus::new(16)),
        ),
        oauth_service: McpOAuthService::new(
            Arc::new(SqliteOAuthTokenRepository::new(db.pool().clone())),
            reqwest::Client::new(),
        ),
        market: market.clone(),
    };
    Fx {
        market,
        state,
        id: created.id,
        out,
        _dir: dir,
    }
}

fn app(f: &Fx, channel: AuthChannel) -> Router {
    mcp_routes(f.state.clone())
        .layer(Extension(channel))
        .layer(Extension(CurrentUser::local_default()))
}

fn test_request(f: &Fx, runtime_header: bool) -> Request<Body> {
    let body = json!({"id": f.id, "name": "env-dump",
        "transport": {"type": "stdio", "command": "ignored", "args": [], "env": {}}});
    let mut builder = Request::post("/api/mcp/test-connection").header("content-type", "application/json");
    if runtime_header {
        builder = builder.header("x-aionui-runtime-token", "rt-fake");
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn send(app: Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn market_test_connection_isolates_env_and_refuses_agents_and_revoked_instances() {
    // SAFETY: this test binary has a single test; nothing reads the env concurrently.
    unsafe { std::env::set_var(LEAK, "leaked-value") };
    let f = fx().await;

    // Owner's browser: the child runs with the launcher env only.
    // The dump "server" exits without answering, so the test itself reports a
    // protocol error; what matters is that it ran, and with which env.
    let (status, body) = send(app(&f, AuthChannel::Session), test_request(&f, false)).await;
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
    let env = std::fs::read_to_string(&f.out).expect("the test child wrote its env");
    assert!(!env.contains(LEAK), "parent AIONUI_* leaked into the market test child");
    assert!(env.lines().any(|l| l.starts_with("OUT=")), "instance env present");
    std::fs::remove_file(&f.out).unwrap();

    // Agents: runtime-token channel, or the header in local identity.
    for (channel, header) in [
        (
            AuthChannel::RuntimeToken {
                conversation_id: "c1".into(),
            },
            false,
        ),
        (AuthChannel::Local, true),
    ] {
        let (status, body) = send(app(&f, channel), test_request(&f, header)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "MCP_MARKET_SESSION_REQUIRED");
        assert!(!f.out.exists(), "an agent started the instance");
    }

    // Revoked: never started.
    f.market.revoke_instance(USER, &f.id).await.unwrap();
    let (status, body) = send(app(&f, AuthChannel::Session), test_request(&f, false)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "MCP_MARKET_FORBIDDEN");
    assert!(!f.out.exists(), "a revoked instance was started");
}
