//! C38 / TASK-136 K1: `/api/devices/*` behind the same guards as
//! `/api/browser/*` (session + trusted Origin on writes), local identity.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::common::body_json;

const ORIGIN: &str = "https://y.talkseeds.com";
const PASSWORD: &str = "StrongP@ss1";

async fn app() -> (axum::Router, aionui_app::AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let mut services = crate::common::test_services(db, &config).await;
    services.origin_policy = Arc::new(aionui_auth::OriginPolicy::new(Some(ORIGIN), &[]));
    services.cookie_config = Arc::new(aionui_auth::CookieConfig::for_identity(false, false));
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

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

fn req(method: &str, path: &str, session: Option<&str>, origin: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(s) = session {
        b = b.header("cookie", format!("aionui-session={s}"));
    }
    if let Some(o) = origin {
        b = b.header("origin", o);
    }
    b.body(Body::from(body.to_owned())).unwrap()
}

#[tokio::test]
async fn devices_need_a_session_writes_need_a_trusted_origin() {
    let (app, services) = app().await;
    let anon = app
        .clone()
        .oneshot(req("GET", "/api/devices", None, None, ""))
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);

    let session = login(&app, &services).await;
    let list = app
        .clone()
        .oneshot(req("GET", "/api/devices", Some(&session), None, ""))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let json = body_json(list).await;
    let ids: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["id"].as_str())
        .collect();
    assert!(ids.contains(&"server_browser:chrome"), "{ids:?}");
    assert!(ids.contains(&"addon:artemis"));

    let body = r#"{"enabled": false}"#;
    for origin in [None, Some("https://evil.example")] {
        let w = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/devices/server_browser:chrome",
                Some(&session),
                origin,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(w.status(), StatusCode::FORBIDDEN, "{origin:?}");
    }
    let ok = app
        .clone()
        .oneshot(req(
            "PUT",
            "/api/devices/server_browser:chrome",
            Some(&session),
            Some(ORIGIN),
            body,
        ))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let row = body_json(ok).await;
    assert_eq!(row["data"]["enabled"], false);
    assert_eq!(row["data"]["kind"], "server_browser");

    let missing = app
        .clone()
        .oneshot(req(
            "PUT",
            "/api/devices/phone:nope",
            Some(&session),
            Some(ORIGIN),
            body,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(missing).await["code"], "DEVICE_NOT_FOUND");

    let unknown_field = app
        .clone()
        .oneshot(req(
            "PUT",
            "/api/devices/server_browser:chrome",
            Some(&session),
            Some(ORIGIN),
            r#"{"enabled": true, "x": 1}"#,
        ))
        .await
        .unwrap();
    assert!(unknown_field.status().is_client_error());
    assert_ne!(unknown_field.status(), StatusCode::OK);

    let job = app
        .clone()
        .oneshot(req("GET", "/api/devices/jobs/job_x", Some(&session), None, ""))
        .await
        .unwrap();
    assert_eq!(job.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(job).await["code"], "DEVICE_JOB_NOT_FOUND");
}
