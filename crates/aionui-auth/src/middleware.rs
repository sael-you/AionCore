#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;
use aionui_db::{IUserRepository, UserStatus, UserType};

use crate::JwtService;
use crate::cookie::CookieNames;
use crate::extract::extract_token_named;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthIdentityMode {
    Local,
    UserSession,
    AionPro,
}

/// Header carrying the conversation-runtime helper token minted by the backend
/// and injected into agent subprocess environments as `AIONUI_RUNTIME_TOKEN`.
pub const RUNTIME_TOKEN_HEADER: &str = "x-aionui-runtime-token";
/// Header carrying the acting user id asserted by the helper CLI.
pub const RUNTIME_USER_ID_HEADER: &str = "x-aionui-user-id";
/// Header carrying the conversation id the helper CLI runs inside.
pub const RUNTIME_CONVERSATION_ID_HEADER: &str = "x-aionui-conversation-id";

/// Port for validating conversation-runtime helper tokens.
///
/// Implemented in the composition layer over the agent runtime's token
/// service; `aionui-auth` must not depend on `aionui-ai-agent` directly.
/// A verifier must confirm the token is a live, conversation-helper-scoped
/// token bound to exactly this (user_id, conversation_id) pair.
pub trait IRuntimeTokenVerifier: Send + Sync {
    fn verify_conversation_helper(&self, token: &str, user_id: &str, conversation_id: &str) -> bool;

    /// Route scope of the runtime-token channel (card C13): whether a request
    /// authenticated by a valid runtime token may call `method path`. The
    /// composition layer owns the allowlist (the routes the agent-facing
    /// helper CLIs call); everything else is refused with 403
    /// `RUNTIME_SCOPE`, so a runtime token never acts with full owner scope.
    /// `path` is the request path without the query string.
    fn allows_route(&self, method: &Method, path: &str) -> bool;
}

/// Error code returned when a valid runtime token calls a route outside its
/// allowlist ([`IRuntimeTokenVerifier::allows_route`]).
pub const RUNTIME_SCOPE_ERROR_CODE: &str = "RUNTIME_SCOPE";

/// Authenticated user injected into request extensions by the auth middleware.
///
/// Route handlers extract this from `request.extensions()` to identify
/// the current user.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    /// User ID from the database.
    pub id: String,
    /// Username.
    pub username: String,
    /// Internal identity source for the current user.
    pub user_type: UserType,
    /// Current account status. Authenticated requests only receive active users.
    pub status: UserStatus,
}

impl CurrentUser {
    pub fn local_default() -> Self {
        Self {
            id: "system_default_user".to_string(),
            username: "system_default_user".to_string(),
            user_type: UserType::Local,
            status: UserStatus::Active,
        }
    }
}

/// Which credential channel authenticated a request. Inserted into the
/// request extensions next to [`CurrentUser`] by [`auth_middleware`] and
/// [`local_auth_middleware`], so a handler can refuse a channel it must not
/// serve (the board's user router refuses agent runtime tokens and points
/// them at its agent router). Handlers that depend on it must fail closed
/// when it is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthChannel {
    /// A JWT access token (header or session cookie).
    Session,
    /// The conversation-helper runtime-token channel used by agent
    /// subprocess CLIs; carries the token-bound conversation.
    RuntimeToken { conversation_id: String },
    /// Local mode: no authentication, fixed default user.
    Local,
}

/// Shared state for the authentication middleware.
#[derive(Clone)]
pub struct AuthState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub identity_mode: AuthIdentityMode,
    /// Optional second credential channel for agent-subprocess helper CLIs
    /// (`aioncore config` / `diagnose`), which cannot carry a JWT or cookies.
    /// `None` disables the channel (requests without a JWT are rejected).
    pub runtime_token_verifier: Option<Arc<dyn IRuntimeTokenVerifier>>,
    /// Cookie names in use (the session cookie is read by this name only).
    pub cookie_names: CookieNames,
}

/// Authentication middleware that verifies JWT tokens and injects `CurrentUser`.
///
/// Flow:
/// 1. Extract bearer token from `Authorization` header or `aionui-session` cookie
/// 2. Verify JWT signature, expiration, and blacklist
/// 3. Look up user in the database to ensure they still exist
/// 4. Insert [`CurrentUser`] into request extensions
///
/// Returns HTTP 401 for authentication failures.
///
/// Use with `axum::middleware::from_fn_with_state`.
pub async fn auth_middleware(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // In local mode, skip JWT verification and inject a fixed default user.
    if state.identity_mode == AuthIdentityMode::Local {
        request.extensions_mut().insert(CurrentUser::local_default());
        request.extensions_mut().insert(AuthChannel::Local);
        return Ok(next.run(request).await);
    }

    let Some(token) = extract_token_named(request.headers(), state.cookie_names.session) else {
        // No JWT/cookie: fall back to the conversation-helper runtime-token
        // channel used by agent subprocess CLIs.
        return runtime_token_channel(&state, request, next).await;
    };

    let user = authenticate_session_token(&state, &token).await?;
    request.extensions_mut().insert(user);
    request.extensions_mut().insert(AuthChannel::Session);

    Ok(next.run(request).await)
}

