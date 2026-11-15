//! Voice routes mounted in the full app (card C9 S1a): the §3.5 identity rule
//! holds in `--local` mode (the default-user injection used by other routes is
//! not trusted), every voice write and the WS upgrade need an allowed Origin
//! in both identities, local identity needs no CSRF header (voice has no CSRF
//! layer of its own) and the global C11 CSRF layer covers voice in session
//! (webui) mode.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::{body_json, get_request, setup_and_login};

const ORIGIN: &str = "https://voice-e2e.test";
/// The production case: an origin that is NOT configured, accepted in local
/// identity only because it names the host the browser addressed.
const SAME_ORIGIN: &str = "https://y.talkseeds.com";
const SAME_HOST: &str = "y.talkseeds.com";

/// Every voice write and the `/api/voice/ws` upgrade, with `origin` (None =
/// no header) and an optional CSRF header, all must be refused with 403.
/// `forwarded_host`: the web host's `X-Forwarded-Host` (`Host` is then the
/// backend's loopback address).
async fn assert_origin_refused(
    router: &axum::Router,
    cookie: &str,
    csrf: Option<&str>,
    origin: Option<&str>,
    forwarded_host: Option<&str>,
) {
    let writes = [
        ("PUT", "/api/voice/settings", r#"{"daily_budget_usd": 2}"#),
        ("POST", "/api/voice/sessions", r#"{"sdp_offer": "v=0\r\n"}"#),
        ("POST", "/api/voice/sessions/vs_x/end", "{}"),
    ];
    for (method, uri, body) in writes {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie)
            .header("content-type", "application/json");
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        if let Some(csrf) = csrf {
            builder = builder.header("x-csrf-token", csrf);
        }
        if let Some(host) = forwarded_host {
            builder = builder
                .header("host", "127.0.0.1:41234")
                .header("x-forwarded-host", host);
        }
        let resp = router
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri} origin={origin:?}");
        let code = body_json(resp).await["code"].as_str().unwrap_or_default().to_owned();
        let expected = if origin.is_some() {
            "ORIGIN_FORBIDDEN"
        } else {
            "ORIGIN_REQUIRED"
        };
        assert_eq!(code, expected, "{method} {uri} origin={origin:?}");
    }
    let mut builder = Request::builder()
        .uri("/api/voice/ws")
        .header("cookie", cookie)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    if let Some(host) = forwarded_host {
        builder = builder
            .header("host", "127.0.0.1:41234")
            .header("x-forwarded-host", host);
    }
    let resp = router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "ws upgrade origin={origin:?}");
    let code = body_json(resp).await["code"].as_str().unwrap_or_default().to_owned();
    // The shared aionui-auth rule (or, in webui, the global C11 WebSocket
    // origin layer before it) answers with the same codes.
    let expected = if origin.is_some() {
        "ORIGIN_FORBIDDEN"
    } else {
        "ORIGIN_REQUIRED"
    };
    assert_eq!(code, expected, "ws upgrade origin={origin:?}");
}

