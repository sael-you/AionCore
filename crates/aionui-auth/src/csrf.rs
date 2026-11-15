#![allow(clippy::disallowed_types)]

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;
use aionui_common::constants::{COOKIE_NAME, CSRF_HEADER_NAME};
use axum::http::StatusCode;

use crate::cookie::{CookieConfig, CookieNames};
use crate::extract::{extract_bearer_token, extract_cookie_value, extract_cookie_values};
use crate::origin::{OriginPolicy, is_unsafe_method};

/// State of [`csrf_middleware`] (installed in non-local modes only).
#[derive(Clone)]
pub struct CsrfState {
    pub cookie_config: Arc<CookieConfig>,
    /// `Some` = webui hardening (card C11: browser-origin rules, strict
    /// runtime-token exemption); `None` = upstream behaviour (AionPro).
    pub origin_policy: Option<Arc<OriginPolicy>>,
}

/// CSRF protection middleware using the Double Submit Cookie pattern.
///
/// Behavior:
/// - Safe methods (GET, HEAD, OPTIONS) bypass validation.
/// - Exempt paths (`/login`, `/api/auth/qr-login`, Bootstrap-secret internal auth) bypass validation.
/// - Webui: `Authorization: Bearer` requests without a session cookie bypass
///   validation (mobile app); the token is still validated behind this layer.
/// - All other requests must include an `x-csrf-token` header whose value
///   matches the CSRF cookie.
/// - Every state-changing request must pass the browser-origin rule
///   ([`OriginPolicy::allows_unsafe_request`], card C11 change 3b), exempt
///   paths included.
/// - Sets the CSRF cookie on responses if the client does not have one.
pub async fn csrf_middleware(
    State(state): State<CsrfState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let cookie_config = &state.cookie_config;
    let method = request.method().clone();
    let path = request.uri().path().to_owned();

    // Extract CSRF cookie before consuming the request
    let csrf_cookie = extract_cookie_value(request.headers(), cookie_config.names.csrf);

    // Validate CSRF for state-changing requests
    let needs_validation = is_unsafe_method(&method);
    if let Some(policy) = &state.origin_policy {
        let allowed = if needs_validation {
            policy.allows_unsafe_request(request.headers())
        } else {
            !path.starts_with("/api/") || policy.allows_safe_api_request(request.headers())
        };
        if !allowed {
            tracing::warn!(path = %path, method = %method, "request refused: untrusted browser origin");
            return Err(ApiError::coded(
                StatusCode::FORBIDDEN,
                "ORIGIN_FORBIDDEN",
                "Cross-site request refused.",
                None,
            ));
        }
    }
    // Requests on the runtime-token channel are exempt: CSRF defends against
    // cookie-bearing cross-site requests, while the helper CLI authenticates
    // via an explicit custom header (never ambient cookies) that browsers
    // cannot attach cross-site without a CORS preflight. The token itself is
    // still fully validated by the auth middleware behind this layer.
    //
    // Only a request that carries no ambient credential qualifies (C11 change
    // 3a): with a session cookie or an Authorization header present, a junk
    // runtime header must not switch the CSRF check off.
    let headers = request.headers();
    let has_session_credential = headers.contains_key(header::AUTHORIZATION)
        || extract_cookie_value(headers, cookie_config.names.session).is_some()
        || extract_cookie_value(headers, COOKIE_NAME).is_some();
    let is_runtime_token_request = headers.contains_key(crate::middleware::RUNTIME_TOKEN_HEADER)
        && (state.origin_policy.is_none() || !has_session_credential);
    // Bearer clients (the mobile app) in webui identity: a request carrying
    // `Authorization: Bearer <token>` and no session cookie under either name
    // is exempt. A browser never attaches an Authorization header on its own
    // and cannot add one cross-site without a CORS preflight, so there is no
    // ambient credential for CSRF to defend. The token is still validated
    // behind this layer (auth middleware reads the Bearer first; the refresh
    // route verifies its own token). A session cookie under either name
    // (`__Host-aionui-session`, `aionui-session`) removes the exemption, so a
    // browser session cannot use it. A runtime-token header next to the
    // Bearer changes nothing: the auth middleware authenticates the Bearer
    // and never falls back to the runtime channel. Webui only: AionPro keeps
    // the upstream behaviour, local identity has no CSRF layer at all.
    // Every `Cookie` header counts (HTTP/2 clients may send one per cookie),
    // not only the first.
    let has_any_session_cookie = [CookieNames::PREFIXED.session, CookieNames::LEGACY.session]
        .into_iter()
        .any(|name| !extract_cookie_values(headers, name).is_empty());
    let is_bearer_request =
        state.origin_policy.is_some() && extract_bearer_token(headers).is_some() && !has_any_session_cookie;
    // agy's PreToolUse callback. A local `aioncore antigravity-hook` process
    // posts here for every tool the agent wants to run; it has no user session
    // and no CSRF token to present. The route is already deliberately outside
    // `auth_middleware` for that reason, and the handler authenticates it with a
    // per-conversation token instead (`x-aionui-hook-token`) — but the CSRF
    // layer is global, so it rejected what auth had been told to let through.
    //
    // Exempting it is safe on CSRF's own terms: this layer stops a browser
    // attaching AMBIENT COOKIES to a cross-site request, and the endpoint reads
    // no cookie. A forged call still has to carry a token minted per
    // conversation and held in memory.
    //
    // Without this, every agy tool call is answered 403 CSRF_INVALID and the
    // hook reads that as "no answer" and denies (measured 2026-08-14: 9/9 then
    // 12/12 calls rejected across two live runs).
    let is_antigravity_hook = path.starts_with("/internal/antigravity-hook/");
    let is_exempt = path == "/login"
        || path == "/api/auth/qr-login"
        || path.starts_with("/api/auth/internal/external-users/")
        || path.starts_with("/api/auth/internal/external-sessions")
        || is_antigravity_hook
        || is_runtime_token_request
        || is_bearer_request;

    if needs_validation && !is_exempt {
        let header_token = request
            .headers()
            .get(CSRF_HEADER_NAME)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());

        match (&csrf_cookie, header_token) {
            (Some(cookie), Some(ref hdr))
                if !cookie.is_empty() && constant_time_eq(cookie.as_bytes(), hdr.as_bytes()) =>
            {
                // Valid: cookie and header match
            }
            _ => {
                return Err(ApiError::CsrfInvalid("CSRF token validation failed".into()));
            }
        }
    }

    let mut response = next.run(request).await;

    // Set CSRF cookie if the client doesn't have one
    if csrf_cookie.is_none() {
        let token = generate_csrf_token();
        let cookie_str = cookie_config.build_csrf_cookie(&token);
        if let Ok(value) = HeaderValue::from_str(&cookie_str) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }

    Ok(response)
}

/// Compare two byte strings without an early exit on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Generate a cryptographically random 32-byte CSRF token as a hex string.
fn generate_csrf_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("OS entropy source unavailable");
    let mut hex = String::with_capacity(64);
    for byte in buf {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_token_is_64_hex_chars() {
        let token = generate_csrf_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn constant_time_compare_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn csrf_tokens_are_unique() {
        let t1 = generate_csrf_token();
        let t2 = generate_csrf_token();
        assert_ne!(t1, t2);
    }
}
