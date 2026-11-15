//! Card C11 (AionCore part) through the real router: non-local CORS
//! allow-list (2), browser-origin rule for writes (3b), WebSocket origin
//! guard (4b), access-token-only `/ws` (4) — and the hard requirement that
//! local mode (`AuthIdentityMode::Local`) behaves exactly as before.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::build_app;
use tower::ServiceExt;

async fn local_app() -> (axum::Router, aionui_app::AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

async fn session_app(policy: aionui_auth::OriginPolicy) -> (axum::Router, aionui_app::AppServices) {
    session_app_with_cookies(policy, None).await
}

/// Webui services; `cookies` overrides the env-derived cookie config (e.g.
/// `CookieConfig::for_identity(true, true)` for the prefixed names).
async fn session_app_with_cookies(
    policy: aionui_auth::OriginPolicy,
    cookies: Option<aionui_auth::CookieConfig>,
) -> (axum::Router, aionui_app::AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let mut services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();
    services.origin_policy = Arc::new(policy);
    if let Some(cookies) = cookies {
        services.cookie_config = Arc::new(cookies);
    }
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

fn public_policy(allowed: &[&str]) -> aionui_auth::OriginPolicy {
    aionui_auth::OriginPolicy::new(Some("https://y.talkseeds.com"), allowed)
}

fn preflight(origin: &str) -> Request<Body> {
    Request::builder()
        .method("OPTIONS")
        .uri("/api/conversations")
        .header("origin", origin)
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap()
}

fn ws_upgrade(path: &str, origin: Option<&str>, fetch_site: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(path)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    if let Some(site) = fetch_site {
        builder = builder.header("sec-fetch-site", site);
    }
    builder.body(Body::empty()).unwrap()
}

fn cross_site_post(path: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("origin", "https://evil.example")
        .header("sec-fetch-site", "cross-site")
        .body(Body::from("{}"))
        .unwrap()
}

async fn status_and_code(app: &axum::Router, req: Request<Body>) -> (StatusCode, Option<String>) {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let code = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|json| json["code"].as_str().map(str::to_owned));
    (status, code)
}

fn api_read(site: &str, origin: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/api/auth/status")
        .header("sec-fetch-site", site)
        .header("sec-fetch-mode", "cors");
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    builder.body(Body::empty()).unwrap()
}

// -- Local mode: no behaviour change ----------------------------------------

#[tokio::test]
async fn local_mode_has_no_origin_rule_on_api_reads() {
    let (app, services) = local_app().await;
    let (status, code) = status_and_code(&app, api_read("cross-site", Some("https://evil.example"))).await;
    assert_eq!(status, StatusCode::OK, "{code:?}");
    services.database.close().await;
}

#[tokio::test]
async fn local_mode_keeps_any_origin_cors() {
    let (app, services) = local_app().await;
    let response = app.oneshot(preflight("https://evil.example")).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap()),
        Some("*")
    );
    services.database.close().await;
}

#[tokio::test]
async fn local_mode_has_no_origin_or_csrf_check_on_writes() {
    let (app, services) = local_app().await;
    let (status, code) = status_and_code(&app, cross_site_post("/api/projects/none/board/settings")).await;
    // Reaches the handler (board 404), not an origin/CSRF refusal.
    assert_ne!(status, StatusCode::FORBIDDEN, "{code:?}");
    assert_ne!(code.as_deref(), Some("ORIGIN_FORBIDDEN"));
    services.database.close().await;
}

#[tokio::test]
async fn local_mode_keeps_websocket_upgrades_from_any_origin() {
    let (app, services) = local_app().await;
    for path in ["/ws", "/api/stt/stream"] {
        let (status, code) = status_and_code(
            &app,
            ws_upgrade(path, Some("https://x.y.talkseeds.com"), Some("same-site")),
        )
        .await;
        assert_ne!(code.as_deref(), Some("ORIGIN_FORBIDDEN"), "{path}: {status}");
    }
    services.database.close().await;
}

#[tokio::test]
async fn local_mode_keeps_the_legacy_cookie_names() {
    let (_app, services) = local_app().await;
    assert_eq!(services.cookie_config.names, aionui_auth::CookieNames::LEGACY);
    services.database.close().await;
}

// -- Non-local (UserSession) ------------------------------------------------

#[tokio::test]
async fn webui_without_an_allow_list_has_no_cors_layer() {
    let (app, services) = session_app(public_policy(&[])).await;
    let response = app.oneshot(preflight("https://evil.example")).await.unwrap();
    assert!(response.headers().get("access-control-allow-origin").is_none());
    services.database.close().await;
}

