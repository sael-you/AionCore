//! HTTP guards of the voice user router (§3.5): credential precedence,
//! Origin allowlist and the cookie session check. No CSRF layer of its own:
//! in local identity the strict Origin check is the cross-site defence (a
//! cross-site browser write or upgrade always carries `Origin`); in webui
//! identity the app's global C11 CSRF layer also applies.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_auth::{
    AuthState, OriginPolicy, RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_TOKEN_HEADER, RUNTIME_USER_ID_HEADER,
    auth_middleware, authenticate_session_token,
};
use aionui_common::ApiError;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::Response;

use crate::ports::{AuthRejected, AuthVerifier, VerifiedUser};

/// Headers that would let something other than the session cookie
/// authenticate (the shared extractor prefers `Authorization`; CSRF exempts
/// runtime-token requests). Voice browser routes refuse them all.
const FORBIDDEN_CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    RUNTIME_TOKEN_HEADER,
    RUNTIME_USER_ID_HEADER,
    RUNTIME_CONVERSATION_ID_HEADER,
    "sec-websocket-protocol",
];

/// How the voice user router is protected; built by the composition layer.
#[derive(Clone)]
pub struct VoiceHttpGuards {
    /// Must use `UserSession` (or `AionPro`) identity — never `Local`, so the
    /// local default user is never accepted (§3.5 identity row).
    pub auth_state: AuthState,
    /// Trusted browser origins (the app's `OriginPolicy` plus the voice
    /// allow-list), never derived from `Host`.
    pub origins: Arc<OriginPolicy>,
}

/// [`AuthVerifier`] over `aionui_auth::authenticate_session_token`: the very
/// checks `auth_middleware` runs, reused for socket/call re-validation.
pub struct SessionTokenVerifier {
    auth_state: AuthState,
}

impl SessionTokenVerifier {
    pub fn new(auth_state: AuthState) -> Self {
        Self { auth_state }
    }
}

#[async_trait::async_trait]
impl AuthVerifier for SessionTokenVerifier {
    async fn verify(&self, token: &str) -> Result<VerifiedUser, AuthRejected> {
        match authenticate_session_token(&self.auth_state, token).await {
            Ok(user) => Ok(VerifiedUser { id: user.id }),
            Err(ApiError::Internal(_)) => Err(AuthRejected::Unavailable),
            Err(_) => Err(AuthRejected::Rejected),
        }
    }
}

fn reject(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError::coded(status, code, message, None)
}

/// 400 when any non-cookie credential is present.
pub async fn credential_precedence_guard(request: Request, next: Next) -> Result<Response, ApiError> {
    let headers = request.headers();
    if let Some(name) = FORBIDDEN_CREDENTIAL_HEADERS
        .iter()
        .find(|name| headers.contains_key(**name))
    {
        tracing::warn!(header = *name, path = %request.uri().path(), "voice request refused: non-cookie credential");
        return Err(reject(
            StatusCode::BAD_REQUEST,
            "VOICE_CREDENTIAL_CONFLICT",
            "Voice requests authenticate with the session cookie only.",
        ));
    }
    Ok(next.run(request).await)
}

/// 403 for a non-allowlisted `Origin`; unsafe methods and WebSocket upgrades
/// must carry one.
pub async fn origin_guard(
    State(origins): State<Arc<OriginPolicy>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let origin = match request.headers().get(header::ORIGIN) {
        None => None,
        Some(value) => match value.to_str() {
            Ok(text) => Some(text),
            Err(_) => {
                // Present but not visible ASCII: never treat it as "missing".
                tracing::warn!(path = %request.uri().path(), "voice request refused: unparseable origin");
                return Err(reject(
                    StatusCode::FORBIDDEN,
                    "VOICE_ORIGIN_DENIED",
                    "Origin not allowed.",
                ));
            }
        },
    };
    match origin {
        Some(origin) if origins.is_trusted(origin) => {}
        Some(_) => {
            tracing::warn!(path = %request.uri().path(), "voice request refused: origin not allowed");
            return Err(reject(
                StatusCode::FORBIDDEN,
                "VOICE_ORIGIN_DENIED",
                "Origin not allowed.",
            ));
        }
        None => {
            let is_upgrade = request.headers().contains_key(header::UPGRADE);
            let safe = matches!(*request.method(), Method::GET | Method::HEAD);
            if is_upgrade || !safe {
                tracing::warn!(path = %request.uri().path(), "voice request refused: missing origin");
                return Err(reject(
                    StatusCode::FORBIDDEN,
                    "VOICE_ORIGIN_REQUIRED",
                    "Origin header required.",
                ));
            }
        }
    }
    Ok(next.run(request).await)
}

/// Wrap the voice routes with the guards. Outermost first: precedence →
/// Origin → session cookie → handler.
pub fn guard_voice_router<S>(routes: Router<S>, guards: VoiceHttpGuards) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    routes
        .route_layer(from_fn_with_state(guards.auth_state, auth_middleware))
        .route_layer(from_fn_with_state(guards.origins, origin_guard))
        .route_layer(from_fn(credential_precedence_guard))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_trust_is_exact_and_normalised() {
        let policy = OriginPolicy::new(Some("https://y.example.com/"), &["http://localhost:5173"]);
        assert!(policy.is_trusted("https://y.example.com"));
        assert!(policy.is_trusted("HTTPS://Y.EXAMPLE.COM"));
        assert!(policy.is_trusted("http://localhost:5173"));
        assert!(!policy.is_trusted("https://y.example.com.evil.test"));
        assert!(!policy.is_trusted("http://y.example.com"));
        assert!(!policy.is_trusted("null"));
    }
}