/// Serve `router` on a real socket and open `/api/voice/ws` with the session
/// cookie, `Origin` and the web host's `X-Forwarded-Host`; the HTTP status on
/// refusal.
async fn real_ws_upgrade(
    router: axum::Router,
    cookie: &str,
    origin: &str,
    forwarded_host: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, u16> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let mut request = format!("ws://{addr}/api/voice/ws").into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert("cookie", cookie.parse().unwrap());
    headers.insert("origin", origin.parse().unwrap());
    headers.insert("x-forwarded-host", forwarded_host.parse().unwrap());
    match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("voice ws connect failed: {other}"),
    }
}

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
    let cookie = format!("aionui-session={token}");
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

    // Local identity: no CSRF header (the C11 UI sends none there); the
    // strict Origin check refuses a missing or foreign Origin on every write
    // and on the WS upgrade.
    assert_origin_refused(&router, &cookie, None, None, None).await;
    assert_origin_refused(&router, &cookie, None, Some("https://evil.test"), None).await;
    let put = Request::builder()
        .method("PUT")
        .uri("/api/voice/settings")
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"daily_budget_usd": 2}"#))
        .unwrap();
    let resp = router.clone().oneshot(put).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "allowed Origin, no CSRF header");

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
    let resp = router.clone().oneshot(post(Some("wrong"))).await.unwrap();
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID", "mismatched token");
    let put = |csrf_header: Option<&str>| {
        let mut builder = Request::builder()
            .method("PUT")
            .uri("/api/voice/settings")
            .header("cookie", &cookie)
            .header("origin", ORIGIN)
            .header("content-type", "application/json");
        if let Some(value) = csrf_header {
            builder = builder.header("x-csrf-token", value);
        }
        builder.body(Body::from(r#"{"daily_budget_usd": 2}"#)).unwrap()
    };
    let resp = router.clone().oneshot(put(None)).await.unwrap();
    assert_eq!(
        body_json(resp).await["code"],
        "CSRF_INVALID",
        "settings write without CSRF"
    );
    let resp = router.clone().oneshot(put(Some(&csrf))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Webui identity: a valid CSRF token never replaces the Origin check.
    assert_origin_refused(&router, &cookie, Some(&csrf), None, None).await;
    assert_origin_refused(&router, &cookie, Some(&csrf), Some("https://evil.test"), None).await;
    // Webui identity never uses the web host's same-origin rule: an origin
    // that is not configured stays refused even when it names the
    // forwarded host.
    assert_origin_refused(&router, &cookie, Some(&csrf), Some(SAME_ORIGIN), Some(SAME_HOST)).await;

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

#[tokio::test]
async fn local_identity_voice_accepts_same_origin_via_the_web_host() {
    set_origin_env();
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("router");
    let hash = aionui_auth::hash_password("Voice-Test-Pass-1").unwrap();
    let user = services.user_repo.create_user("voiceuser", &hash).await.unwrap();
    let token = services
        .jwt_service
        .sign_with_session_generation(&user.id, "voiceuser", user.session_generation)
        .unwrap();
    let cookie = format!("aionui-session={token}");

    // Same-origin write: Host rewritten by the web host, X-Forwarded-Host =
    // the browser's Host, no CSRF header.
    let put = Request::builder()
        .method("PUT")
        .uri("/api/voice/settings")
        .header("cookie", &cookie)
        .header("origin", SAME_ORIGIN)
        .header("host", "127.0.0.1:41234")
        .header("x-forwarded-host", SAME_HOST)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"daily_budget_usd": 2}"#))
        .unwrap();
    let resp = router.clone().oneshot(put).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "same-origin write");

    // The same-origin upgrade completes on a real socket.
    assert!(
        real_ws_upgrade(router.clone(), &cookie, SAME_ORIGIN, SAME_HOST)
            .await
            .is_ok(),
        "same-origin upgrade"
    );

    // Another host, or no Origin, is refused on every write and the upgrade
    // even with the forwarded host present.
    assert_origin_refused(&router, &cookie, None, Some("https://evil.test"), Some(SAME_HOST)).await;
    assert_origin_refused(&router, &cookie, None, None, Some(SAME_HOST)).await;
    // Without the web host's X-Forwarded-Host the unconfigured origin is foreign.
    assert_origin_refused(&router, &cookie, None, Some(SAME_ORIGIN), None).await;
    services.database.close().await;
}

/// TASK-68 (director): in local identity (production) a real session cookie
/// for `system_default_user` — the Token Pool internal login — is accepted on
/// every voice route and the `/api/voice/ws` upgrade, behind the web host.
/// Voice only refuses requests that carry no session cookie.
#[tokio::test]
async fn local_identity_accepts_a_system_default_user_session_on_every_voice_route_and_the_ws() {
    set_origin_env();
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("router");
    let user = services
        .user_repo
        .find_by_id("system_default_user")
        .await
        .unwrap()
        .expect("local identity seeds system_default_user");
    let token = services
        .jwt_service
        .sign_with_session_generation(
            &user.id,
            user.username.as_deref().unwrap_or("system_default_user"),
            user.session_generation,
        )
        .unwrap();
    let cookie = format!("aionui-session={token}");
    let call = |method: &str, uri: &str, body: Option<&str>| {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", &cookie)
            .header("origin", SAME_ORIGIN)
            .header("host", "127.0.0.1:41234")
            .header("x-forwarded-host", SAME_HOST)
            .header("content-type", "application/json");
        builder
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
            .unwrap()
    };
    // Each route answers as the authenticated user: success, or the route's
    // own refusal — never 401 or an Origin/CSRF refusal.
    for (method, uri, body, status, code) in [
        ("GET", "/api/voice/settings", None, StatusCode::OK, None),
        (
            "PUT",
            "/api/voice/settings",
            Some(r#"{"enabled": true}"#),
            StatusCode::OK,
            None,
        ),
        ("GET", "/api/voice/status", None, StatusCode::OK, None),
        ("GET", "/api/voice/sessions", None, StatusCode::OK, None),
        (
            "POST",
            "/api/voice/sessions",
            Some(r#"{"sdp_offer": "v=0\r\n"}"#),
            StatusCode::PRECONDITION_FAILED,
            Some("VOICE_NO_PROVIDER"),
        ),
        (
            "POST",
            "/api/voice/sessions/vs_missing/end",
            Some("{}"),
            StatusCode::NOT_FOUND,
            Some("NOT_FOUND"),
        ),
        (
            "GET",
            "/api/voice/readbacks/rb_missing",
            None,
            StatusCode::NOT_FOUND,
            None,
        ),
    ] {
        let resp = router.clone().oneshot(call(method, uri, body)).await.unwrap();
        let got = resp.status();
        let json = body_json(resp).await;
        assert_eq!(got, status, "{method} {uri}: {json}");
        if let Some(code) = code {
            assert_eq!(json["code"], code, "{method} {uri}: {json}");
        }
    }

    // The WebSocket: upgrade, hello, armed (voice was just turned on).
    use futures_util::{SinkExt, StreamExt};
    let mut ws = real_ws_upgrade(router.clone(), &cookie, SAME_ORIGIN, SAME_HOST)
        .await
        .expect("voice ws upgrade with the system_default_user session");
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        r#"{"name":"voice.hello","data":{"device":"web","capabilities":{}}}"#.into(),
    ))
    .await
    .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("a frame after hello")
        .expect("socket open")
        .unwrap();
    let frame: serde_json::Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(frame["name"], "voice.state", "{frame}");
    assert_eq!(frame["data"]["state"], "armed", "{frame}");

    // Without the cookie the same requests are refused.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/voice/settings")
                .header("origin", SAME_ORIGIN)
                .header("host", "127.0.0.1:41234")
                .header("x-forwarded-host", SAME_HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        real_ws_upgrade(router, "a=b", SAME_ORIGIN, SAME_HOST).await.err(),
        Some(401)
    );
    services.database.close().await;
}
