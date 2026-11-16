//! Project memory review routes (C29, TASK-109) through the REAL app router in
//! local identity (production): reads with the browser session, writes with
//! the session AND a trusted Origin, the agent runtime-token channel refused.
//! The memory root is a temp dir (the real `~/.claude` is never touched).

mod common;

use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::body_json;
use serde_json::{Value, json};
use tower::ServiceExt;

const ORIGIN: &str = "https://y.talkseeds.com";
const PASSWORD: &str = "StrongP@ss1";
const USER: &str = "system_default_user";

struct App {
    router: axum::Router,
    services: aionui_app::AppServices,
    token: String,
    project: String,
    memory_dir: std::path::PathBuf,
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
    let root = tempfile::tempdir().unwrap();
    let memory = aionui_project::MemoryService::new(root.path());
    services.memory_service = Arc::new(memory.clone());

    let workspace = tempfile::tempdir().unwrap();
    let uri = aionui_project::canonical::to_file_uri(workspace.path()).unwrap();
    let created = services
        .project_service
        .create_standard(USER, uri)
        .await
        .expect("project");
    let project = created.project.project_id.clone();
    let ws = services
        .project_service
        .workspace_path(USER, &project)
        .await
        .unwrap()
        .expect("workspace path");
    let memory_dir = memory.dir_for(&ws);
    tokio::fs::create_dir_all(&memory_dir).await.unwrap();
    tokio::fs::write(
        memory_dir.join("MEMORY.md"),
        "# Memory Index\n- [Rule](rule.md) — tests first\n- [Gone](gone.md) — missing\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        memory_dir.join("rule.md"),
        "---\nname: rule\ndescription: Tests first\nmetadata:\n  type: feedback\n---\n\nKeep tests green. See [[nowhere]].\n",
    )
    .await
    .unwrap();
    tokio::fs::write(root.path().join("secret.md"), "outside")
        .await
        .unwrap();

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
        memory_dir,
        _dirs: (root, workspace),
    }
}

#[derive(Default)]
struct Call<'a> {
    session: bool,
    origin: bool,
    runtime: Option<&'a str>,
    body: Option<Value>,
}

impl App {
    async fn send(&self, method: &str, path: &str, call: Call<'_>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if call.session {
            builder = builder.header("cookie", format!("aionui-session={}", self.token));
        }
        if call.origin {
            builder = builder.header("origin", ORIGIN);
        }
        if let Some(token) = call.runtime {
            builder = builder
                .header("x-aionui-user-id", USER)
                .header("x-aionui-conversation-id", "conv-mem")
                .header("x-aionui-runtime-token", token);
        }
        let body = match call.body {
            Some(body) => {
                builder = builder.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let resp = self.router.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    fn path(&self, rest: &str) -> String {
        format!("/api/projects/{}/memory{rest}", self.project)
    }

    fn browser(body: Option<Value>) -> Call<'static> {
        Call {
            session: true,
            origin: true,
            runtime: None,
            body,
        }
    }

    fn runtime_token(&self) -> String {
        self.services
            .runtime_token_service
            .issue(
                USER,
                "conv-mem",
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
                [RuntimeTokenScope::ConversationHelper],
            )
            .token
    }
}

#[tokio::test]
async fn the_list_and_detail_read_the_workspaces_memory_with_lint() {
    let app = app().await;
    let (status, body) = app.send("GET", &app.path(""), App::browser(None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data = &body["data"];
    assert_eq!(data["dir_exists"], true);
    assert_eq!(data["index_lines"], 3);
    let item = &data["items"][0];
    assert_eq!(item["file"], "rule.md");
    assert_eq!(item["type"], "feedback");
    assert_eq!(item["in_index"], true);
    assert_eq!(item["broken_links"], json!(["nowhere"]));
    assert!(item.get("body").is_none(), "no bodies on the list");
    let kinds: Vec<&str> = data["lint"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"missing_file") && kinds.contains(&"broken_link"),
        "{kinds:?}"
    );

    let (status, body) = app.send("GET", &app.path("/rule.md"), App::browser(None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]["frontmatter_raw"]
            .as_str()
            .unwrap()
            .starts_with("name: rule")
    );
    assert!(body["data"]["body"].as_str().unwrap().starts_with("Keep tests green."));
    assert!(body["data"]["modified"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn unknown_projects_traversal_and_missing_files_are_refused_with_codes() {
    let app = app().await;
    let (status, body) = app.send("GET", "/api/projects/nope/memory", App::browser(None)).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );
    for bad in ["..%2Fsecret.md", "..%2F..%2Fsecret.md", "rule.txt", ".trash"] {
        let (status, body) = app.send("GET", &app.path(&format!("/{bad}")), App::browser(None)).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("MEMORY_INVALID_NAME")),
            "{bad}: {body}"
        );
    }
    let (status, body) = app.send("GET", &app.path("/absent.md"), App::browser(None)).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("MEMORY_FILE_NOT_FOUND"))
    );
}

