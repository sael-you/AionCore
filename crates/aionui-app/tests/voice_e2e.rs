//! Voice routes mounted in the full app (card C9 S1a): the §3.5 identity rule
//! holds in `--local` mode (the default-user injection used by other routes is
//! not trusted) and the global CSRF layer covers voice in session mode.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::{body_json, get_request, setup_and_login};

const ORIGIN: &str = "https://voice-e2e.test";

fn set_origin_env() {
    // SAFETY: each test sets the same value before building its router; no
    // test reads a different value concurrently.
    unsafe {
        std::env::set_var("AIONUI_VOICE_ALLOWED_ORIGINS", ORIGIN);
        // Non-local modes also check writes against the app policy (C11).
        std::env::set_var("AIONUI_ALLOWED_ORIGINS", ORIGIN);
    }
}

#[tokio::test]
async fn local_mode_voice_rejects_injected_default_user_but_accepts_session_cookie() {
    set_origin_env();
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("router");

    // Other routes inject the local default user without a token…
    let resp = router.clone().oneshot(get_request("/api/settings")).await.unwrap();
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    // …voice does not.
    let resp = router
        .clone()
        .oneshot(get_request("/api/voice/settings"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");

    // A real session cookie works.
    let hash = aionui_auth::hash_password("Voice-Test-Pass-1").unwrap();
    let user = services.user_repo.create_user("voiceuser", &hash).await.unwrap();
    let token = services
        .jwt_service
        .sign_with_session_generation(&user.id, "voiceuser", user.session_generation)
        .unwrap();
    let cookie = format!("aionui-session={token}; aionui-csrf-token=c1");
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/voice/settings")
                .header("cookie", &cookie)
                .header("origin", ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["enabled"], false);

    // Local mode has no global CSRF layer; voice adds its own.
    let put = |csrf: Option<&str>| {
        let mut builder = Request::builder()
            .method("PUT")
            .uri("/api/voice/settings")
            .header("cookie", &cookie)
            .header("origin", ORIGIN)
            .header("content-type", "application/json");
        if let Some(csrf) = csrf {
            builder = builder.header("x-csrf-token", csrf);
        }
        builder.body(Body::from(r#"{"daily_budget_usd": 2}"#)).unwrap()
    };
    let resp = router.clone().oneshot(put(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
    let resp = router.clone().oneshot(put(Some("c1"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Bearer tokens are refused even when valid (cookie is the only credential).
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/voice/status")
                .header("authorization", format!("Bearer {token}"))
                .header("origin", ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["code"], "VOICE_CREDENTIAL_CONFLICT");
    services.database.close().await;
}

#[tokio::test]
async fn session_mode_voice_is_behind_the_global_csrf_layer() {
    set_origin_env();
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();
    let mut router = aionui_app::create_router(&services).await.expect("router");
    let (token, csrf) = setup_and_login(&mut router, &services, "voiceuser", "Voice-Test-Pass-1").await;
    let cookie = format!("aionui-session={token}; aionui-csrf-token={csrf}");

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/voice/status")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["live_calls"], 0);

    // Voice off by default: admission refused, never reaching a provider.
    let post = |csrf_header: Option<&str>| {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/voice/sessions")
            .header("cookie", &cookie)
            .header("origin", ORIGIN)
            .header("content-type", "application/json");
        if let Some(value) = csrf_header {
            builder = builder.header("x-csrf-token", value);
        }
        builder.body(Body::from(r#"{"sdp_offer": "v=0\r\n"}"#)).unwrap()
    };
    let resp = router.clone().oneshot(post(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
    let resp = router.clone().oneshot(post(Some(&csrf))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "VOICE_DISABLED");
    services.database.close().await;
}

/// S1b mounts: the agent `say` route sits on the runtime-token router and
/// only speaks for conversations the user sent to by voice; clips are behind
/// the voice user guards.
#[tokio::test]
async fn voice_agent_route_and_clip_endpoint_are_mounted() {
    use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
    set_origin_env();
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();
    let mut router = aionui_app::create_router(&services).await.expect("router");
    let (token, csrf) = setup_and_login(&mut router, &services, "voiceuser", "Voice-Test-Pass-1").await;
    let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'voiceuser'")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv-voice-agent', ?, 'Kanban build', 'acp', '{}', 'finished', 1, 1)",
    )
    .bind(&user_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO voice_settings (user_id, enabled, updated_at) VALUES (?, 1, 1)")
        .bind(&user_id)
        .execute(services.database.pool())
        .await
        .unwrap();
    let say = |headers: Option<(&str, &str)>| {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/runtime/voice/say")
            .header("content-type", "application/json");
        if let Some((user, token)) = headers {
            builder = builder
                .header("x-aionui-user-id", user)
                .header("x-aionui-conversation-id", "conv-voice-agent")
                .header("x-aionui-runtime-token", token);
        }
        builder.body(Body::from(r#"{"text":"tests are green"}"#)).unwrap()
    };
    let resp = router.clone().oneshot(say(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = router.clone().oneshot(say(Some((&user_id, "forged")))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let runtime_token = services
        .runtime_token_service
        .issue(
            &user_id,
            "conv-voice-agent",
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token;
    let resp = router
        .clone()
        .oneshot(say(Some((&user_id, &runtime_token))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "no voice request was sent to it");

    let cookie = format!("aionui-session={token}; aionui-csrf-token={csrf}");
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/voice/readbacks/rb_missing")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    services.database.close().await;
}
