//! Card C13: the agent runtime token (`AIONUI_RUNTIME_TOKEN`) is limited to
//! the routes the helper CLIs call; any other route behind `auth_middleware`
//! answers 403 `RUNTIME_SCOPE`. Local identity is unchanged.
//!
//! The real router is used throughout; the CLI tests run the real `aioncore`
//! binary against it over TCP.

use std::process::Stdio;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;
use tower::ServiceExt;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};

const USER_ID: &str = "system_default_user";
const CONVERSATION_ID: &str = "conv-c13";

async fn app(config: aionui_app::AppConfig) -> (axum::Router, String) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");
    let token = services
        .runtime_token_service
        .issue(
            USER_ID,
            CONVERSATION_ID,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token;
    (router, token)
}

/// Webui identity (`AppConfig::default()`), the mode the C11 switch enables.
async fn webui_app() -> (axum::Router, String) {
    app(aionui_app::AppConfig::default()).await
}

fn helper_request(method: &str, path: &str, token: &str) -> Request<Body> {
    let body = if method == "GET" || method == "DELETE" {
        Body::empty()
    } else {
        Body::from("{}")
    };
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-aionui-user-id", USER_ID)
        .header("x-aionui-conversation-id", CONVERSATION_ID)
        .header("x-aionui-runtime-token", token)
        .body(body)
        .unwrap()
}

async fn status_and_json(router: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Owner routes no helper CLI calls; a runtime token must not reach them.
const FORBIDDEN: &[(&str, &str)] = &[
    ("POST", "/api/conversations"),
    ("DELETE", "/api/conversations/conv-other"),
    ("POST", "/api/conversations/conv-other/messages"),
    ("GET", "/api/conversations/conv-other/workspace"),
    ("GET", "/api/sidebar"),
    ("GET", "/api/projects/p-1"),
];

#[tokio::test]
async fn runtime_token_on_owner_routes_is_refused_with_runtime_scope() {
    let (router, token) = webui_app().await;
    for (method, path) in FORBIDDEN {
        let (status, json) = status_and_json(&router, helper_request(method, path, &token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {json}");
        assert_eq!(json["code"], "RUNTIME_SCOPE", "{method} {path}: {json}");
    }
}

#[tokio::test]
async fn runtime_token_still_reaches_the_helper_cli_routes() {
    let (router, token) = webui_app().await;
    for path in ["/api/settings", "/api/providers", "/api/cron/jobs", "/api/mcp/servers"] {
        let (status, json) = status_and_json(&router, helper_request("GET", path, &token)).await;
        assert_eq!(status, StatusCode::OK, "GET {path}: {json}");
    }
}

#[tokio::test]
async fn local_identity_is_unchanged_for_runtime_token_requests() {
    let (router, token) = app(aionui_app::AppConfig {
        local: true,
        ..Default::default()
    })
    .await;
    let (status, json) = status_and_json(&router, helper_request("GET", "/api/sidebar", &token)).await;
    assert_eq!(status, StatusCode::OK, "local GET /api/sidebar: {json}");
}

// ---------------------------------------------------------------------------
// The real helper CLIs against the real router (webui identity).
// ---------------------------------------------------------------------------

async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

async fn run_cli(base_url: &str, token: &str, args: &[&str], stdin: Option<&str>) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aioncore"))
        .args(args)
        .env("AIONUI_BASE_URL", base_url)
        .env("AIONUI_CONVERSATION_ID", CONVERSATION_ID)
        .env("AIONUI_USER_ID", USER_ID)
        .env("AIONUI_RUNTIME_TOKEN", token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = stdin {
        child.stdin.as_mut().unwrap().write_all(input.as_bytes()).await.unwrap();
    }
    drop(child.stdin.take());
    child.wait_with_output().await.unwrap()
}

fn describe(output: &std::process::Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test]
async fn helper_cli_commands_work_with_a_scoped_runtime_token() {
    let (router, token) = webui_app().await;
    let (base_url, handle) = serve(router).await;

    let commands: &[&[&str]] = &[
        &["config", "settings", "get"],
        &["config", "providers", "list"],
        &["config", "mcp", "servers", "list"],
        &["config", "cron", "jobs", "list"],
        // /health + providers + mcp servers + cron jobs + conversations
        &["diagnose", "overview"],
    ];
    for args in commands {
        let output = run_cli(&base_url, &token, args, None).await;
        assert!(output.status.success(), "{args:?} failed\n{}", describe(&output));
    }

    handle.abort();
}

#[tokio::test]
async fn diagnose_http_escape_hatch_cannot_leave_the_runtime_scope() {
    let (router, token) = webui_app().await;
    let (base_url, handle) = serve(router).await;

    let allowed = run_cli(
        &base_url,
        &token,
        &["diagnose", "http", "get"],
        Some(r#"{ "path": "/api/providers" }"#),
    )
    .await;
    assert!(allowed.status.success(), "{}", describe(&allowed));

    let refused = run_cli(
        &base_url,
        &token,
        &["diagnose", "http", "get"],
        Some(r#"{ "path": "/api/sidebar" }"#),
    )
    .await;
    handle.abort();
    assert_eq!(refused.status.code(), Some(3), "{}", describe(&refused));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("DIAGNOSE_HTTP_STATUS_ERROR"), "{stderr}");
    assert!(stderr.contains("403"), "{stderr}");
}