#[tokio::test]
async fn a_stale_put_is_a_409_and_a_fresh_one_saves() {
    let app = app().await;
    let (_, read) = app.send("GET", &app.path("/rule.md"), App::browser(None)).await;
    let modified = read["data"]["modified"].as_i64().unwrap();
    let frontmatter = read["data"]["frontmatter_raw"].clone();
    let (status, body) = app
        .send(
            "PUT",
            &app.path("/rule.md"),
            App::browser(Some(json!({"body": "Changed.", "frontmatter_raw": frontmatter,
                "expected_modified": modified - 1}))),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("MEMORY_CHANGED")),
        "{body}"
    );
    assert_eq!(body["details"]["current_modified"], modified, "{body}");

    let (status, body) = app
        .send(
            "PUT",
            &app.path("/rule.md"),
            App::browser(Some(json!({"body": "Changed.", "frontmatter_raw": frontmatter,
                "expected_modified": modified}))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["body"], "Changed.\n");
    let on_disk = tokio::fs::read_to_string(app.memory_dir.join("rule.md")).await.unwrap();
    assert!(
        on_disk.starts_with("---\nname: rule\n") && on_disk.ends_with("Changed.\n"),
        "{on_disk}"
    );

    // Unknown fields are refused (deny_unknown_fields → the router's JSON rejection, 400).
    let (status, _) = app
        .send(
            "PUT",
            &app.path("/rule.md"),
            App::browser(Some(
                json!({"body": "x", "frontmatter_raw": "", "expected_modified": 0, "extra": 1}),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_drops_the_index_line_into_the_trash_and_restore_undoes_it() {
    let app = app().await;
    let (status, body) = app.send("DELETE", &app.path("/rule.md"), App::browser(None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["index_lines_removed"], 1);
    let trash = body["data"]["trash_file"].as_str().unwrap();
    assert!(app.memory_dir.join(trash).exists());
    let index = tokio::fs::read_to_string(app.memory_dir.join("MEMORY.md"))
        .await
        .unwrap();
    assert!(!index.contains("rule.md"), "{index}");

    let (status, body) = app
        .send("POST", &app.path("/rule.md/restore"), App::browser(None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["body"].as_str().unwrap().contains("Keep tests green."));
    let index = tokio::fs::read_to_string(app.memory_dir.join("MEMORY.md"))
        .await
        .unwrap();
    assert!(index.contains("- [Rule](rule.md) — tests first"), "{index}");
}

#[tokio::test]
async fn writes_need_a_session_and_a_trusted_origin_and_refuse_the_runtime_token() {
    let app = app().await;
    let token = app.runtime_token();
    let writes = [
        (
            "PUT",
            app.path("/rule.md"),
            Some(json!({"body": "x", "frontmatter_raw": "", "expected_modified": 0})),
        ),
        ("DELETE", app.path("/rule.md"), None),
        ("POST", app.path("/rule.md/restore"), None),
    ];
    for (method, path, body) in &writes {
        // No session at all.
        let (status, _) = app
            .send(
                method,
                path,
                Call {
                    origin: true,
                    body: body.clone(),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path} unauthenticated");
        // Session without an Origin: refused by the strict Origin rule.
        let (status, _) = app
            .send(
                method,
                path,
                Call {
                    session: true,
                    body: body.clone(),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} without Origin");
        // The agent runtime-token channel, even with an Origin.
        let (status, _) = app
            .send(
                method,
                path,
                Call {
                    origin: true,
                    runtime: Some(&token),
                    body: body.clone(),
                    ..Default::default()
                },
            )
            .await;
        // The session-only auth state has no runtime-token verifier: no session → 401.
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path} runtime token");
    }
    // Nothing changed.
    let on_disk = tokio::fs::read_to_string(app.memory_dir.join("rule.md")).await.unwrap();
    assert!(on_disk.contains("Keep tests green."));

    // Reads ride the ordinary auth layer: in local identity it serves the local
    // owner to every caller of the loopback port (aionui-auth middleware.rs,
    // Local branch), exactly like the project detail route next to them.
    let project = format!("/api/projects/{}", app.project);
    for runtime in [None, Some(token.as_str())] {
        let read = || Call {
            runtime,
            ..Default::default()
        };
        let (memory, _) = app.send("GET", &app.path(""), read()).await;
        let (detail, _) = app.send("GET", &project, read()).await;
        assert_eq!(
            memory,
            detail,
            "memory reads are guarded like the project detail (runtime={})",
            runtime.is_some()
        );
    }
}
