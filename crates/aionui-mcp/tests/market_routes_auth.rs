//! Auth matrix of `/api/mcp/market/*` (R1 #1): reads behind the ordinary auth
//! state, writes behind a session-only auth state (as `aionui-app` mounts
//! them, also in `--local`), plus the handlers' own channel check.

use std::sync::Arc;

use aionui_auth::{AuthIdentityMode, AuthState, CookieNames, IRuntimeTokenVerifier, JwtService, auth_middleware};
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

struct AlwaysValid;
impl IRuntimeTokenVerifier for AlwaysValid {
    fn verify_conversation_helper(&self, _: &str, _: &str, _: &str) -> bool {
        true
    }
}

struct Fx {
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
    let market = McpMarketService::new(repo, servers, Arc::new(catalog), [9u8; 32]);
    Fx {
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

/// The composition used by `aionui-app` in `--local`: reads with the Local
/// state, writes with the session-only state.
fn app(f: &Fx) -> Router {
    let read = mcp_market_read_routes(f.state.clone()).route_layer(from_fn_with_state(
        auth(f, AuthIdentityMode::Local, None),
        auth_middleware,
    ));
    let write = mcp_market_write_routes(f.state.clone()).route_layer(from_fn_with_state(
        auth(f, AuthIdentityMode::UserSession, None),
        auth_middleware,
    ));
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

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
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

/// C11: market writes read the session cookie under the configured name and,
/// in local mode, sit behind the shared `CsrfState` double submit (the
/// composition in `aionui-app`), so `__Host-` names in webui work and a
/// write without a matching CSRF header is refused.
#[tokio::test]
async fn writes_follow_the_c11_cookie_names_and_csrf_state() {
    use aionui_auth::{CookieConfig, CsrfState, csrf_middleware};
    let f = fx().await;
    let token = f.jwt.sign(USER, USER).unwrap();
    let names = CookieNames::PREFIXED;
    let mut write_auth = auth(&f, AuthIdentityMode::UserSession, None);
    write_auth.cookie_names = names;
    let csrf = CsrfState {
        cookie_config: Arc::new(CookieConfig::for_identity(true, true)),
        origin_policy: None,
    };
    let app = mcp_market_write_routes(f.state.clone())
        .route_layer(from_fn_with_state(write_auth, auth_middleware))
        .route_layer(from_fn_with_state(csrf, csrf_middleware));
    let body = create_body();
    let write = |cookie: String, csrf_header: Option<&str>| {
        let mut req = post("/api/mcp/market/instances", &body).header(header::COOKIE, cookie);
        if let Some(h) = csrf_header {
            req = req.header("x-csrf-token", h);
        }
        req.body(Body::from(body.to_string())).unwrap()
    };

    // Missing CSRF header → refused before the handler.
    let cookie = format!("{}={token}; {}=c1", names.session, names.csrf);
    let (status, resp) = send(&app, write(cookie.clone(), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");
    assert_eq!(resp["code"], "CSRF_INVALID");

    // The legacy session cookie name is not read once prefixed names are set.
    let legacy = format!("aionui-session={token}; {}=c1", names.csrf);
    let (status, _) = send(&app, write(legacy, Some("c1"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Prefixed session cookie + matching double submit → created.
    let (status, created) = send(&app, write(cookie, Some("c1"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
}