#[tokio::test]
async fn webui_cors_answers_listed_origins_only() {
    let (app, services) = session_app(public_policy(&["https://app.example"])).await;
    let listed = app.clone().oneshot(preflight("https://app.example")).await.unwrap();
    assert_eq!(
        listed
            .headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap()),
        Some("https://app.example")
    );
    assert_eq!(
        listed
            .headers()
            .get("access-control-allow-credentials")
            .map(|v| v.to_str().unwrap()),
        Some("true")
    );
    let foreign = app.oneshot(preflight("https://x.y.talkseeds.com")).await.unwrap();
    assert!(foreign.headers().get("access-control-allow-origin").is_none());
    services.database.close().await;
}

#[tokio::test]
async fn webui_refuses_cross_site_writes_but_not_non_browser_clients() {
    let (app, services) = session_app(public_policy(&[])).await;
    let (status, code) = status_and_code(&app, cross_site_post("/api/projects/none/board/settings")).await;
    assert_eq!(
        (status, code.as_deref()),
        (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN"))
    );
    // Helper CLI shape: runtime-token headers, no browser headers → past
    // CSRF/origin (then refused by auth for the invalid token, not by origin).
    let helper = Request::builder()
        .method("POST")
        .uri("/api/projects/none/board/settings")
        .header("content-type", "application/json")
        .header("x-aionui-runtime-token", "invalid")
        .header("x-aionui-user-id", "system_default_user")
        .header("x-aionui-conversation-id", "c")
        .body(Body::from("{}"))
        .unwrap();
    let (status, code) = status_and_code(&app, helper).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{code:?}");
    services.database.close().await;
}

#[tokio::test]
async fn webui_websocket_upgrades_refuse_untrusted_origins() {
    let (app, services) = session_app(public_policy(&[])).await;
    for path in ["/ws", "/api/stt/stream"] {
        let (status, code) = status_and_code(
            &app,
            ws_upgrade(path, Some("https://x.y.talkseeds.com"), Some("same-site")),
        )
        .await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN")),
            "{path}"
        );
        let (_, code) = status_and_code(
            &app,
            ws_upgrade(path, Some("https://y.talkseeds.com"), Some("same-origin")),
        )
        .await;
        assert_ne!(
            code.as_deref(),
            Some("ORIGIN_FORBIDDEN"),
            "{path}: same-origin passes the guard"
        );
    }
    services.database.close().await;
}

#[tokio::test]
async fn webui_websocket_refuses_a_refresh_token() {
    let (_app, services) = build_app().await;
    let ws_state = aionui_app::build_ws_state(&services, Arc::new(aionui_realtime::NoopMessageRouter));
    let refresh = services
        .jwt_service
        .sign_refresh("system_default_user", "admin", 0)
        .unwrap();
    assert!(!(ws_state.token_validator)(&refresh));
    assert!((ws_state.token_user_resolver)(refresh).await.is_none());
    let access = services
        .jwt_service
        .sign_with_session_generation("system_default_user", "admin", 0)
        .unwrap();
    assert!((ws_state.token_validator)(&access));
    services.database.close().await;
}

#[tokio::test]
async fn webui_refuses_cross_site_api_reads_without_a_trusted_origin() {
    let (app, services) = session_app(public_policy(&["https://app.example"])).await;
    let (status, code) = status_and_code(&app, api_read("same-site", None)).await;
    assert_eq!(
        (status, code.as_deref()),
        (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN"))
    );
    let (status, _) = status_and_code(&app, api_read("cross-site", Some("https://app.example"))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = status_and_code(&app, api_read("same-origin", None)).await;
    assert_eq!(status, StatusCode::OK);
    services.database.close().await;
}

#[tokio::test]
async fn webui_stt_stream_refuses_a_refresh_token() {
    let (app, services) = session_app(public_policy(&[])).await;
    let with_bearer = |token: &str| {
        let mut req = ws_upgrade("/api/stt/stream", None, None);
        req.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        req
    };
    let refresh = services
        .jwt_service
        .sign_refresh("system_default_user", "admin", 0)
        .unwrap();
    let (status, _) = status_and_code(&app, with_bearer(&refresh)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let access = services
        .jwt_service
        .sign_with_session_generation("system_default_user", "admin", 0)
        .unwrap();
    let (status, _) = status_and_code(&app, with_bearer(&access)).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED, "an access token passes auth");
    services.database.close().await;
}

#[tokio::test]
async fn webui_ws_ignores_a_legacy_session_cookie_in_prefixed_mode() {
    let (_app, services) = session_app_with_cookies(
        public_policy(&[]),
        Some(aionui_auth::CookieConfig::for_identity(true, true)),
    )
    .await;
    let ws_state = aionui_app::build_ws_state(&services, Arc::new(aionui_realtime::NoopMessageRouter));
    let with_cookie = |cookie: &str| {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cookie", cookie.parse().unwrap());
        headers
    };
    assert_eq!((ws_state.token_extractor)(&with_cookie("aionui-session=tossed")), None);
    assert_eq!(
        (ws_state.token_extractor)(&with_cookie("aionui-session=tossed; __Host-aionui-session=real")),
        Some("real".to_owned())
    );
    services.database.close().await;
}

#[tokio::test]
async fn webui_login_sets_prefixed_cookie_names_through_the_router() {
    let (app, services) = session_app_with_cookies(
        public_policy(&[]),
        Some(aionui_auth::CookieConfig::for_identity(true, true)),
    )
    .await;
    let hash = aionui_auth::hash_password("StrongP@ss1").unwrap();
    services
        .user_repo
        .set_system_user_credentials("admin", &hash)
        .await
        .unwrap();
    let login = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"username":"admin","password":"StrongP@ss1"}"#))
        .unwrap();
    let response = app.oneshot(login).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let names: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|cookie| {
            assert!(cookie.contains("Secure"), "{cookie}");
            cookie.split('=').next().unwrap().to_owned()
        })
        .collect();
    for expected in [
        "__Host-aionui-session",
        "__Secure-aionui-refresh",
        "__Host-aionui-csrf-token",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "{expected} missing in {names:?}"
        );
    }
    assert!(names.iter().all(|name| name.starts_with("__")), "{names:?}");
    services.database.close().await;
}

