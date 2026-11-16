//! C34 / TASK-125: `/api/browser/extension` is reachable without a session
//! (the extension has none) but only from the pinned extension origin; every
//! other route keeps its layers (404s stay JSON with security headers).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aionui_app::{AppConfig, AppServices};

async fn build_app() -> axum::Router {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    aionui_app::create_router(&services).await.expect("build router")
}

fn upgrade(origin: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("GET")
        .uri("/api/browser/extension")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
    if let Some(o) = origin {
        b = b.header("origin", o);
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn the_pinned_origin_passes_without_a_session_and_others_are_refused() {
    let app = build_app().await;
    // No session, pinned origin: past auth, CSRF and the origin layer; only
    // the in-process request cannot actually upgrade (426, not 401/403).
    let resp = app
        .clone()
        .oneshot(upgrade(Some("chrome-extension://bdgcogndgcbcmaijgnhahjddbaclknbb")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);

    for origin in [
        Some("https://evil.example"),
        Some("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        None,
    ] {
        let resp = app.clone().oneshot(upgrade(origin)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{origin:?}");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "BROWSER_BAD_MESSAGE", "{json}");
    }

    // The owner routes next to it still need a session.
    let devices = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/browser/devices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(devices.status(), StatusCode::UNAUTHORIZED);
}
