//! MCP market writes (card C15) through the real router after C11, in both
//! identities:
//! - local identity (production): no CSRF layer (the UI sends no CSRF header
//!   there); every write needs the browser session cookie the gateway login
//!   obtained AND a trusted `Origin` — missing or foreign is refused;
//! - webui identity: the same Origin rule, plus the global C11 CSRF layer
//!   (no or wrong token → `CSRF_INVALID`, the real token → through).
//!
//! Local identity in production configures no origin list: same-origin is
//! read from `X-Forwarded-Host`, which the web host overwrites with the
//! browser's `Host` (it rewrites `Host` itself to the backend's loopback
//! address). Webui identity never uses it.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, extract_csrf_token, get_request};
use tower::ServiceExt;

const ORIGIN: &str = "https://y.talkseeds.com";
const PASSWORD: &str = "StrongP@ss1";

/// Every market write route (method, path).
const WRITES: &[(&str, &str)] = &[
    ("POST", "/api/mcp/market/instances"),
    ("PUT", "/api/mcp/market/instances/i1"),
    ("DELETE", "/api/mcp/market/instances/i1"),
    ("POST", "/api/mcp/market/instances/i1/duplicate"),
    ("POST", "/api/mcp/market/instances/i1/revoke"),
    ("POST", "/api/mcp/market/instances/i1/refresh-template"),
    ("PUT", "/api/mcp/market/instances/i1/approval"),
    ("PUT", "/api/mcp/market/instances/i1/secrets/API_KEY"),
    ("DELETE", "/api/mcp/market/instances/i1/secrets/API_KEY"),
    ("POST", "/api/mcp/market/assignments"),
    ("DELETE", "/api/mcp/market/assignments/a1"),
];

async fn app(local: bool) -> (axum::Router, aionui_app::AppServices) {
    app_with_policy(local, aionui_auth::OriginPolicy::new(Some(ORIGIN), &[])).await
}

/// `policy` replaces the env-derived origin list (production local identity:
/// `OriginPolicy::default()`, nothing configured).
async fn app_with_policy(local: bool, policy: aionui_auth::OriginPolicy) -> (axum::Router, aionui_app::AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local,
        ..Default::default()
    };
    let mut services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    services.origin_policy = Arc::new(policy);
    // Legacy cookie names in both identities (http), independent of the env.
    services.cookie_config = Arc::new(aionui_auth::CookieConfig::for_identity(!local, false));
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

/// The gateway's login: `/login` with the instance account (the system user
/// row, `system_default_user`) → the `aionui-session` JWT cookie value.
async fn login(app: &axum::Router, services: &aionui_app::AppServices) -> String {
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
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login");
    body_json(resp).await["token"].as_str().unwrap().to_owned()
}

struct Write<'a> {
    method: &'a str,
    path: &'a str,
    session: Option<&'a str>,
    origin: Option<&'a str>,
    /// (cookie, header) of the CSRF double submit.
    csrf: Option<(&'a str, Option<&'a str>)>,
    /// `X-Forwarded-Host` as the web host writes it (`Host` is then the
    /// backend's loopback address).
    forwarded_host: Option<&'a str>,
}

fn request(w: &Write) -> Request<Body> {
    let mut cookies = Vec::new();
    if let Some(token) = w.session {
        cookies.push(format!("aionui-session={token}"));
    }
    let mut builder = Request::builder()
        .method(w.method)
        .uri(w.path)
        .header("content-type", "application/json");
    if let Some((cookie, header)) = w.csrf {
        cookies.push(format!("aionui-csrf-token={cookie}"));
        if let Some(header) = header {
            builder = builder.header("x-csrf-token", header);
        }
    }
    if !cookies.is_empty() {
        builder = builder.header("cookie", cookies.join("; "));
    }
    if let Some(origin) = w.origin {
        builder = builder.header("origin", origin);
    }
    if let Some(host) = w.forwarded_host {
        builder = builder
            .header("host", "127.0.0.1:41234")
            .header("x-forwarded-host", host);
    }
    builder.body(Body::from("{}")).unwrap()
}

async fn send(app: &axum::Router, w: Write<'_>) -> (StatusCode, Option<String>) {
    let resp = app.clone().oneshot(request(&w)).await.unwrap();
    let status = resp.status();
    let json = body_json(resp).await;
    (status, json["code"].as_str().map(str::to_owned))
}

// -- local identity (production) --------------------------------------------

#[tokio::test]
async fn local_every_market_write_needs_a_trusted_origin() {
    let (app, services) = app(true).await;
    let token = login(&app, &services).await;
    for &(method, path) in WRITES {
        let base = || Write {
            method,
            path,
            session: Some(&token),
            origin: None,
            csrf: None,
            forwarded_host: None,
        };
        let (status, code) = send(&app, base()).await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("ORIGIN_REQUIRED")),
            "{method} {path} without Origin"
        );
        let (status, code) = send(
            &app,
            Write {
                origin: Some("https://evil.example"),
                ..base()
            },
        )
        .await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN")),
            "{method} {path} foreign Origin"
        );
    }
}

