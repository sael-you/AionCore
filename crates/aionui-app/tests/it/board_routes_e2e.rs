//! Board routers through the REAL app router, in `AuthIdentityMode::Local`
//! and `UserSession`, for the user, system (supervisor header) and agent
//! (runtime token) actors (SPEC §2, §10).

use crate::common;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_app, setup_and_login};
use serde_json::{Value, json};
use tower::ServiceExt;

const LOCAL_USER: &str = "system_default_user";
const PROJECT: &str = "proj-board-e2e";
const CONV: &str = "conv-board-e2e";

async fn local_app() -> (axum::Router, aionui_app::AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = crate::common::test_services(db, &config).await;
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

async fn seed_project(services: &aionui_app::AppServices, user: &str, project: &str, conversation: &str) {
    let pool = services.database.pool();
    sqlx::query("INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, 'P', 'standard', 1, 1)")
        .bind(project)
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, project_id, created_at, updated_at) \
         VALUES (?, ?, 'c', 'acp', '{\"backend\":\"claude\"}', 'finished', ?, 1, 1)",
    )
    .bind(conversation)
    .bind(user)
    .bind(project)
    .execute(pool)
    .await
    .unwrap();
}

fn token(services: &aionui_app::AppServices, user: &str, conversation: &str) -> String {
    services
        .runtime_token_service
        .issue(
            user,
            conversation,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token
}

/// A request builder carrying the chosen credentials.
#[derive(Clone, Default)]
struct Auth {
    bearer: Option<String>,
    /// Also send the bearer as the session cookie (a browser session).
    session_cookie: bool,
    csrf: Option<String>,
    supervisor: bool,
    runtime: Option<(String, String, String)>,
}

fn request(method: &str, uri: &str, body: Option<Value>, auth: &Auth) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(bearer) = &auth.bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    let mut cookies = Vec::new();
    if let (true, Some(bearer)) = (auth.session_cookie, &auth.bearer) {
        cookies.push(format!("aionui-session={bearer}"));
    }
    if let Some(csrf) = &auth.csrf {
        builder = builder.header("x-csrf-token", csrf);
        cookies.push(format!("aionui-csrf-token={csrf}"));
    }
    if !cookies.is_empty() {
        builder = builder.header("cookie", cookies.join("; "));
    }
    if auth.supervisor {
        builder = builder.header("x-aionui-board-actor", "supervisor");
    }
    if let Some((user, conversation, token)) = &auth.runtime {
        builder = builder
            .header("x-aionui-user-id", user)
            .header("x-aionui-conversation-id", conversation)
            .header("x-aionui-runtime-token", token);
    }
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn enable(app: &axum::Router, auth: &Auth) -> Value {
    let (status, body) = send(
        app,
        request(
            "PUT",
            &format!("/api/projects/{PROJECT}/board/settings"),
            Some(json!({ "version": 0, "enabled": true, "template": "studio" })),
            auth,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn create_card(app: &axum::Router, auth: &Auth, title: &str) -> (StatusCode, Value) {
    send(
        app,
        request(
            "POST",
            &format!("/api/projects/{PROJECT}/board/cards"),
            Some(json!({ "title": title })),
            auth,
        ),
    )
    .await
}

#[tokio::test]
async fn local_mode_user_system_and_agent_actors() {
    let (app, services) = local_app().await;
    seed_project(&services, LOCAL_USER, PROJECT, CONV).await;
    let owner = Auth::default();

    // Never-enabled board: 200 with enabled:false, version 0, last_seq 0.
    let (status, body) = send(
        &app,
        request("GET", &format!("/api/projects/{PROJECT}/board"), None, &owner),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["settings"]["enabled"], false);
    assert_eq!(body["data"]["settings"]["version"], 0);
    assert_eq!(body["data"]["last_seq"], 0);

    let settings = enable(&app, &owner).await;
    assert_eq!(settings["data"]["template"], "studio");
    let (status, card) = create_card(&app, &owner, "owner card").await;
    assert_eq!(status, StatusCode::CREATED, "{card}");
    assert_eq!(card["data"]["creator_kind"], "user");
    let card_id = card["data"]["id"].as_str().unwrap().to_owned();

    // Mutations need a JSON body.
    let (status, body) = send(
        &app,
        request("POST", &format!("/api/board/cards/{card_id}/archive"), None, &owner),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("board_invalid"))
    );

    // System actor (Local only): may create and move, never force or gate.
    let system = Auth {
        supervisor: true,
        ..Default::default()
    };
    let (status, card2) = create_card(&app, &system, "supervisor card").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(card2["data"]["creator_ref"], "supervisor");
    let (status, body) = send(
        &app,
        request(
            "POST",
            &format!("/api/board/cards/{card_id}/move"),
            Some(json!({ "to_column": "inbox", "expected_column": "ideas", "force": true })),
            &system,
        ),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("board_system_forbidden"))
    );
    let (status, body) = send(
        &app,
        request(
            "POST",
            &format!("/api/board/cards/{card_id}/gate"),
            Some(json!({ "decision": "approve", "idempotency_key": "g1" })),
            &system,
        ),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("board_system_forbidden"))
    );
    let (status, _) = send(
        &app,
        request(
            "POST",
            &format!("/api/board/cards/{card_id}/move"),
            Some(json!({ "to_column": "inbox", "expected_column": "ideas" })),
            &system,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Agent router: validated in Local mode too.
    let agent = Auth {
        runtime: Some((LOCAL_USER.into(), CONV.into(), token(&services, LOCAL_USER, CONV))),
        ..Default::default()
    };
    let (status, body) = send(&app, request("GET", "/api/runtime/board/status", None, &agent)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["enabled"], true);
    assert_eq!(body["data"]["project_id"], PROJECT);
    let (status, body) = send(
        &app,
        request(
            "POST",
            "/api/runtime/board/cards",
            Some(json!({ "title": "agent card" })),
            &agent,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["success"], true);
    assert_eq!(body["meta"]["command"], "board create");
    let number = body["data"]["number"].as_i64().unwrap();
    let (status, body) = send(
        &app,
        request(
            "POST",
            &format!("/api/runtime/board/cards/{number}/move"),
            Some(json!({ "to_column": "inbox" })),
            &agent,
        ),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("board_agent_move_denied"))
    );
    let (status, body) = send(
        &app,
        request(
            "PUT",
            &format!("/api/runtime/board/cards/TASK-{number}/reviews/qa"),
            Some(json!({ "verdict": "approve" })),
            &agent,
        ),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("board_department_mismatch"))
    );
    let (status, body) = send(
        &app,
        request(
            "GET",
            &format!("/api/runtime/board/cards/{number}?packet=true"),
            None,
            &agent,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"]["text"].as_str().unwrap().contains("<card-data>"));
    let forged = Auth {
        runtime: Some((LOCAL_USER.into(), CONV.into(), "forged".into())),
        ..Default::default()
    };
    let (status, body) = send(&app, request("GET", "/api/runtime/board/status", None, &forged)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("runtime_auth_failed"))
    );

    // Chat notices for the agent's conversation.
    let (status, body) = send(
        &app,
        request("GET", &format!("/api/conversations/{CONV}/board-events"), None, &owner),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["has_more"], false);
    services.database.close().await;
}

#[tokio::test]
async fn user_session_mode_enforces_auth_channels_csrf_and_isolation() {
    let (mut app, services) = build_app().await;
    let (bearer, csrf) = setup_and_login(&mut app, &services, "admin", "Passw0rd!x").await;
    seed_project(&services, LOCAL_USER, PROJECT, CONV).await;
    let owner = Auth {
        bearer: Some(bearer.clone()),
        csrf: Some(csrf.clone()),
        ..Default::default()
    };

    // Unauthenticated → 401; missing CSRF on a write → 403.
    let (status, _) = send(
        &app,
        request("GET", &format!("/api/projects/{PROJECT}/board"), None, &Auth::default()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A browser session (cookie) without CSRF; a cookieless Bearer client is
    // exempt from CSRF (C11-switch).
    let no_csrf = Auth {
        bearer: Some(bearer.clone()),
        session_cookie: true,
        ..Default::default()
    };
    let (status, _) = create_card(&app, &no_csrf, "x").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    enable(&app, &owner).await;
    let (status, card) = create_card(&app, &owner, "owner card").await;
    assert_eq!(status, StatusCode::CREATED, "{card}");

    // The supervisor header outside Local mode → 403.
    let system = Auth {
        supervisor: true,
        ..owner.clone()
    };
    let (status, body) = create_card(&app, &system, "x").await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("board_system_forbidden"))
    );

    // An agent riding the auth middleware's runtime-token channel is refused
    // by the runtime-token route scope (C13) before the board handler; the
    // handler's own `board_use_agent_router` refusal stays as a second layer
    // (aionui-board routes.rs unit test).
    let token = token(&services, LOCAL_USER, CONV);
    let channel = Auth {
        runtime: Some((LOCAL_USER.into(), CONV.into(), token.clone())),
        ..Default::default()
    };
    let (status, body) = send(
        &app,
        request("GET", &format!("/api/projects/{PROJECT}/board"), None, &channel),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("RUNTIME_SCOPE"))
    );
    let (status, body) = send(&app, request("GET", "/api/runtime/board/status", None, &channel)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the agent router validates the token itself, CSRF-exempt"
    );
    assert_eq!(body["data"]["project_id"], PROJECT);

    // Cross-user isolation: another account sees no board of this project.
    let (bob_bearer, bob_csrf) = setup_and_login(&mut app, &services, "bob", "Passw0rd!y").await;
    let bob = Auth {
        bearer: Some(bob_bearer),
        csrf: Some(bob_csrf),
        ..Default::default()
    };
    let (status, body) = send(
        &app,
        request("GET", &format!("/api/projects/{PROJECT}/board"), None, &bob),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("board_not_found"))
    );
    let card_id = card["data"]["id"].as_str().unwrap();
    let (status, _) = send(&app, request("GET", &format!("/api/board/cards/{card_id}"), None, &bob)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    services.database.close().await;
}

#[tokio::test]
async fn temp_projects_answer_board_no_project() {
    let (app, services) = local_app().await;
    sqlx::query("INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('tmp', ?, 'T', 'temp', 1, 1)")
        .bind(LOCAL_USER)
        .execute(services.database.pool())
        .await
        .unwrap();
    let (status, body) = send(&app, request("GET", "/api/projects/tmp/board", None, &Auth::default())).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("board_no_project"))
    );
    services.database.close().await;
}