// -- Mobile app: Bearer refresh without cookies (switch prerequisite) -------

fn bearer_refresh(bearer: &str, refresh: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/auth/refresh")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"));
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder
        .body(Body::from(serde_json::json!({ "token": refresh }).to_string()))
        .unwrap()
}

#[tokio::test]
async fn webui_bearer_refresh_without_cookies_passes_csrf() {
    let (app, services) = session_app_with_cookies(
        public_policy(&[]),
        Some(aionui_auth::CookieConfig::for_identity(true, true)),
    )
    .await;
    let access = services
        .jwt_service
        .sign_with_session_generation("system_default_user", "admin", 0)
        .unwrap();
    let refresh = services
        .jwt_service
        .sign_refresh("system_default_user", "admin", 0)
        .unwrap();
    let (status, code) = status_and_code(&app, bearer_refresh(&access, &refresh, None)).await;
    assert_eq!(status, StatusCode::OK, "{code:?}");
    // The refresh route still verifies its own token: junk is 401, not 403.
    let (status, code) = status_and_code(&app, bearer_refresh(&access, "junk", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{code:?}");
    // A browser (any session cookie) gets no exemption.
    for cookie in ["__Host-aionui-session=s", "aionui-session=s"] {
        let (status, code) = status_and_code(&app, bearer_refresh(&access, &refresh, Some(cookie))).await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("CSRF_INVALID")),
            "{cookie}"
        );
    }
    services.database.close().await;
}

#[tokio::test]
async fn webui_an_exempt_bearer_write_is_still_authenticated() {
    let (app, services) = session_app(public_policy(&[])).await;
    let write = |bearer: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/projects/none/board/settings")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::from("{}"))
            .unwrap()
    };
    let (status, code) = status_and_code(&app, write("junk")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{code:?}");
    services.database.close().await;
}

#[tokio::test]
async fn local_mode_bearer_refresh_is_unchanged() {
    let (app, services) = local_app().await;
    let refresh = services
        .jwt_service
        .sign_refresh("system_default_user", "admin", 0)
        .unwrap();
    // Local identity has no CSRF layer: with or without a cookie, never CSRF_INVALID.
    for cookie in [None, Some("aionui-session=s")] {
        let (_, code) = status_and_code(&app, bearer_refresh("x", &refresh, cookie)).await;
        assert_ne!(code.as_deref(), Some("CSRF_INVALID"), "{cookie:?}");
    }
    services.database.close().await;
}

// -- AionPro: upstream behaviour (C11 L4) -----------------------------------

#[tokio::test]
async fn aionpro_keeps_mirror_cors_and_no_origin_rules() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some("bootstrap-secret".to_string()),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let app = aionui_app::create_router(&services).await.expect("build router");
    let response = app.clone().oneshot(preflight("https://evil.example")).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap()),
        Some("https://evil.example")
    );
    let (_, code) = status_and_code(&app, cross_site_post("/api/projects/none/board/settings")).await;
    assert_ne!(code.as_deref(), Some("ORIGIN_FORBIDDEN"));
    let (_, code) = status_and_code(&app, api_read("cross-site", None)).await;
    assert_ne!(code.as_deref(), Some("ORIGIN_FORBIDDEN"));
    let (_, code) = status_and_code(
        &app,
        ws_upgrade("/ws", Some("https://x.y.talkseeds.com"), Some("same-site")),
    )
    .await;
    assert_ne!(code.as_deref(), Some("ORIGIN_FORBIDDEN"));
    assert_eq!(services.cookie_config.names, aionui_auth::CookieNames::LEGACY);
    services.database.close().await;
}