#[tokio::test]
async fn local_session_cookie_and_trusted_origin_pass_without_any_csrf_token() {
    let (app, services) = app(true).await;
    let token = login(&app, &services).await;
    // What the browser sends in production: the gateway-obtained session
    // cookie and Origin, no CSRF header (C11 csrf.ts in local identity).
    let (status, code) = send(
        &app,
        Write {
            method: "DELETE",
            path: "/api/mcp/market/instances/missing",
            session: Some(&token),
            origin: Some(ORIGIN),
            csrf: None,
            forwarded_host: None,
        },
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "reached the handler: {code:?}");

    // Without the session cookie the local default user is not accepted.
    let (status, _) = send(
        &app,
        Write {
            method: "DELETE",
            path: "/api/mcp/market/instances/missing",
            session: None,
            origin: Some(ORIGIN),
            csrf: None,
            forwarded_host: None,
        },
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// -- webui identity ---------------------------------------------------------

async fn webui_csrf(app: &axum::Router) -> String {
    let resp = app.clone().oneshot(get_request("/api/auth/status")).await.unwrap();
    extract_csrf_token(&resp).expect("CSRF cookie")
}

#[tokio::test]
async fn webui_every_market_write_needs_a_trusted_origin() {
    let (app, services) = app(false).await;
    let token = login(&app, &services).await;
    let csrf = webui_csrf(&app).await;
    for &(method, path) in WRITES {
        let base = || Write {
            method,
            path,
            session: Some(&token),
            origin: None,
            csrf: Some((&csrf, Some(&csrf))),
            forwarded_host: None,
        };
        let (status, code) = send(&app, base()).await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("ORIGIN_REQUIRED")),
            "{method} {path} without Origin"
        );
        let (status, code) = send(
            &app,
            Write {
                origin: Some("https://evil.example"),
                ..base()
            },
        )
        .await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN")),
            "{method} {path} foreign Origin"
        );
    }
}

#[tokio::test]
async fn webui_market_writes_go_through_the_global_csrf_layer() {
    let (app, services) = app(false).await;
    let token = login(&app, &services).await;
    let csrf = webui_csrf(&app).await;
    let wrong = "0".repeat(64);
    let delete = |header: Option<&str>| {
        request(&Write {
            method: "DELETE",
            path: "/api/mcp/market/instances/missing",
            session: Some(&token),
            origin: Some(ORIGIN),
            csrf: Some((&csrf, header)),
            forwarded_host: None,
        })
    };
    for (label, header) in [("no token", None), ("wrong token", Some(wrong.as_str()))] {
        let resp = app.clone().oneshot(delete(header)).await.unwrap();
        let status = resp.status();
        let code = body_json(resp).await["code"].as_str().map(str::to_owned);
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("CSRF_INVALID")),
            "{label}"
        );
    }
    let resp = app.clone().oneshot(delete(Some(&csrf))).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "the real token reaches the handler"
    );
}

// -- local identity, production config: same-origin via the web host --------

const HOST: &str = "y.talkseeds.com";

#[tokio::test]
async fn local_same_origin_via_the_web_host_passes_without_configured_origins() {
    let (app, services) = app_with_policy(true, aionui_auth::OriginPolicy::default()).await;
    let token = login(&app, &services).await;
    for &(method, path) in WRITES {
        let (status, code) = send(
            &app,
            Write {
                method,
                path,
                session: Some(&token),
                origin: Some(ORIGIN),
                csrf: None,
                forwarded_host: Some(HOST),
            },
        )
        .await;
        assert!(
            status != StatusCode::FORBIDDEN && status != StatusCode::UNAUTHORIZED,
            "{method} {path}: {status} {code:?}"
        );
    }
    let (status, _) = send(
        &app,
        Write {
            method: "DELETE",
            path: "/api/mcp/market/instances/missing",
            session: Some(&token),
            origin: Some(ORIGIN),
            csrf: None,
            forwarded_host: Some(HOST),
        },
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "reached the handler");
}

#[tokio::test]
async fn local_same_origin_rule_refuses_other_hosts_and_missing_origins() {
    let (app, services) = app_with_policy(true, aionui_auth::OriginPolicy::default()).await;
    let token = login(&app, &services).await;
    for &(method, path) in WRITES {
        let base = || Write {
            method,
            path,
            session: Some(&token),
            origin: Some("https://evil.example"),
            csrf: None,
            forwarded_host: Some(HOST),
        };
        let (status, code) = send(&app, base()).await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN")),
            "{method} {path} other host"
        );
        let (status, code) = send(&app, Write { origin: None, ..base() }).await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("ORIGIN_REQUIRED")),
            "{method} {path} missing Origin"
        );
        // Without the web host's X-Forwarded-Host there is nothing to match.
        let (status, code) = send(
            &app,
            Write {
                origin: Some(ORIGIN),
                forwarded_host: None,
                ..base()
            },
        )
        .await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN")),
            "{method} {path} no forwarded host"
        );
    }
}

#[tokio::test]
async fn webui_never_uses_the_same_origin_rule() {
    let (app, services) = app_with_policy(false, aionui_auth::OriginPolicy::default()).await;
    let token = login(&app, &services).await;
    let csrf = webui_csrf(&app).await;
    let (status, code) = send(
        &app,
        Write {
            method: "DELETE",
            path: "/api/mcp/market/instances/missing",
            session: Some(&token),
            origin: Some(ORIGIN),
            csrf: Some((&csrf, Some(&csrf))),
            forwarded_host: Some(HOST),
        },
    )
    .await;
    assert_eq!(
        (status, code.as_deref()),
        (StatusCode::FORBIDDEN, Some("ORIGIN_FORBIDDEN"))
    );
}
