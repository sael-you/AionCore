use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::middleware;
use axum::routing::{get, post};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use tower::ServiceExt;

use aionui_auth::{
    AuthIdentityMode, AuthState, CookieConfig, CurrentUser, IRuntimeTokenVerifier, JwtService, RateLimiter,
    TokenPayload, api_rate_limit_middleware, auth_middleware, auth_rate_limit_middleware,
    authenticated_action_rate_limit_middleware, csrf_middleware, security_headers_middleware,
};
use aionui_db::{IUserRepository, SqliteUserRepository, UserStatus, UserType, init_database_memory};

/// CSRF state with an empty origin policy (no trusted browser origin).
fn csrf_state(cookie_config: Arc<CookieConfig>) -> aionui_auth::CsrfState {
    aionui_auth::CsrfState {
        cookie_config,
        origin_policy: Some(Arc::new(aionui_auth::OriginPolicy::default())),
    }
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

// ============================================================
// T12.1 — Security response headers
// ============================================================

#[tokio::test]
async fn t12_1_security_headers_on_get() {
    let app = Router::new()
        .route("/test", get(|| async { "ok" }))
        .layer(middleware::from_fn(security_headers_middleware));

    let resp = app
        .oneshot(Request::get("/test").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
    assert_eq!(resp.headers().get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(resp.headers().get("x-xss-protection").unwrap(), "1; mode=block");
    assert_eq!(
        resp.headers().get("referrer-policy").unwrap(),
        "strict-origin-when-cross-origin"
    );
}

// ============================================================
// T12.2 — CSRF protection
// ============================================================

fn csrf_app() -> Router {
    let config = Arc::new(CookieConfig {
        secure: false,
        same_site: "Lax",
        names: aionui_auth::CookieNames::LEGACY,
    });
    Router::new()
        .route("/api/test", post(|| async { "ok" }))
        .route("/login", post(|| async { "logged in" }))
        .route("/api/auth/qr-login", post(|| async { "qr ok" }))
        .route("/get-test", get(|| async { "get ok" }))
        .route(
            "/internal/antigravity-hook/{conversation_id}",
            post(|| async { "hook ok" }),
        )
        .layer(middleware::from_fn_with_state(csrf_state(config), csrf_middleware))
}

#[tokio::test]
async fn t12_2_get_requests_bypass_csrf() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::get("/get-test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// agy's PreToolUse callback must reach its handler without a CSRF token.
///
/// The hook is a local `aioncore antigravity-hook` process: no user session, no
/// cookie, no token to present. It authenticates with a per-conversation
/// `x-aionui-hook-token` that the handler checks. When CSRF rejected it, every
/// call came back 403, the hook read that as "no answer" and denied, and agy
/// turns produced no tool frames at all — measured 2026-08-14, 6/6 calls
/// rejected.
#[tokio::test]
async fn the_antigravity_hook_is_not_blocked_by_csrf() {
    let app = csrf_app();
    let resp = app
        .oneshot(
            Request::post("/internal/antigravity-hook/conv-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a hook callback with no CSRF token must still reach the handler"
    );
}

#[tokio::test]
async fn t12_2_post_without_csrf_token_rejected() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::post("/api/test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn t12_2_post_with_matching_csrf_tokens_accepted() {
    let app = csrf_app();
    let token = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
    let resp = app
        .oneshot(
            Request::post("/api/test")
                .header("cookie", format!("aionui-csrf-token={token}"))
                .header("x-csrf-token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn t12_2_post_with_mismatched_csrf_tokens_rejected() {
    let app = csrf_app();
    let resp = app
        .oneshot(
            Request::post("/api/test")
                .header("cookie", "aionui-csrf-token=token_a")
                .header("x-csrf-token", "token_b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "CSRF_INVALID");
}

// ============================================================
// Auth middleware
// ============================================================

async fn auth_app(jwt_service: Arc<JwtService>) -> Router {
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    protected_auth_app(jwt_service, user_repo)
}

fn protected_auth_app(jwt_service: Arc<JwtService>, user_repo: Arc<dyn IUserRepository>) -> Router {
    protected_auth_app_with_mode(jwt_service, user_repo, AuthIdentityMode::UserSession)
}

fn protected_auth_app_with_mode(
    jwt_service: Arc<JwtService>,
    user_repo: Arc<dyn IUserRepository>,
    identity_mode: AuthIdentityMode,
) -> Router {
    protected_auth_app_with_mode_and_verifier(jwt_service, user_repo, identity_mode, None)
}

fn protected_auth_app_with_mode_and_verifier(
    jwt_service: Arc<JwtService>,
    user_repo: Arc<dyn IUserRepository>,
    identity_mode: AuthIdentityMode,
    runtime_token_verifier: Option<Arc<dyn IRuntimeTokenVerifier>>,
) -> Router {
    let state = AuthState {
        jwt_service,
        user_repo,
        identity_mode,
        runtime_token_verifier,
        cookie_names: aionui_auth::CookieNames::LEGACY,
    };

    Router::new()
        .route("/protected", get(|| async { "ok" }))
        .route_layer(middleware::from_fn_with_state(state, auth_middleware))
}

/// Like the protected app, but echoes the injected `CurrentUser.id` so tests
/// can assert which identity the runtime-token channel resolved.
fn identity_echo_app(
    jwt_service: Arc<JwtService>,
    user_repo: Arc<dyn IUserRepository>,
    identity_mode: AuthIdentityMode,
    runtime_token_verifier: Option<Arc<dyn IRuntimeTokenVerifier>>,
) -> Router {
    let state = AuthState {
        jwt_service,
        user_repo,
        identity_mode,
        runtime_token_verifier,
        cookie_names: aionui_auth::CookieNames::LEGACY,
    };

    Router::new()
        .route(
            "/whoami",
            get(|user: axum::Extension<CurrentUser>| async move { user.id.clone() }),
        )
        .route_layer(middleware::from_fn_with_state(state, auth_middleware))
}

fn expired_token(jwt_service: &JwtService, secret: &str, user_id: &str, username: &str) -> String {
    let token = jwt_service.sign(user_id, username).unwrap();
    let mut validation = Validation::default();
    validation.validate_exp = false;
    validation.validate_aud = false;

    let mut claims = decode::<TokenPayload>(&token, &DecodingKey::from_secret(secret.as_bytes()), &validation)
        .unwrap()
        .claims;

    claims.iat = 1000;
    claims.exp = 1001;

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn auth_middleware_missing_token_returns_unauthorized_code() {
    let app = auth_app(Arc::new(JwtService::new("middleware_test_secret".into()))).await;

    let resp = app
        .oneshot(Request::get("/protected").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_invalid_token_returns_unauthorized_code() {
    let app = auth_app(Arc::new(JwtService::new("middleware_test_secret".into()))).await;

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, "Bearer not-a-valid-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_expired_token_returns_unauthorized_code() {
    let secret = "middleware_test_secret";
    let jwt_service = Arc::new(JwtService::new(secret.into()));
    let token = expired_token(&jwt_service, secret, "system_default_user", "system_default_user");
    let app = auth_app(jwt_service).await;

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_missing_user_returns_unauthorized_code() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let token = jwt_service.sign("missing_user", "ghost").unwrap();
    let app = auth_app(jwt_service).await;

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_session_generation_mismatch_returns_unauthorized_code() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let token = jwt_service
        .sign_with_session_generation("system_default_user", "system_default_user", 0)
        .unwrap();
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    repo.increment_session_generation("system_default_user").await.unwrap();
    let app = protected_auth_app(jwt_service, repo as Arc<dyn IUserRepository>);

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_middleware_aionpro_rejects_local_user_token() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let token = jwt_service
        .sign_with_session_generation("system_default_user", "system_default_user", 0)
        .unwrap();
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    let app = protected_auth_app_with_mode(jwt_service, repo as Arc<dyn IUserRepository>, AuthIdentityMode::AionPro);

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "USER_CONTEXT_REQUIRED");
}

#[tokio::test]
async fn auth_middleware_database_error_returns_internal_error_code() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let token = jwt_service.sign("system_default_user", "system_default_user").unwrap();
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    db.close().await;
    let app = protected_auth_app(jwt_service, user_repo);

    let resp = app
        .oneshot(
            Request::get("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "INTERNAL_ERROR");
    assert_eq!(json["error"], "Internal server error.");
    let error = json["error"].as_str().unwrap();
    assert!(!error.contains("Database error"));
    assert!(!error.contains("Authentication service unavailable"));
    assert!(!error.to_ascii_lowercase().contains("closed"));
    assert!(!error.to_ascii_lowercase().contains("sqlx"));
}

#[tokio::test]
async fn t12_2_login_exempt_from_csrf() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::post("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn t12_2_qr_login_exempt_from_csrf() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::post("/api/auth/qr-login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn t12_2_csrf_cookie_set_on_first_request() {
    let app = csrf_app();
    let resp = app
        .oneshot(Request::get("/get-test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set_cookie.contains("aionui-csrf-token="));
    // NOT HttpOnly (JS must read it)
    assert!(!set_cookie.contains("HttpOnly"));
}

// ============================================================
// Rate limiter middleware
// ============================================================

fn rate_limit_app(limiter: Arc<RateLimiter>) -> Router {
    Router::new()
        .route("/test", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(limiter, api_rate_limit_middleware))
}

#[tokio::test]
async fn api_rate_limit_allows_within_quota() {
    let limiter = Arc::new(RateLimiter::new(3, Duration::from_secs(60)));
    let app = rate_limit_app(limiter);

    for _ in 0..3 {
        let resp = app
            .clone()
            .oneshot(Request::get("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn api_rate_limit_rejects_over_quota() {
    let limiter = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let app = rate_limit_app(limiter);

    // First two pass
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(Request::get("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Third rejected
    let resp = app
        .oneshot(Request::get("/test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn auth_rate_limit_skips_successful_responses() {
    let limiter = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let app = Router::new()
        .route("/login", post(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(limiter, auth_rate_limit_middleware));

    // Successful responses (200) don't count toward the limit
    for _ in 0..5 {
        let resp = app
            .clone()
            .oneshot(Request::post("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn auth_rate_limit_counts_failed_responses() {
    let limiter = Arc::new(RateLimiter::new(2, Duration::from_secs(60)));
    let app = Router::new()
        .route("/login", post(|| async { StatusCode::UNAUTHORIZED }))
        .layer(middleware::from_fn_with_state(limiter, auth_rate_limit_middleware));

    // First two failures pass through
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(Request::post("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // Third request blocked by rate limiter
    let resp = app
        .oneshot(Request::post("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn authenticated_action_limit_uses_user_id_key() {
    let limiter = Arc::new(RateLimiter::new(1, Duration::from_secs(60)));

    // Handler that injects a CurrentUser extension before the limiter
    let app = Router::new()
        .route("/action", post(|| async { "done" }))
        .layer(middleware::from_fn_with_state(
            limiter.clone(),
            authenticated_action_rate_limit_middleware,
        ))
        .layer(middleware::from_fn(
            |mut request: axum::extract::Request, next: axum::middleware::Next| async {
                request.extensions_mut().insert(CurrentUser {
                    id: "user_42".into(),
                    username: "admin".into(),
                    user_type: UserType::Local,
                    status: UserStatus::Active,
                });
                Ok::<_, std::convert::Infallible>(next.run(request).await)
            },
        ));

    // First request passes
    let resp = app
        .clone()
        .oneshot(Request::post("/action").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second request for same user is rate limited
    let resp = app
        .oneshot(Request::post("/action").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ============================================================
// T12.3 — Cookie security attributes (via CookieConfig)
// ============================================================

#[test]
fn t12_3_session_cookie_is_httponly() {
    let config = CookieConfig {
        secure: false,
        same_site: "Lax",
        names: aionui_auth::CookieNames::LEGACY,
    };
    let cookie = config.build_session_cookie("token123");
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));
    assert!(cookie.contains("Max-Age="));
}

#[test]
fn t12_3_session_cookie_secure_when_https() {
    let config = CookieConfig {
        secure: true,
        same_site: "Strict",
        names: aionui_auth::CookieNames::LEGACY,
    };
    let cookie = config.build_session_cookie("token123");
    assert!(cookie.contains("; Secure"));
    assert!(cookie.contains("SameSite=Strict"));
}

// ============================================================
// T13 — Token extraction strategy
// ============================================================

#[test]
fn t13_1_authorization_header_takes_priority() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, "Bearer header_tok".parse().unwrap());
    headers.insert(header::COOKIE, "aionui-session=cookie_tok".parse().unwrap());
    assert_eq!(
        aionui_auth::extract_token_from_headers(&headers),
        Some("header_tok".into())
    );
}

#[test]
fn t13_2_cookie_fallback() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::COOKIE, "aionui-session=fallback_tok".parse().unwrap());
    assert_eq!(
        aionui_auth::extract_token_from_headers(&headers),
        Some("fallback_tok".into())
    );
}

#[test]
fn t13_3_no_token_returns_none() {
    let headers = axum::http::HeaderMap::new();
    assert_eq!(aionui_auth::extract_token_from_headers(&headers), None);
}

// ============================================================
// Runtime-token channel (conversation helper CLI)
// ============================================================

struct MatchVerifier {
    token: &'static str,
    user_id: &'static str,
    conversation_id: &'static str,
}

impl IRuntimeTokenVerifier for MatchVerifier {
    fn verify_conversation_helper(&self, token: &str, user_id: &str, conversation_id: &str) -> bool {
        token == self.token && user_id == self.user_id && conversation_id == self.conversation_id
    }

    /// Test scope: only `GET /whoami` (the identity echo route).
    fn allows_route(&self, method: &axum::http::Method, path: &str) -> bool {
        method == axum::http::Method::GET && path == "/whoami"
    }
}

fn helper_request(token: Option<&str>, user_id: &str, conversation_id: &str) -> Request<Body> {
    let mut builder = Request::get("/whoami")
        .header("x-aionui-user-id", user_id)
        .header("x-aionui-conversation-id", conversation_id);
    if let Some(token) = token {
        builder = builder.header("x-aionui-runtime-token", token);
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn runtime_token_channel_authenticates_helper_and_injects_bound_user() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let app = identity_echo_app(
        jwt_service,
        repo,
        AuthIdentityMode::UserSession,
        Some(Arc::new(MatchVerifier {
            token: "tok-1",
            user_id: "system_default_user",
            conversation_id: "conv-1",
        })),
    );

    let resp = app
        .oneshot(helper_request(Some("tok-1"), "system_default_user", "conv-1"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"system_default_user");
}

#[tokio::test]
async fn runtime_token_channel_aionpro_authenticates_external_user() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    let user = repo
        .ensure_external_user(
            UserType::Aionpro,
            "ext-user-1",
            aionui_db::ExternalUserProjection {
                username: Some("pro-user".into()),
                email: None,
                avatar_path: None,
            },
        )
        .await
        .unwrap();
    let token: &'static str = "tok-pro";
    let user_id: &'static str = Box::leak(user.id.clone().into_boxed_str());
    let app = identity_echo_app(
        jwt_service,
        repo as Arc<dyn IUserRepository>,
        AuthIdentityMode::AionPro,
        Some(Arc::new(MatchVerifier {
            token,
            user_id,
            conversation_id: "conv-pro",
        })),
    );

    let resp = app
        .oneshot(helper_request(Some(token), user_id, "conv-pro"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(std::str::from_utf8(&body).unwrap(), user_id);
}

#[tokio::test]
async fn runtime_token_channel_rejects_forged_user_header() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let app = identity_echo_app(
        jwt_service,
        repo,
        AuthIdentityMode::UserSession,
        Some(Arc::new(MatchVerifier {
            token: "tok-1",
            user_id: "system_default_user",
            conversation_id: "conv-1",
        })),
    );

    let resp = app
        .oneshot(helper_request(Some("tok-1"), "another_user", "conv-1"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
    assert_eq!(json["error"], "Invalid runtime token");
}

#[tokio::test]
async fn runtime_token_channel_rejects_cross_conversation_token() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let app = identity_echo_app(
        jwt_service,
        repo,
        AuthIdentityMode::UserSession,
        Some(Arc::new(MatchVerifier {
            token: "tok-1",
            user_id: "system_default_user",
            conversation_id: "conv-1",
        })),
    );

    let resp = app
        .oneshot(helper_request(Some("tok-1"), "system_default_user", "conv-other"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["error"], "Invalid runtime token");
}

#[tokio::test]
async fn runtime_token_channel_without_token_returns_authentication_required() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let app = identity_echo_app(
        jwt_service,
        repo,
        AuthIdentityMode::AionPro,
        Some(Arc::new(MatchVerifier {
            token: "tok-1",
            user_id: "system_default_user",
            conversation_id: "conv-1",
        })),
    );

    let resp = app
        .oneshot(helper_request(None, "system_default_user", "conv-1"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
    assert_eq!(json["error"], "Authentication required");
}

#[tokio::test]
async fn runtime_token_channel_disabled_when_verifier_absent() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let app = identity_echo_app(jwt_service, repo, AuthIdentityMode::UserSession, None);

    let resp = app
        .oneshot(helper_request(Some("tok-1"), "system_default_user", "conv-1"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["error"], "Authentication required");
}

#[tokio::test]
async fn runtime_token_channel_aionpro_rejects_local_user_token_binding() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let app = identity_echo_app(
        jwt_service,
        repo,
        AuthIdentityMode::AionPro,
        Some(Arc::new(MatchVerifier {
            token: "tok-1",
            user_id: "system_default_user",
            conversation_id: "conv-1",
        })),
    );

    let resp = app
        .oneshot(helper_request(Some("tok-1"), "system_default_user", "conv-1"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["code"], "USER_CONTEXT_REQUIRED");
}

#[tokio::test]
async fn csrf_exempts_requests_bearing_runtime_token_header() {
    let cookie_config = Arc::new(CookieConfig {
        secure: false,
        same_site: "Lax",
        names: aionui_auth::CookieNames::LEGACY,
    });
    let app = Router::new()
        .route("/api/thing", post(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(
            csrf_state(cookie_config),
            csrf_middleware,
        ));

    // Without the runtime-token header a cookieless POST is rejected.
    let resp = app
        .clone()
        .oneshot(Request::post("/api/thing").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // With the header, CSRF is skipped (token validity is enforced later by
    // the auth middleware, which is not part of this app).
    let resp = app
        .oneshot(
            Request::post("/api/thing")
                .header("x-aionui-runtime-token", "tok-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Card C11 — CSRF runtime-token exemption (3a), browser-origin rule (3b),
// prefixed cookie names (6)
// ---------------------------------------------------------------------------

fn csrf_app_with_policy(policy: aionui_auth::OriginPolicy) -> Router {
    let state = aionui_auth::CsrfState {
        cookie_config: Arc::new(CookieConfig {
            secure: false,
            same_site: "Lax",
            names: aionui_auth::CookieNames::LEGACY,
        }),
        origin_policy: Some(Arc::new(policy)),
    };
    Router::new()
        .route("/api/thing", post(|| async { "ok" }))
        .route("/login", post(|| async { "login ok" }))
        .layer(middleware::from_fn_with_state(state, csrf_middleware))
}

#[tokio::test]
async fn c11_3a_a_session_cookie_disables_the_runtime_token_exemption() {
    let app = csrf_app_with_policy(aionui_auth::OriginPolicy::default());
    // A Bearer header alone (no session cookie) is exempt on its own since the
    // C11 switch prerequisite (mobile app; auth validates the Bearer and never
    // falls back to the runtime channel), so the Bearer case carries a cookie.
    for headers in [
        &[("cookie", "aionui-session=abc")][..],
        &[("authorization", "Bearer abc"), ("cookie", "aionui-session=abc")][..],
    ] {
        let mut builder = Request::post("/api/thing").header("x-aionui-runtime-token", "junk");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let resp = app.clone().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{headers:?} + junk runtime header"
        );
        assert_eq!(json_body(resp).await["code"], "CSRF_INVALID");
    }
}

#[tokio::test]
async fn c11_3b_unsafe_requests_follow_the_origin_rule() {
    let app = csrf_app_with_policy(aionui_auth::OriginPolicy::new(Some("https://y.talkseeds.com"), &[]));
    let post_with = |pairs: &[(&str, &str)]| {
        let mut builder = Request::post("/api/thing")
            .header("x-csrf-token", "t")
            .header("cookie", "aionui-csrf-token=t");
        for (name, value) in pairs {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    };
    // Helper CLI (no browser headers, runtime token) and aion-client (cookie +
    // CSRF, no browser headers) pass.
    let helper = Request::post("/api/thing")
        .header("x-aionui-runtime-token", "tok")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(helper).await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        app.clone().oneshot(post_with(&[])).await.unwrap().status(),
        StatusCode::OK
    );
    // Same-origin browser write passes.
    let same = post_with(&[("sec-fetch-site", "same-origin"), ("origin", "https://y.talkseeds.com")]);
    assert_eq!(app.clone().oneshot(same).await.unwrap().status(), StatusCode::OK);
    // Cross-site fetch (even with a valid double-submit pair) is refused.
    let cross = post_with(&[("sec-fetch-site", "cross-site"), ("origin", "https://evil.example")]);
    let resp = app.clone().oneshot(cross).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(resp).await["code"], "ORIGIN_FORBIDDEN");
    // A sibling dev server is same-site, not same-origin.
    let sibling = post_with(&[("sec-fetch-site", "same-site"), ("origin", "https://x.y.talkseeds.com")]);
    assert_eq!(
        app.clone().oneshot(sibling).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    // Older browser without Sec-Fetch-Site: Origin decides (never Host).
    let foreign = post_with(&[("origin", "https://evil.example"), ("host", "y.talkseeds.com")]);
    assert_eq!(
        app.clone().oneshot(foreign).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    // Exempt paths are still origin-checked.
    let login = Request::post("/login")
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(login).await.unwrap().status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn c11_6_prefixed_names_ignore_a_tossed_unprefixed_session_cookie() {
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let jwt_service = Arc::new(JwtService::new("c11-cookie-secret".into()));
    let token = jwt_service
        .sign_with_session_generation("system_default_user", "admin", 0)
        .unwrap();
    let state = AuthState {
        jwt_service,
        user_repo,
        identity_mode: AuthIdentityMode::UserSession,
        runtime_token_verifier: None,
        cookie_names: aionui_auth::CookieNames::PREFIXED,
    };
    let app = Router::new()
        .route("/protected", get(|| async { "ok" }))
        .route_layer(middleware::from_fn_with_state(state, auth_middleware));
    let with_cookie = |cookie: String| {
        Request::get("/protected")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    };
    let tossed = app
        .clone()
        .oneshot(with_cookie(format!("aionui-session={token}")))
        .await
        .unwrap();
    assert_eq!(
        tossed.status(),
        StatusCode::UNAUTHORIZED,
        "un-prefixed cookie is ignored"
    );
    let real = app
        .oneshot(with_cookie(format!("__Host-aionui-session={token}")))
        .await
        .unwrap();
    assert_eq!(real.status(), StatusCode::OK);
}

fn csrf_app_with(names: aionui_auth::CookieNames, policy: Option<aionui_auth::OriginPolicy>) -> Router {
    let state = aionui_auth::CsrfState {
        cookie_config: Arc::new(CookieConfig {
            secure: true,
            same_site: "Lax",
            names,
        }),
        origin_policy: policy.map(Arc::new),
    };
    Router::new()
        .route("/api/thing", post(|| async { "ok" }).get(|| async { "read" }))
        .layer(middleware::from_fn_with_state(state, csrf_middleware))
}

#[tokio::test]
async fn c11_6_prefixed_csrf_ignores_a_tossed_legacy_csrf_cookie() {
    let app = csrf_app_with(
        aionui_auth::CookieNames::PREFIXED,
        Some(aionui_auth::OriginPolicy::default()),
    );
    let post_with = |cookie: &str| {
        Request::post("/api/thing")
            .header("cookie", cookie)
            .header("x-csrf-token", "tossed")
            .body(Body::empty())
            .unwrap()
    };
    // A sibling-domain cookie under the legacy name plus a matching header: refused.
    let tossed = app
        .clone()
        .oneshot(post_with("aionui-csrf-token=tossed"))
        .await
        .unwrap();
    assert_eq!(tossed.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(tossed).await["code"], "CSRF_INVALID");
    // The real __Host- cookie with the matching header passes.
    let real = app.oneshot(post_with("__Host-aionui-csrf-token=tossed")).await.unwrap();
    assert_eq!(real.status(), StatusCode::OK);
}

#[tokio::test]
async fn c11_l4_aionpro_csrf_keeps_upstream_behaviour() {
    // `None` policy (AionPro): no origin rule, the runtime header alone still exempts.
    let app = csrf_app_with(aionui_auth::CookieNames::LEGACY, None);
    let cross_site = app
        .clone()
        .oneshot(
            Request::post("/api/thing")
                .header("sec-fetch-site", "cross-site")
                .header("origin", "https://evil.example")
                .header("cookie", "aionui-csrf-token=t")
                .header("x-csrf-token", "t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_site.status(), StatusCode::OK);
    let helper = app
        .clone()
        .oneshot(
            Request::post("/api/thing")
                .header("x-aionui-runtime-token", "junk")
                .header("cookie", "aionui-session=abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(helper.status(), StatusCode::OK);
    let read = app
        .oneshot(
            Request::get("/api/thing")
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
}

#[tokio::test]
async fn c11_b_l3_cross_site_api_reads_need_navigation_or_a_trusted_origin() {
    let policy = aionui_auth::OriginPolicy::new(Some("https://app.example"), &["https://dev.example"]);
    let app = csrf_app_with(aionui_auth::CookieNames::PREFIXED, Some(policy));
    let get = |site: &str, mode: Option<&str>, origin: Option<&str>| {
        let mut builder = Request::get("/api/thing").header("sec-fetch-site", site);
        if let Some(mode) = mode {
            builder = builder.header("sec-fetch-mode", mode);
        }
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        builder.body(Body::empty()).unwrap()
    };
    for (site, mode, origin, expected) in [
        ("same-origin", Some("cors"), None, StatusCode::OK),
        ("none", Some("navigate"), None, StatusCode::OK),
        ("cross-site", Some("navigate"), None, StatusCode::OK),
        ("cross-site", Some("cors"), Some("https://dev.example"), StatusCode::OK),
        ("same-site", Some("no-cors"), None, StatusCode::FORBIDDEN),
        (
            "cross-site",
            Some("cors"),
            Some("https://evil.example"),
            StatusCode::FORBIDDEN,
        ),
        ("cross-site", None, None, StatusCode::FORBIDDEN),
    ] {
        let resp = app.clone().oneshot(get(site, mode, origin)).await.unwrap();
        assert_eq!(resp.status(), expected, "{site} {mode:?} {origin:?}");
    }
}

// ---------------------------------------------------------------------------
// C11 switch prerequisite — Bearer clients (mobile app) and the webui CSRF layer
// ---------------------------------------------------------------------------

fn bearer_post(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::post(path).header("authorization", "Bearer some-jwt");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn c11_switch_bearer_without_a_session_cookie_is_csrf_exempt() {
    for names in [aionui_auth::CookieNames::LEGACY, aionui_auth::CookieNames::PREFIXED] {
        let app = csrf_app_with(names, Some(aionui_auth::OriginPolicy::default()));
        let resp = app.clone().oneshot(bearer_post("/api/thing", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{names:?}");
        // Other cookies (CSRF, refresh) are not session cookies.
        let resp = app
            .oneshot(bearer_post(
                "/api/thing",
                Some("aionui-csrf-token=x; __Secure-aionui-refresh=r"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{names:?} + non-session cookies");
    }
}

#[tokio::test]
async fn c11_switch_bearer_with_a_session_cookie_still_needs_csrf() {
    for names in [aionui_auth::CookieNames::LEGACY, aionui_auth::CookieNames::PREFIXED] {
        let app = csrf_app_with(names, Some(aionui_auth::OriginPolicy::default()));
        // Either session-cookie name removes the exemption, including the
        // name this mode does not read (a tossed legacy cookie in prefixed mode).
        for cookie in ["__Host-aionui-session=s", "aionui-session=s"] {
            let resp = app
                .clone()
                .oneshot(bearer_post("/api/thing", Some(cookie)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{names:?} {cookie}");
            assert_eq!(json_body(resp).await["code"], "CSRF_INVALID");
        }
        // With the double-submit pair it passes as before.
        let resp = app
            .oneshot(
                Request::post("/api/thing")
                    .header("authorization", "Bearer some-jwt")
                    .header("cookie", format!("aionui-session=s; {}=t", names.csrf))
                    .header("x-csrf-token", "t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{names:?} with CSRF pair");
    }
}

#[tokio::test]
async fn c11_switch_a_session_cookie_in_a_later_cookie_header_still_needs_csrf() {
    for names in [aionui_auth::CookieNames::LEGACY, aionui_auth::CookieNames::PREFIXED] {
        let app = csrf_app_with(names, Some(aionui_auth::OriginPolicy::default()));
        for session in ["__Host-aionui-session=s", "aionui-session=s"] {
            // HTTP/2 lets a client split cookies over several `Cookie` headers.
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/api/thing")
                        .header("authorization", "Bearer some-jwt")
                        .header("cookie", "theme=dark")
                        .header("cookie", session)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{names:?} {session}");
            assert_eq!(json_body(resp).await["code"], "CSRF_INVALID");
        }
    }
}

#[tokio::test]
async fn c11_switch_non_bearer_authorization_is_not_exempt() {
    let app = csrf_app_with(
        aionui_auth::CookieNames::PREFIXED,
        Some(aionui_auth::OriginPolicy::default()),
    );
    for value in ["Basic abc", "Bearer ", "bearer abc"] {
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/thing")
                    .header("authorization", value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{value}");
    }
}

#[tokio::test]
async fn c11_switch_bearer_exemption_still_follows_the_origin_rule() {
    let app = csrf_app_with(
        aionui_auth::CookieNames::PREFIXED,
        Some(aionui_auth::OriginPolicy::new(Some("https://y.talkseeds.com"), &[])),
    );
    let resp = app
        .oneshot(
            Request::post("/api/thing")
                .header("authorization", "Bearer some-jwt")
                .header("sec-fetch-site", "cross-site")
                .header("origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(resp).await["code"], "ORIGIN_FORBIDDEN");
}

#[tokio::test]
async fn c11_switch_aionpro_keeps_csrf_for_bearer_requests() {
    let app = csrf_app_with(aionui_auth::CookieNames::LEGACY, None);
    let resp = app.oneshot(bearer_post("/api/thing", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(resp).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn c11_switch_an_exempt_bearer_is_still_validated_by_auth() {
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let jwt_service = Arc::new(JwtService::new("c11-bearer-secret".into()));
    let access = jwt_service
        .sign_with_session_generation("system_default_user", "admin", 0)
        .unwrap();
    let refresh = jwt_service.sign_refresh("system_default_user", "admin", 0).unwrap();
    let auth = AuthState {
        jwt_service,
        user_repo,
        identity_mode: AuthIdentityMode::UserSession,
        runtime_token_verifier: None,
        cookie_names: aionui_auth::CookieNames::PREFIXED,
    };
    let csrf = aionui_auth::CsrfState {
        cookie_config: Arc::new(CookieConfig::for_identity(true, true)),
        origin_policy: Some(Arc::new(aionui_auth::OriginPolicy::default())),
    };
    // Same order as the real router: CSRF outside, auth inside.
    let app = Router::new()
        .route("/api/thing", post(|| async { "ok" }))
        .route_layer(middleware::from_fn_with_state(auth, auth_middleware))
        .layer(middleware::from_fn_with_state(csrf, csrf_middleware));
    let post_bearer = |token: &str, runtime: bool| {
        let mut builder = Request::post("/api/thing").header("authorization", format!("Bearer {token}"));
        if runtime {
            builder = builder
                .header("x-aionui-runtime-token", "tok")
                .header("x-aionui-user-id", "system_default_user")
                .header("x-aionui-conversation-id", "c");
        }
        builder.body(Body::empty()).unwrap()
    };
    let ok = app.clone().oneshot(post_bearer(&access, false)).await.unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    for (token, runtime) in [("junk", false), ("junk", true), (refresh.as_str(), false)] {
        let resp = app.clone().oneshot(post_bearer(token, runtime)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "bearer {token} runtime header {runtime}"
        );
    }
}

// ============================================================
// Runtime-token route scope (card C13)
// ============================================================

/// `GET /whoami` is inside the test verifier's scope; `GET /owner-only` and
/// `POST /whoami` are not.
fn scoped_app(user_repo: Arc<dyn IUserRepository>, jwt_service: Arc<JwtService>) -> Router {
    let state = AuthState {
        jwt_service,
        user_repo,
        identity_mode: AuthIdentityMode::UserSession,
        runtime_token_verifier: Some(Arc::new(MatchVerifier {
            token: "tok-1",
            user_id: "system_default_user",
            conversation_id: "conv-1",
        })),
        cookie_names: aionui_auth::CookieNames::LEGACY,
    };
    let whoami = |user: axum::Extension<CurrentUser>| async move { user.id.clone() };
    Router::new()
        .route("/whoami", get(whoami).post(whoami))
        .route("/owner-only", get(whoami))
        .route_layer(middleware::from_fn_with_state(state, auth_middleware))
}

fn scoped_request(method: &str, path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("x-aionui-user-id", "system_default_user")
        .header("x-aionui-conversation-id", "conv-1")
        .header("x-aionui-runtime-token", token)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn runtime_token_outside_route_scope_is_refused_with_runtime_scope() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let app = scoped_app(repo, jwt_service);

    let allowed = app
        .clone()
        .oneshot(scoped_request("GET", "/whoami?x=1", "tok-1"))
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);

    for (method, path) in [("GET", "/owner-only"), ("POST", "/whoami")] {
        let resp = app
            .clone()
            .oneshot(scoped_request(method, path, "tok-1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {path}");
        let json = json_body(resp).await;
        assert_eq!(json["code"], aionui_auth::RUNTIME_SCOPE_ERROR_CODE, "{method} {path}");
        assert_eq!(json["code"], "RUNTIME_SCOPE");
    }
}

#[tokio::test]
async fn invalid_runtime_token_outside_scope_is_still_401() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let app = scoped_app(repo, jwt_service);

    let resp = app
        .oneshot(scoped_request("GET", "/owner-only", "wrong-token"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(resp).await;
    assert_eq!(json["error"], "Invalid runtime token");
}

#[tokio::test]
async fn session_jwt_is_not_limited_by_the_runtime_route_scope() {
    let jwt_service = Arc::new(JwtService::new("middleware_test_secret".into()));
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let token = jwt_service.sign("system_default_user", "system_default_user").unwrap();
    let app = scoped_app(repo, jwt_service);

    let resp = app
        .oneshot(
            Request::get("/owner-only")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
