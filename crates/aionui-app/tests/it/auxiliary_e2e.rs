//! E2E integration tests for auxiliary conversation routes.
//!
//! Tests cover: workspace browse, side-question,
//! slash-commands, and openclaw-runtime endpoints.

use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, get_with_token, json_with_token, setup_and_login};

// ── Helpers ─────────────────────────────────────────────────────

fn create_conv_body(name: &str, agent_type: &str) -> serde_json::Value {
    json!({
        "type": agent_type,
        "name": name,
        "extra": {}
    })
}

fn create_conv_body_with_workspace(name: &str, agent_type: &str, workspace: &str) -> serde_json::Value {
    json!({
        "type": agent_type,
        "name": name,
        "extra": { "workspace": workspace }
    })
}

async fn create_conversation_with_workspace(
    app: &mut axum::Router,
    token: &str,
    csrf: &str,
    name: &str,
    agent_type: &str,
    workspace: &str,
) -> String {
    let req = common::json_with_token(
        "POST",
        "/api/conversations",
        create_conv_body_with_workspace(name, agent_type, workspace),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = common::body_json(resp).await;
    json["data"]["id"].as_str().unwrap().to_owned()
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str, name: &str, agent_type: &str) -> String {
    let req = common::json_with_token(
        "POST",
        "/api/conversations",
        create_conv_body(name, agent_type),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = common::body_json(resp).await;
    json["data"]["id"].as_str().unwrap().to_owned()
}

async fn build_app() -> (axum::Router, aionui_app::AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = crate::common::test_services(db, &aionui_app::AppConfig::default()).await;
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

// ── 9.1 Workspace browse ────────────────────────────────────────

#[tokio::test]
async fn workspace_browse_requires_auth() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/conversations/test-conv/workspace?path=/src")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn workspace_browse_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    // Seed a real workspace on disk so the handler can canonicalize it.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), b"// hi").unwrap();

    let ws = tmp.path().to_string_lossy().into_owned();
    let conv_id = create_conversation_with_workspace(&mut app, &token, &csrf, "Test Conv", "acp", &ws).await;

    let req = get_with_token(&format!("/api/conversations/{conv_id}/workspace?path=/src"), &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    // Workspace comes from DB; no active agent required.
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let entries = json["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "lib.rs");
    assert_eq!(entries[0]["type"], "file");
}

#[tokio::test]
async fn workspace_browse_conversation_not_found() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token("/api/conversations/does-not-exist/workspace?path=/src", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workspace_browse_empty_path() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token("/api/conversations/some-conv/workspace?path=", &token);
    let resp = app.oneshot(req).await.unwrap();
    // Empty path should return 400 (validated before agent lookup)
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_browse_treats_symlinked_skill_dir_as_directory() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let builtin = tmp.path().join("builtin-skills/auto-inject/aionui-skills");
    std::fs::create_dir_all(workspace.join(".claude/skills")).unwrap();
    std::fs::create_dir_all(&builtin).unwrap();
    std::fs::write(builtin.join("SKILL.md"), b"---\ndescription: test\n---\nbody").unwrap();
    std::os::unix::fs::symlink(&builtin, workspace.join(".claude/skills/aionui-skills")).unwrap();

    let ws = workspace.to_string_lossy().into_owned();
    let conv_id = create_conversation_with_workspace(&mut app, &token, &csrf, "Test Conv", "acp", &ws).await;

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/workspace?path=/.claude/skills"),
        &token,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries
            .iter()
            .any(|entry| entry["name"] == "aionui-skills" && entry["type"] == "directory"),
        "symlinked skill dir should stay visible as directory: {entries:?}"
    );

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/workspace?path=/.claude/skills/aionui-skills"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["data"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry["name"] == "SKILL.md" && entry["type"] == "file"),
        "symlinked skill dir should remain browsable: {entries:?}"
    );
}

// ── 9.2 Side question ───────────────────────────────────────────

#[tokio::test]
async fn side_question_requires_auth() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations/test-conv/side-question")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"question":"test?"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn side_question_empty_question() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    // side-question is now dispatched to AgentInstance after the
    // conversation lookup, so a missing conversation surfaces as 404
    // before the empty-question check gets a chance to fire.
    let req = json_with_token(
        "POST",
        "/api/conversations/some-conv/side-question",
        json!({ "question": "" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn side_question_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Side Q Test", "acp").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/side-question"),
        json!({ "question": "What is this?" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // No active agent → 404
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 9.4 Slash commands ──────────────────────────────────────────

#[tokio::test]
async fn slash_commands_requires_auth() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/conversations/test-conv/slash-commands")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn slash_commands_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &_csrf, "Slash Test", "acp").await;

    let req = get_with_token(&format!("/api/conversations/{conv_id}/slash-commands"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Confirmation routes (no active task → graceful defaults) ─────

#[tokio::test]
async fn list_confirmations_no_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Confirm Test", "acp").await;

    let req = get_with_token(&format!("/api/conversations/{conv_id}/confirmations"), &token);
    let resp = app.oneshot(req).await.unwrap();
    // No active agent → returns empty list gracefully
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn confirm_call_no_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Confirm Test", "acp").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/confirmations/call-1/confirm"),
        json!({ "msg_id": "msg-1", "data": { "value": "allow" }, "always_allow": false }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── "Continue in background" on a running tool call (TASK-8) ─────

#[tokio::test]
async fn background_tool_call_is_a_csrf_protected_write() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations/test-conv/tool-calls/toolu_1/background")
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn background_tool_call_validates_the_call_and_the_owner() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Bg Test", "acp").await;
    let post = |path: String, token: &str, csrf: &str| json_with_token("POST", &path, json!({}), token, csrf);

    // A call id that could not be a tool_use_id (it rides the control request id).
    let resp = app
        .clone()
        .oneshot(post(
            format!("/api/conversations/{conv_id}/tool-calls/bad%20id/background"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Another user's conversation does not exist for them.
    let (token2, csrf2) = setup_and_login(&mut app, &services, "user2", "pass123").await;
    let resp = app
        .clone()
        .oneshot(post(
            format!("/api/conversations/{conv_id}/tool-calls/toolu_1/background"),
            &token2,
            &csrf2,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // No running agent for the conversation: nothing to background.
    let resp = app
        .oneshot(post(
            format!("/api/conversations/{conv_id}/tool-calls/toolu_1/background"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Dedicated ask-answer route (2026-08-05: question answers do not ride
// the permission confirm endpoint) ─────

#[tokio::test]
async fn answer_ask_no_task_is_404() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Ask Test", "acp").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/asks/req-1/answer"),
        json!({ "answers": [{ "question": "Which?", "labels": ["A"] }] }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // No active agent behind the conversation → the ask cannot be answered.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn answer_ask_rejects_empty_answers() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Ask Test", "acp").await;

    // Neither answers nor decline: an "empty allow" would be silent data loss
    // (claude drops unanswered questions), so the route must reject it before
    // any dispatch.
    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/asks/req-1/answer"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn answer_ask_rejects_decline_with_answers() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Ask Test", "acp").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/asks/req-1/answer"),
        json!({ "decline": true, "answers": [{ "question": "Which?", "labels": ["A"] }] }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn check_approval_no_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Approval Test", "acp").await;

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/approvals/check?action=edit_file"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    // No active agent → returns approved=false gracefully
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["approved"], false);
}

// ── Stop + Warmup (no active task → idempotent success) ───────

#[tokio::test]
async fn stop_stream_no_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Stop Test", "acp").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/cancel"),
        json!({ "turn_id": "turn_no_active_task" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // Stop with no active agent is idempotent.
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Conversation notices (TASK-137: the Token Pool says it switched accounts) ─────

#[tokio::test]
async fn a_notice_is_stored_and_pushed_live_to_its_owner() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Notice Test", "acp").await;
    let mut bus = services.event_bus.subscribe();

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/notices"),
        json!({
            "content": "Switched Claude account: Work → Personal (weekly limit, resets Fri 09:00).",
            "tip_type": "warning",
            "code": "TOKEN_POOL_SWITCHED",
            "params": {"from": "Work", "to": "Personal"}
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let msg_id = body_json(resp).await["data"]["msg_id"].as_str().unwrap().to_owned();

    // Live: one message.stream tips frame for this user and conversation.
    let frame = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), bus.recv())
            .await
            .expect("a live frame")
            .unwrap();
        if event.name == "message.stream" && event.data["type"] == "tips" {
            break event;
        }
    };
    assert_eq!(frame.data["conversation_id"], conv_id.as_str());
    assert_eq!(frame.data["msg_id"], msg_id.as_str());
    assert!(frame.data["user_id"].is_string(), "routed to its user only");
    assert_eq!(frame.data["data"]["code"], "TOKEN_POOL_SWITCHED");
    assert_eq!(frame.data["data"]["type"], "warning");

    // Stored: it is part of the conversation's history.
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await.to_string();
    assert!(
        body.contains("TOKEN_POOL_SWITCHED") && body.contains("weekly limit"),
        "{body}"
    );
}

#[tokio::test]
async fn a_notice_is_validated_and_owner_only() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Notice Test", "acp").await;
    let post = |body: serde_json::Value, token: &str, csrf: &str| {
        json_with_token(
            "POST",
            &format!("/api/conversations/{conv_id}/notices"),
            body,
            token,
            csrf,
        )
    };
    for bad in [
        json!({"content": "", "tip_type": "info"}),
        json!({"content": "x", "tip_type": "error"}),
        json!({"content": "x", "tip_type": "info", "code": "lower-case"}),
        json!({"content": "x", "tip_type": "info", "params": [1, 2]}),
        json!({"content": "x".repeat(501), "tip_type": "info"}),
    ] {
        let resp = app.clone().oneshot(post(bad.clone(), &token, &csrf)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    // Unknown fields are refused, not ignored.
    let resp = app
        .clone()
        .oneshot(post(
            json!({"content": "x", "tip_type": "info", "turn": true}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());

    let (token2, csrf2) = setup_and_login(&mut app, &services, "user2", "pass123").await;
    let resp = app
        .clone()
        .oneshot(post(json!({"content": "x", "tip_type": "info"}), &token2, &csrf2))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "another user's conversation does not exist for them"
    );

    let no_csrf = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/conversations/{conv_id}/notices"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"content":"x","tip_type":"info"}"#))
        .unwrap();
    let resp = app.oneshot(no_csrf).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}

// ── TASK-158: Context panel ─────────────────────────────────────

/// Put the conversation on `backend` (and optionally mark a turn running),
/// straight in the DB — the routes under test only read these.
async fn set_backend(services: &aionui_app::AppServices, conv_id: &str, backend: &str, status: &str) {
    sqlx::query("UPDATE conversations SET extra = json_set(extra, '$.backend', ?), status = ? WHERE id = ?")
        .bind(backend)
        .bind(status)
        .bind(conv_id)
        .execute(services.database.pool())
        .await
        .unwrap();
}

/// Store a usage snapshot (and optionally a claude context report) the way the
/// agent pump does.
async fn seed_runtime(
    services: &aionui_app::AppServices,
    conv_id: &str,
    usage: &serde_json::Value,
    report: Option<&serde_json::Value>,
) {
    use aionui_db::{CreateAcpSessionParams, IAcpSessionRepository, SaveRuntimeStateParams};
    let (user_id,): (String,) = sqlx::query_as("SELECT user_id FROM conversations WHERE id = ?")
        .bind(conv_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let repo = aionui_db::SqliteAcpSessionRepository::new(services.database.pool().clone());
    if repo.get_for_user(&user_id, conv_id).await.unwrap().is_none() {
        repo.create(&CreateAcpSessionParams {
            user_id: &user_id,
            conversation_id: conv_id,
            agent_source: "builtin",
            agent_id: "claude",
        })
        .await
        .unwrap();
    }
    let usage = usage.to_string();
    let report = report.map(|r| json!({ "report": r, "updated_at": 1_795_000_000_000_i64 }).to_string());
    let params = SaveRuntimeStateParams {
        context_usage_json: Some(Some(&usage)),
        context_report_json: report.as_deref().map(Some),
        ..Default::default()
    };
    assert!(
        repo.save_runtime_state_for_user(&user_id, conv_id, &params)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn the_context_panel_serves_null_then_estimated_then_the_cli_report() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Context", "acp").await;
    let uri = format!("/api/conversations/{conv_id}/context");

    // Before the first usage: 200 with null, like /usage.
    let resp = app.clone().oneshot(get_with_token(&uri, &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["data"].is_null());

    // A backend that reports totals only: an estimated split, no select.
    set_backend(&services, &conv_id, "codex", "finished").await;
    seed_runtime(&services, &conv_id, &json!({"used": 50_000, "size": 258_400}), None).await;
    let resp = app.clone().oneshot(get_with_token(&uri, &token)).await.unwrap();
    let data = body_json(resp).await["data"].clone();
    assert_eq!(data["source"], "estimated");
    assert_eq!(data["used"], 50_000);
    assert_eq!(data["size"], 258_400);
    assert_eq!(data["percent"], 19.3);
    assert_eq!(data["auto_compact"]["supported"], false);
    assert_eq!(data["categories"][0]["id"], "messages");
    assert_eq!(data["categories"][0]["estimated"], true);
    assert_eq!(data["items"]["mcp_tools"], json!([]));

    // Claude with its own report: passed through, with the auto-compact numbers.
    set_backend(&services, &conv_id, "claude", "finished").await;
    let report = json!({
        "totalTokens": 600_000, "maxTokens": 1_000_000, "model": "claude-opus-5-5",
        "isAutoCompactEnabled": true, "autoCompactThreshold": 960_000,
        "categories": [
            {"id": "messages", "name": "Messages", "tokens": 600_000, "color": "x"},
            {"id": "mcp_tools_deferred", "name": "MCP tools (deferred)", "tokens": 42_000, "color": "inactive", "isDeferred": true}
        ],
        "mcpTools": [{"name": "search", "serverName": "docs", "tokens": 900, "isLoaded": false}],
        "memoryFiles": [{"path": "/w/CLAUDE.md", "type": "Project", "tokens": 1_200}],
    });
    seed_runtime(
        &services,
        &conv_id,
        &json!({"used": 600_000, "size": 1_000_000}),
        Some(&report),
    )
    .await;
    let resp = app.clone().oneshot(get_with_token(&uri, &token)).await.unwrap();
    let data = body_json(resp).await["data"].clone();
    assert_eq!(data["source"], "cli");
    assert_eq!(data["percent"], 60.0);
    assert_eq!(data["updated_at"], 1_795_000_000_000_i64);
    assert_eq!(
        data["auto_compact"],
        json!({"supported": true, "mode": "auto", "threshold_tokens": 960_000, "until_tokens": 360_000})
    );
    assert_eq!(
        data["categories"][1],
        json!({"id": "mcp_tools_deferred", "label_key": "conversation.context.category.mcp_tools_deferred",
               "name": "MCP tools (deferred)", "tokens": 42_000, "deferred": true, "estimated": false})
    );
    assert_eq!(
        data["items"]["mcp_tools"][0],
        json!({"name": "search", "server": "docs", "tokens": 900, "loaded": false})
    );
    assert_eq!(
        data["items"]["memory_files"][0],
        json!({"path": "/w/CLAUDE.md", "kind": "Project", "tokens": 1_200})
    );

    // Owner-scoped: another user gets 404; no session gets 401.
    let (token2, _csrf2) = setup_and_login(&mut app, &services, "user2", "pass123").await;
    let resp = app.clone().oneshot(get_with_token(&uri, &token2)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let anonymous = axum::http::Request::builder()
        .uri(&uri)
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(anonymous).await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_auto_compact_setting_is_per_conversation_validated_and_claude_only() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Auto compact", "acp").await;
    let uri = format!("/api/conversations/{conv_id}/auto-compact");
    let put = |body: serde_json::Value, token: &str, csrf: &str| json_with_token("PUT", &uri, body, token, csrf);

    // A backend without the setting: refused with its own code.
    set_backend(&services, &conv_id, "codex", "finished").await;
    let resp = app
        .clone()
        .oneshot(put(json!({"mode": "60"}), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(resp).await["code"], "AUTO_COMPACT_UNSUPPORTED");

    set_backend(&services, &conv_id, "claude", "finished").await;
    for bad in [
        json!({"mode": "55"}),
        json!({"mode": 60}),
        json!({}),
        json!({"mode": "60", "x": 1}),
    ] {
        let resp = app.clone().oneshot(put(bad.clone(), &token, &csrf)).await.unwrap();
        assert!(resp.status().is_client_error(), "{bad}: {}", resp.status());
    }
    let resp = app
        .clone()
        .oneshot(put(json!({"mode": "60"}), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let data = body_json(resp).await["data"].clone();
    assert_eq!((&data["supported"], &data["mode"]), (&json!(true), &json!("60")));

    // Persisted on the conversation (what the next agent start reads) …
    let (extra,): (String,) = sqlx::query_as("SELECT extra FROM conversations WHERE id = ?")
        .bind(&conv_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let extra: serde_json::Value = serde_json::from_str(&extra).unwrap();
    assert_eq!(extra["auto_compact"], "60");
    assert_eq!(extra["backend"], "claude", "other extra keys are kept");
    // … and shown by the panel once there is usage.
    seed_runtime(&services, &conv_id, &json!({"used": 10, "size": 100}), None).await;
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/conversations/{conv_id}/context"), &token))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["data"]["auto_compact"]["mode"], "60");

    let (token2, csrf2) = setup_and_login(&mut app, &services, "user2", "pass123").await;
    let resp = app
        .clone()
        .oneshot(put(json!({"mode": "off"}), &token2, &csrf2))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let no_csrf = axum::http::Request::builder()
        .method("PUT")
        .uri(&uri)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"mode":"off"}"#))
        .unwrap();
    let resp = app.oneshot(no_csrf).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn compact_is_refused_while_a_turn_runs_and_where_unsupported() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Compact", "acp").await;
    let uri = format!("/api/conversations/{conv_id}/compact");
    let post = |token: &str, csrf: &str| json_with_token("POST", &uri, json!({}), token, csrf);

    // No compact command on this backend.
    set_backend(&services, &conv_id, "gemini", "finished").await;
    let resp = app.clone().oneshot(post(&token, &csrf)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(resp).await["code"], "COMPACT_UNSUPPORTED");

    // A turn is running: 409 TURN_RUNNING, nothing is sent.
    set_backend(&services, &conv_id, "claude", "running").await;
    let resp = app.clone().oneshot(post(&token, &csrf)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(resp).await["code"], "TURN_RUNNING");
    let (messages,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
        .bind(&conv_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(messages, 0, "a refused compact leaves no message");

    let (token2, csrf2) = setup_and_login(&mut app, &services, "user2", "pass123").await;
    let resp = app.clone().oneshot(post(&token2, &csrf2)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let no_csrf = axum::http::Request::builder()
        .method("POST")
        .uri(&uri)
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(no_csrf).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}