/// Validate a session JWT exactly like [`auth_middleware`] does for a request:
/// access-token type + signature/expiry/blacklist, the subject must be an
/// active user, the AionPro identity rule, and the token's
/// `session_generation` must match the user's current one (logout-everywhere
/// and password changes bump it). The JWT secret rotation done by a password
/// change makes old tokens fail the signature check.
///
/// Exposed so long-lived connections (e.g. the voice WebSocket, which
/// re-validates every 60 s) reuse the same checks instead of copying them.
/// `state.identity_mode` must not be [`AuthIdentityMode::Local`]: local mode
/// skips token checks in the middleware and has no token to validate here.
pub async fn authenticate_session_token(state: &AuthState, token: &str) -> Result<CurrentUser, ApiError> {
    // Require an *access* token here: a refresh token must never authenticate an
    // ordinary request — it is accepted only at the refresh endpoint.
    let payload = state.jwt_service.verify_access(token).map_err(|e| {
        tracing::debug!("Token verification failed: {e}");
        ApiError::Unauthorized("Invalid or expired token".into())
    })?;

    let user = state
        .user_repo
        .find_active_by_id(&payload.user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "auth middleware user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    if payload.session_generation != user.session_generation {
        return Err(ApiError::Unauthorized("Invalid authentication session".into()));
    }

    Ok(CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    })
}

/// Authenticate a JWT-less request via the conversation-helper runtime token.
///
/// The helper CLI sends the token the backend minted for its conversation
/// runtime plus the (user, conversation) pair the token was bound to. The
/// verifier enforces that binding, so a forged user or conversation header
/// fails closed. On success the token's user is loaded and injected as
/// [`CurrentUser`], making ordinary user-scoped handlers work unchanged.
async fn runtime_token_channel(state: &AuthState, mut request: Request, next: Next) -> Result<Response, ApiError> {
    let headers = request.headers();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(verifier), Some(token), Some(user_id), Some(conversation_id)) = (
        state.runtime_token_verifier.as_ref(),
        header(RUNTIME_TOKEN_HEADER),
        header(RUNTIME_USER_ID_HEADER),
        header(RUNTIME_CONVERSATION_ID_HEADER),
    ) else {
        return Err(ApiError::Unauthorized("Authentication required".into()));
    };

    if !verifier.verify_conversation_helper(&token, &user_id, &conversation_id) {
        return Err(ApiError::Unauthorized("Invalid runtime token".into()));
    }

    // C13: a valid runtime token only reaches the routes the agent-facing
    // helper CLIs call; it never acts with the owner's full scope.
    if !verifier.allows_route(request.method(), request.uri().path()) {
        tracing::warn!(
            method = %request.method(),
            path = request.uri().path(),
            conversation_id = %conversation_id,
            "runtime token refused outside its route scope"
        );
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            RUNTIME_SCOPE_ERROR_CODE,
            "Runtime token is not allowed on this route.",
            None,
        ));
    }

    let user = state
        .user_repo
        .find_active_by_id(&user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "runtime token channel user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    request.extensions_mut().insert(CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    });
    request
        .extensions_mut()
        .insert(AuthChannel::RuntimeToken { conversation_id });

    Ok(next.run(request).await)
}

/// Local-mode authentication middleware that skips JWT verification.
///
/// Injects a fixed `CurrentUser` with id and username `system_default_user`.
/// Used when the server runs as an embedded subprocess inside Electron.
pub async fn local_auth_middleware(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(CurrentUser::local_default());
    request.extensions_mut().insert(AuthChannel::Local);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    async fn echo_user(request: Request<Body>) -> String {
        let user = request.extensions().get::<CurrentUser>().unwrap();
        format!("{}:{}", user.id, user.username)
    }

    #[tokio::test]
    async fn test_local_auth_middleware_injects_default_user() {
        let app = Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn(local_auth_middleware));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "system_default_user:system_default_user"
        );
    }

    async fn echo_channel(request: Request<Body>) -> String {
        format!("{:?}", request.extensions().get::<AuthChannel>())
    }

    #[tokio::test]
    async fn local_auth_middleware_marks_the_local_channel() {
        let app = Router::new()
            .route("/test", get(echo_channel))
            .route_layer(axum::middleware::from_fn(local_auth_middleware));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "Some(Local)");
    }
}
