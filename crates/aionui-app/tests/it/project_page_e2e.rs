//! Project page routes (C42, TASK-169) through the REAL app router in local
//! identity (production): the reads answer the owner's browser session; the
//! storage rescan is a write — session AND a trusted Origin, the agent
//! runtime-token channel refused.

use crate::common;

use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::body_json;
use serde_json::Value;
use tower::ServiceExt;

const ORIGIN: &str = "https://y.talkseeds.com";
const PASSWORD: &str = "StrongP@ss1";
const USER: &str = "system_default_user";

struct App {
    router: axum::Router,
    services: aionui_app::AppServices,
    token: String,
    project: String,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn app() -> App {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let mut services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    services.origin_policy = Arc::new(aionui_auth::OriginPolicy::new(Some(ORIGIN), &[]));
    services.cookie_config = Arc::new(aionui_auth::CookieConfig::for_identity(false, false));
    let memory_root = tempfile::tempdir().unwrap();
    services.memory_service = Arc::new(aionui_project::MemoryService::new(memory_root.path()));

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("file.bin"), vec![b'x'; 2048]).unwrap();
    let uri = aionui_project::canonical::to_file_uri(workspace.path()).unwrap();
    let created = services
        .project_service
        .create_standard(USER, uri)
        .await
        .expect("project");
    let project = created.project.project_id.clone();

    let router = aionui_app::create_router(&services).await.expect("build router");
    let hash = aionui_auth::hash_password(PASSWORD).unwrap();
    services
        .user_repo
        .set_system_user_credentials("admin", &hash)
        .await
        .unwrap();
    let req = Request::post("/login")
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"username":"admin","password":"{PASSWORD}"}}"#)))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login");
    let token = body_json(resp).await["token"].as_str().unwrap().to_owned();
    App {
        router,
        services,
        token,
        project,
        _dirs: (memory_root, workspace),
    }
}

impl App {
    async fn send(&self, method: &str, path: &str, session: bool, origin: bool, runtime: bool) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if session {
            builder = builder.header("cookie", format!("aionui-session={}", self.token));
        }
        if origin {
            builder = builder.header("origin", ORIGIN);
        }
        if runtime {
            let token = self
                .services
                .runtime_token_service
                .issue(
                    USER,
                    "conv-page",
                    TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
                    [RuntimeTokenScope::ConversationHelper],
                )
                .token;
            builder = builder
                .header("x-aionui-user-id", USER)
                .header("x-aionui-conversation-id", "conv-page")
                .header("x-aionui-runtime-token", token);
        }
        let resp = self
            .router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    fn path(&self, rest: &str) -> String {
        format!("/api/projects/{}{rest}", self.project)
    }
}

#[tokio::test]
async fn the_reads_answer_the_owners_session() {
    let app = app().await;
    let (status, overview) = app.send("GET", &app.path("/overview"), true, false, false).await;
    assert_eq!(status, StatusCode::OK, "{overview}");
    assert_eq!(overview["data"]["project_id"], app.project.as_str());
    assert_eq!(overview["data"]["conversations"]["total"], 0);
    assert!(overview["data"].get("storage").is_none(), "the overview never scans");

    let (status, usage) = app.send("GET", &app.path("/usage?days=30"), true, false, false).await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["data"]["days"].as_array().unwrap().len(), 30);

    let (status, summary) = app.send("GET", "/api/usage/summary?days=7", true, false, false).await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    assert_eq!(summary["data"]["range_days"], 7);

    let (status, storage) = app.send("GET", &app.path("/storage"), true, false, false).await;
    assert_eq!(status, StatusCode::OK, "{storage}");
    assert_eq!(storage["data"]["roots"][0]["label"], "workspace");
    assert_eq!(storage["data"]["roots"][0]["bytes"], 2048);

    let (status, body) = app.send("GET", "/api/projects/nope/overview", true, false, false).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );
    let (status, body) = app.send("GET", &app.path("/usage?days=5"), true, false, false).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("usage_invalid_days"))
    );
}

#[tokio::test]
async fn the_rescan_is_a_write_session_and_trusted_origin_only() {
    let app = app().await;
    let rescan = app.path("/storage/rescan");
    // Session without Origin: refused by the Origin guard.
    let (status, _) = app.send("POST", &rescan, true, false, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Origin without a session: no local default user on a write route.
    let (status, _) = app.send("POST", &rescan, false, true, false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // An agent (runtime token) cannot trigger a scan.
    let (status, _) = app.send("POST", &rescan, false, true, true).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The owner's browser: scans now.
    let (status, body) = app.send("POST", &rescan, true, true, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["roots"][0]["bytes"], 2048);
    // The cached scan now feeds the overview.
    let (_, overview) = app.send("GET", &app.path("/overview"), true, false, false).await;
    assert_eq!(overview["data"]["storage"]["total_bytes"], body["data"]["total_bytes"]);
}
