//! Auth matrix of `/api/mcp/market/*` (R1 #1): reads behind the ordinary auth
//! state, writes behind a session-only auth state (as `aionui-app` mounts
//! them, also in `--local`), plus the handlers' own channel check.

use std::sync::Arc;

use aionui_auth::{
    AuthIdentityMode, AuthState, CookieNames, IRuntimeTokenVerifier, JwtService, OriginGuard, OriginPolicy,
    auth_middleware, require_trusted_origin_middleware,
};
use aionui_db::{
    IMcpMarketRepository, IMcpServerRepository, IUserRepository, SqliteMcpMarketRepository, SqliteMcpServerRepository,
    SqliteUserRepository, init_database_memory,
};
use aionui_mcp::{
    McpMarketRouterState, McpMarketService, StaticTemplateCatalog, mcp_market_read_routes, mcp_market_write_routes,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::middleware::from_fn_with_state;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const USER: &str = "system_default_user";
const FIXTURE_SECRET: &str = "pdl_live_ROUTEFIXTURESECRET0123456789";
const ORIGIN: &str = "https://y.talkseeds.test";

struct AlwaysValid;
impl IRuntimeTokenVerifier for AlwaysValid {
    fn verify_conversation_helper(&self, _: &str, _: &str, _: &str) -> bool {
        true
    }
    /// Unscoped on purpose: these tests prove the market write router
    /// refuses the runtime-token channel by itself, not via the C13 scope.
    fn allows_route(&self, _: &axum::http::Method, _: &str) -> bool {
        true
    }
}

struct Fx {
    db: aionui_db::Database,
    jwt: Arc<JwtService>,
    users: Arc<dyn IUserRepository>,
    state: McpMarketRouterState,
}

async fn fx() -> Fx {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let catalog = StaticTemplateCatalog::new(vec![json!({"id": "paddle", "name": "Paddle", "market": {
        "transport": "stdio", "command": {"command": "npx", "args": ["-y", "@paddle/paddle-mcp@0.1.0"]},
        "env": {"PADDLE_API_KEY": {"kind": "secret", "required": true}},
        "tiers": ["read"], "enforcement": "server", "clis": ["claude"]
    }})]);
    let market = McpMarketService::new(repo, servers, Arc::new(catalog), [9u8; 32])
        .with_kits(Arc::new(aionui_db::SqliteKitRepository::new(db.pool().clone())))
        .with_server_pool(aionui_mcp::server_pool::ServerPoolService::new(
            Arc::new(aionui_db::SqliteServerPoolRepository::new(db.pool().clone())),
            [9u8; 32],
        ));
    Fx {
        db: db.clone(),
        jwt: Arc::new(JwtService::new("route-test-signing-secret".into())),
        users: Arc::new(SqliteUserRepository::new(db.pool().clone())),
        state: McpMarketRouterState { market },
    }
}

fn auth(f: &Fx, mode: AuthIdentityMode, verifier: Option<Arc<dyn IRuntimeTokenVerifier>>) -> AuthState {
    AuthState {
        jwt_service: f.jwt.clone(),
        user_repo: f.users.clone(),
        identity_mode: mode,
        runtime_token_verifier: verifier,
        cookie_names: CookieNames::LEGACY,
    }
}

fn origin_guard() -> OriginGuard {
    OriginGuard {
        policy: Arc::new(OriginPolicy::new(Some(ORIGIN), &[])),
        same_origin_via_web_host: true,
    }
}

/// Writes as `aionui-app` mounts them: session-only state, then the strict
/// Origin rule outermost (every identity, C11/C15).
fn write_router(f: &Fx, write_auth: AuthState) -> Router {
    mcp_market_write_routes(f.state.clone())
        .route_layer(from_fn_with_state(write_auth, auth_middleware))
        .route_layer(from_fn_with_state(origin_guard(), require_trusted_origin_middleware))
}

/// The composition used by `aionui-app` in `--local`: reads with the Local
/// state, writes with the session-only state behind the Origin rule.
fn app(f: &Fx) -> Router {
    let read = mcp_market_read_routes(f.state.clone()).route_layer(from_fn_with_state(
        auth(f, AuthIdentityMode::Local, None),
        auth_middleware,
    ));
    let write = write_router(f, auth(f, AuthIdentityMode::UserSession, None));
    Router::new().merge(read).merge(write)
}

fn create_body() -> Value {
    json!({"name": "tb-route", "template_id": "paddle", "environment": "production", "tier": "read",
           "clis": ["claude"], "tools_allow": ["list_products"]})
}

fn post(path: &str, body: &Value) -> axum::http::request::Builder {
    let _ = body;
    Request::post(path).header(header::CONTENT_TYPE, "application/json")
}

/// Sends like a same-origin browser: the trusted `Origin` unless the
/// request already carries one (see [`send_raw`] for other cases).
async fn send(app: &Router, mut req: Request<Body>) -> (StatusCode, Value) {
    if !req.headers().contains_key(header::ORIGIN) {
        req.headers_mut().insert(header::ORIGIN, ORIGIN.parse().unwrap());
    }
    send_raw(app, req).await
}

async fn send_raw(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn reads_use_the_ordinary_auth_state() {
    let f = fx().await;
    let (status, body) = send(
        &app(&f),
        Request::get("/api/mcp/market/instances").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!([]));
    let (status, body) = send(
        &app(&f),
        Request::get("/api/mcp/market/templates").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"][0]["id"], "paddle");
}

#[tokio::test]
async fn writes_without_a_session_are_rejected_even_in_local_mode() {
    let f = fx().await;
    let body = create_body();
    let (status, _) = send(
        &app(&f),
        post("/api/mcp/market/instances", &body)
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "loopback caller without a session");

    // An agent's runtime-token headers are not accepted on the write state.
    let (status, _) = send(
        &app(&f),
        post("/api/mcp/market/instances", &body)
            .header(aionui_auth::RUNTIME_TOKEN_HEADER, "tok")
            .header(aionui_auth::RUNTIME_USER_ID_HEADER, USER)
            .header(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER, "conv-1")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A forged token is rejected.
    let (status, _) = send(
        &app(&f),
        post("/api/mcp/market/instances", &body)
            .header(header::AUTHORIZATION, "Bearer not-a-jwt")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn handlers_fail_closed_when_mounted_under_the_wrong_auth_state() {
    let f = fx().await;
    let body = create_body();
    // Mis-mounted under the Local state: the handler itself refuses.
    let local = mcp_market_write_routes(f.state.clone()).route_layer(from_fn_with_state(
        auth(&f, AuthIdentityMode::Local, None),
        auth_middleware,
    ));
    let (status, resp) = send(
        &local,
        post("/api/mcp/market/instances", &body)
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(resp["code"], "MCP_MARKET_SESSION_REQUIRED");

    // Mis-mounted with a runtime-token verifier: the agent channel is refused.
    let runtime = mcp_market_write_routes(f.state.clone()).route_layer(from_fn_with_state(
        auth(&f, AuthIdentityMode::UserSession, Some(Arc::new(AlwaysValid))),
        auth_middleware,
    ));
    let (status, resp) = send(
        &runtime,
        post("/api/mcp/market/instances", &body)
            .header(aionui_auth::RUNTIME_TOKEN_HEADER, "tok")
            .header(aionui_auth::RUNTIME_USER_ID_HEADER, USER)
            .header(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER, "conv-1")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(resp["code"], "MCP_MARKET_SESSION_REQUIRED");
}

#[tokio::test]
async fn session_writes_succeed_and_never_echo_secrets() {
    let f = fx().await;
    let token = f.jwt.sign(USER, USER).unwrap();
    let app = app(&f);
    let body = create_body();
    let (status, created) = send(
        &app,
        post("/api/mcp/market/instances", &body)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["data"]["id"].as_str().unwrap().to_owned();

    // Client-supplied launch data is rejected (R1 #1).
    let mut with_command = create_body();
    with_command["name"] = json!("tb-route-two");
    with_command["command"] = json!("/bin/sh");
    let (status, _) = send(
        &app,
        post("/api/mcp/market/instances", &with_command)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(with_command.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown field `command` is refused");

    let secret_body = json!({"value": FIXTURE_SECRET});
    let path = format!("/api/mcp/market/instances/{id}/secrets/PADDLE_API_KEY");
    let (status, resp) = send(
        &app,
        Request::put(&path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(secret_body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["data"]["set"], true);
    // Without a session the same secret write is refused.
    let (status, _) = send(
        &app,
        Request::put(&path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(secret_body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A malformed body never echoes its content.
    let (status, resp) = send(
        &app,
        Request::put(&path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(format!(r#"{{"value": 5, "x": "{FIXTURE_SECRET}"}}"#)))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!resp.to_string().contains("ROUTEFIXTURE"));

    // Every read shows refs and `set` only.
    for read in [
        "/api/mcp/market/instances",
        "/api/mcp/market/secrets",
        "/api/mcp/market/status",
    ] {
        let (status, resp) = send(&app, Request::get(read).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!resp.to_string().contains("ROUTEFIXTURE"), "{read}: {resp}");
    }
    let (_, inst) = send(
        &app,
        Request::get(format!("/api/mcp/market/instances/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(inst["data"]["secrets"][0]["set"], true);
    assert_eq!(inst["data"]["secrets"][0]["name"], "TB_ROUTE__PADDLE_API_KEY");

    // Bad path: unknown instance -> 404 with a message, global assignment -> 400.
    let (status, _) = send(
        &app,
        Request::put("/api/mcp/market/instances/mcp_missing/secrets/PADDLE_API_KEY")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(secret_body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let assign = json!({"instance_id": id, "clis": ["claude"]});
    let (status, resp) = send(
        &app,
        post("/api/mcp/market/assignments", &assign)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(assign.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(resp["code"], "MCP_MARKET_INVALID");
    assert!(resp.to_string().contains("R2"), "{resp}");
}

/// C11/C15: market writes read the session cookie under the configured name
/// (`__Host-aionui-session` in webui over HTTPS) and refuse a missing or
/// foreign `Origin` before authentication, in every identity.
#[tokio::test]
async fn writes_need_a_trusted_origin_and_read_the_configured_session_cookie() {
    let f = fx().await;
    let token = f.jwt.sign(USER, USER).unwrap();
    let names = CookieNames::PREFIXED;
    let mut write_auth = auth(&f, AuthIdentityMode::UserSession, None);
    write_auth.cookie_names = names;
    let app = write_router(&f, write_auth);
    let body = create_body();
    let write = |cookie: String, origin: Option<&str>| {
        let mut req = post("/api/mcp/market/instances", &body).header(header::COOKIE, cookie);
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        req.body(Body::from(body.to_string())).unwrap()
    };
    let session = format!("{}={token}", names.session);

    let (status, resp) = send_raw(&app, write(session.clone(), None)).await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("ORIGIN_REQUIRED"))
    );
    let (status, resp) = send_raw(&app, write(session.clone(), Some("https://evil.example"))).await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN"))
    );

    // The legacy session cookie name is not read once prefixed names are set.
    let (status, _) = send_raw(&app, write(format!("aionui-session={token}"), Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, created) = send_raw(&app, write(session, Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
}

/// Kits (C27) ride the market routers: reads with the ordinary state, writes
/// need a verified session + a trusted Origin; agents are refused.
#[tokio::test]
async fn kit_routes_use_the_market_auth_stacks() {
    let f = fx().await;
    let app = app(&f);
    let (status, body) = send(&app, Request::get("/api/kits").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"].as_array().unwrap().len(), 13);

    let kit = json!({"name": "Mine", "skills": ["pdf"]});
    // no session (local loopback caller)
    let (status, _) = send(&app, post("/api/kits", &kit).body(Body::from(kit.to_string())).unwrap()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // agent runtime-token channel, mis-mounted: the handler refuses
    let runtime = mcp_market_write_routes(f.state.clone()).route_layer(from_fn_with_state(
        auth(&f, AuthIdentityMode::UserSession, Some(Arc::new(AlwaysValid))),
        auth_middleware,
    ));
    let (status, resp) = send(
        &runtime,
        post("/api/kits/attachments", &kit)
            .header(aionui_auth::RUNTIME_TOKEN_HEADER, "tok")
            .header(aionui_auth::RUNTIME_USER_ID_HEADER, USER)
            .header(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER, "conv-1")
            .body(Body::from(
                json!({"level": "global", "item_kind": "skill", "item_ref": "pdf", "op": "add"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("MCP_MARKET_SESSION_REQUIRED"))
    );

    let token = f.jwt.sign(USER, USER).unwrap();
    // session but no Origin
    let (status, resp) = send_raw(
        &app,
        post("/api/kits", &kit)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(kit.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("ORIGIN_REQUIRED"))
    );
    // unknown field (wire contract)
    let wrong = json!({"name": "Mine", "skill_names": ["pdf"]});
    let (status, _) = send(
        &app,
        post("/api/kits", &wrong)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(wrong.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // session + same origin
    let (status, created) = send(
        &app,
        post("/api/kits", &kit)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(kit.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    let (status, dup) = send(
        &app,
        post("/api/kits", &kit)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(kit.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, dup["code"].as_str()),
        (StatusCode::CONFLICT, Some("KIT_NAME_TAKEN"))
    );
    let attach = json!({"level": "global", "item_kind": "kit", "item_ref": id, "op": "add"});
    let (status, attached) = send(
        &app,
        post("/api/kits/attachments", &attach)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(attach.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{attached}");
    assert_eq!(attached["data"]["item_name"], "Mine");
    let (status, eff) = send(&app, Request::get("/api/kits/effective").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{eff}");
    // K0 (seeded by the first GET) contributes cs-delegate/cs-verify at global too.
    let pdf = eff["data"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["item_ref"] == "pdf")
        .unwrap();
    assert_eq!(pdf["provenance"]["level"], "global");
}

/// C28: project kit codes over HTTP and the project-kit read route.
#[tokio::test]
async fn project_kit_routes_return_their_codes() {
    let f = fx().await;
    sqlx::query(
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', ?, 'A', 'standard', 1, 1)",
    )
    .bind(USER)
    .execute(f.db.pool())
    .await
    .unwrap();
    let app = app(&f);
    let token = f.jwt.sign(USER, USER).unwrap();
    let create = |body: Value| {
        post("/api/kits", &body)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let (status, resp) = send(&app, create(json!({"name": "x", "project_id": "nope"}))).await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("KIT_UNKNOWN_TARGET"))
    );
    let (status, resp) = send(
        &app,
        Request::get("/api/projects/proj-a/kit").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("KIT_NOT_FOUND"))
    );

    let (status, created) = send(
        &app,
        create(json!({"name": "A kit", "project_id": "proj-a", "skills": ["tb-protocol"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["data"]["project_id"], "proj-a");
    let (status, resp) = send(&app, create(json!({"name": "A kit 2", "project_id": "proj-a"}))).await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::CONFLICT, Some("KIT_PROJECT_HAS_KIT"))
    );

    let (status, got) = send(
        &app,
        Request::get("/api/projects/proj-a/kit").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["data"]["id"], created["data"]["id"]);
    let (status, list) = send(
        &app,
        Request::get("/api/kits?project_id=proj-a").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"].as_array().unwrap().len(), 1);

    // Global attach of the project kit: 400 KIT_INVALID_TARGET.
    let attach = json!({"level": "global", "item_kind": "kit", "item_ref": created["data"]["id"], "op": "add"});
    let (status, resp) = send(
        &app,
        post("/api/kits/attachments", &attach)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(attach.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("KIT_INVALID_TARGET"))
    );

    // The agent runtime-token channel cannot create a project kit.
    let runtime = mcp_market_write_routes(f.state.clone()).route_layer(from_fn_with_state(
        auth(&f, AuthIdentityMode::UserSession, Some(Arc::new(AlwaysValid))),
        auth_middleware,
    ));
    let body = json!({"name": "agent kit", "project_id": "proj-a"});
    let (status, resp) = send(
        &runtime,
        post("/api/kits", &body)
            .header(aionui_auth::RUNTIME_TOKEN_HEADER, "tok")
            .header(aionui_auth::RUNTIME_USER_ID_HEADER, USER)
            .header(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER, "conv-1")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("MCP_MARKET_SESSION_REQUIRED"))
    );

    // C28 §5: the .mcp.json preview/import are session-only too.
    let (status, resp) = send(
        &runtime,
        Request::get("/api/projects/proj-a/kit/import-preview")
            .header(aionui_auth::RUNTIME_TOKEN_HEADER, "tok")
            .header(aionui_auth::RUNTIME_USER_ID_HEADER, USER)
            .header(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER, "conv-1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("MCP_MARKET_SESSION_REQUIRED"))
    );
    let (status, _) = send(
        &app,
        Request::get("/api/projects/proj-a/kit/import-preview")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no session on the app stack");
    let (status, preview) = send(
        &app,
        Request::get("/api/projects/proj-a/kit/import-preview")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["data"]["file_found"], false);
}

/// C36: `/api/servers` rides the market stacks — reads with the ordinary auth
/// state, writes session-only + Origin, agents refused, the key never echoed.
#[tokio::test]
async fn server_routes_use_the_market_auth_stacks_and_never_echo_the_key() {
    let f = fx().await;
    let app = app(&f);
    let (status, body) = send(&app, Request::get("/api/servers").body(Body::empty()).unwrap()).await;
    assert_eq!((status, body["data"].clone()), (StatusCode::OK, json!([])));

    let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nROUTEKEYMATERIAL0123456789\n-----END OPENSSH PRIVATE KEY-----";
    let server = json!({"name": "web-1", "host": "127.0.0.1", "port": 1, "ssh_user": "deploy", "private_key": key,
                        "environment": "production", "tier": "write"});
    // No session (loopback caller).
    let (status, _) = send(
        &app,
        post("/api/servers", &server)
            .body(Body::from(server.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Agent runtime-token channel, mis-mounted: the handler refuses.
    let runtime = mcp_market_write_routes(f.state.clone()).route_layer(from_fn_with_state(
        auth(&f, AuthIdentityMode::UserSession, Some(Arc::new(AlwaysValid))),
        auth_middleware,
    ));
    let (status, resp) = send(
        &runtime,
        post("/api/servers", &server)
            .header(aionui_auth::RUNTIME_TOKEN_HEADER, "tok")
            .header(aionui_auth::RUNTIME_USER_ID_HEADER, USER)
            .header(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER, "conv-1")
            .body(Body::from(server.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("MCP_MARKET_SESSION_REQUIRED"))
    );

    let token = f.jwt.sign(USER, USER).unwrap();
    // Session without Origin.
    let (status, _) = send_raw(
        &app,
        post("/api/servers", &server)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(server.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, created) = send(
        &app,
        post("/api/servers", &server)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(server.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(!created.to_string().contains("ROUTEKEYMATERIAL"));
    assert_eq!(created["data"]["has_key"], true);
    assert_eq!(created["data"]["crucial_commands_ask"], true);
    let id = created["data"]["id"].as_str().unwrap().to_owned();

    // Same name again → 409 with the code; bad host → 400.
    let (status, resp) = send(
        &app,
        post("/api/servers", &server)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(server.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::CONFLICT, Some("SERVER_NAME_TAKEN"))
    );
    let mut bad = server.clone();
    bad["name"] = json!("web-2");
    bad["host"] = json!("-oProxyCommand=sh");
    let (status, resp) = send(
        &app,
        post("/api/servers", &bad)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(bad.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("SERVER_INVALID"))
    );

    // Classify (read stack, runs nothing): the card's reason + environment.
    let classify = json!({"server": "web-1", "command": "systemctl restart nginx"});
    let (status, resp) = send(
        &app,
        post("/api/servers/classify", &classify)
            .body(Body::from(classify.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["data"]["crucial"], true);
    assert_eq!(resp["data"]["reason"], "stops, restarts or disables a service");
    assert_eq!(resp["data"]["environment"], "production");
    assert_eq!(resp["data"]["server_id"], id.as_str());

    // Test connection: session only; without a stored host key it reports host_key_unknown.
    let (status, _) = send(
        &app,
        Request::post(format!("/api/servers/{id}/test"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, resp) = send(
        &app,
        Request::post(format!("/api/servers/{id}/test"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(
        (resp["data"]["ok"].as_bool(), resp["data"]["error_code"].as_str()),
        (Some(false), Some("host_key_unknown"))
    );
    let scan = json!({"host": "-oProxyCommand=sh"});
    let (status, resp) = send(
        &app,
        post("/api/servers/scan-host-key", &scan)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(scan.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("SERVER_INVALID"))
    );

    let (status, listed) = send(&app, Request::get("/api/servers").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!listed.to_string().contains("ROUTEKEYMATERIAL"));

    let (status, resp) = send(
        &app,
        Request::delete(format!("/api/servers/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    let (status, resp) = send(
        &app,
        Request::get(format!("/api/servers/{id}")).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("SERVER_NOT_FOUND"))
    );
}
