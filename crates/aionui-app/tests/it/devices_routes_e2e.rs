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

/// C38 W3: the agent bridge's routes take ONLY a valid conversation runtime
/// token (no session), and the token's user + conversation are the caller.
#[tokio::test]
async fn runtime_device_routes_need_a_conversation_runtime_token() {
    use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
    let (app, services) = app().await;
    let runtime = |token: Option<&str>, user: &str, method: &str, path: &str, body: &str| {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("x-aionui-user-id", user)
            .header("x-aionui-conversation-id", "conv-1");
        if let Some(t) = token {
            b = b.header("x-aionui-runtime-token", t);
        }
        b.body(Body::from(body.to_owned())).unwrap()
    };
    let user = "system_default_user";
    for token in [None, Some("forged")] {
        let r = app
            .clone()
            .oneshot(runtime(token, user, "GET", "/api/runtime/devices", ""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{token:?}");
    }
    // A browser session is not a runtime token.
    let session = login(&app, &services).await;
    let r = app
        .clone()
        .oneshot(req("GET", "/api/runtime/devices", Some(&session), Some(ORIGIN), ""))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let issued = services.runtime_token_service.issue(
        user,
        "conv-1",
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );
    let list = app
        .clone()
        .oneshot(runtime(Some(&issued.token), user, "GET", "/api/runtime/devices", ""))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let json = body_json(list).await;
    assert_eq!(json["ok"], true);
    assert_eq!(json["devices"], serde_json::json!([]), "no phone or Mac paired");
    // The token is bound to its user: another user id with it is refused.
    let other = app
        .clone()
        .oneshot(runtime(
            Some(&issued.token),
            "someone_else",
            "GET",
            "/api/runtime/devices",
            "",
        ))
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::UNAUTHORIZED);

    let call = app
        .clone()
        .oneshot(runtime(
            Some(&issued.token),
            user,
            "POST",
            "/api/runtime/devices/call",
            r#"{"device_id": "phone:nope", "verb": "tap", "args": {}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(call.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(call).await["code"], "DEVICE_NOT_FOUND");
    // Add-on runs: token required; an unknown add-on is a coded 404.
    let anon = app
        .clone()
        .oneshot(runtime(None, user, "POST", "/api/runtime/devices/addon-run", "{}"))
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
    let run = app
        .clone()
        .oneshot(runtime(
            Some(&issued.token),
            user,
            "POST",
            "/api/runtime/devices/addon-run",
            r#"{"addon_id": "addon:nope:artemis", "phone_id": "phone:nope:x", "task": "open the settings"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(run.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(run).await["code"], "DEVICE_NOT_FOUND");
    let bad = app
        .clone()
        .oneshot(runtime(
            Some(&issued.token),
            user,
            "POST",
            "/api/runtime/devices/call",
            r#"{"x": 1}"#,
        ))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(bad).await["code"], "DEVICE_BAD_MESSAGE");
    // TASK-175: a conversation cannot set a device's access mode. The owner
    // routes do not take a runtime token, and the runtime router has none.
    let access_body = r#"{"mode": "full", "full_duration": "until_off", "include_sensitive": true}"#;
    for (method, path, body) in [
        ("PUT", "/api/devices/phone:nope:x/access", access_body),
        (
            "PUT",
            "/api/devices/phone:nope:x/access/trusted-apps",
            r#"{"apps": ["com.android.settings"]}"#,
        ),
        (
            "POST",
            "/api/devices/approvals/apr_x",
            r#"{"decision": "allow", "always": true}"#,
        ),
    ] {
        // Without an Origin the guard refuses first (403); with the trusted
        // Origin the missing owner session does (401). Never accepted.
        let bare = app
            .clone()
            .oneshot(runtime(Some(&issued.token), user, method, path, body))
            .await
            .unwrap();
        assert_eq!(
            bare.status(),
            StatusCode::FORBIDDEN,
            "{method} {path} without an Origin"
        );
        let mut with_origin = runtime(Some(&issued.token), user, method, path, body);
        with_origin.headers_mut().insert("origin", ORIGIN.parse().unwrap());
        let r = app.clone().oneshot(with_origin).await.unwrap();
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} with a runtime token only"
        );
    }
    for path in [
        "/api/runtime/devices/access",
        "/api/runtime/devices/phone:nope:x/access",
    ] {
        let r = app
            .clone()
            .oneshot(runtime(Some(&issued.token), user, "PUT", path, access_body))
            .await
            .unwrap();
        assert!(
            matches!(r.status(), StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED),
            "{path}: {}",
            r.status()
        );
    }
    // The owner's session reaches the route (no such device here: coded 404),
    // and only with a trusted Origin.
    let owner = app
        .clone()
        .oneshot(req(
            "PUT",
            "/api/devices/phone:nope:x/access",
            Some(&session),
            Some(ORIGIN),
            access_body,
        ))
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(owner).await["code"], "DEVICE_NOT_FOUND");
    let no_origin = app
        .clone()
        .oneshot(req(
            "PUT",
            "/api/devices/phone:nope:x/access",
            Some(&session),
            Some("https://evil.example"),
            access_body,
        ))
        .await
        .unwrap();
    assert_eq!(no_origin.status(), StatusCode::FORBIDDEN);
    // A bad body without a token is still 401: the token is checked first.
    let anon_bad = app
        .clone()
        .oneshot(runtime(None, user, "POST", "/api/runtime/devices/call", r#"{"x": 1}"#))
        .await
        .unwrap();
    assert_eq!(anon_bad.status(), StatusCode::UNAUTHORIZED);
}
