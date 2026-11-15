#![allow(clippy::disallowed_types)]

//! Browser-origin checks for non-local modes (card C11, changes 2/3b/4b).
//!
//! Browsers mark every request with `Sec-Fetch-Site` and (for unsafe methods
//! and WebSocket upgrades) `Origin`. The policy compares `Origin` only with the
//! configured public origin (`AIONUI_PUBLIC_ORIGIN`) and the explicit
//! allow-list (`AIONUI_ALLOWED_ORIGINS`, comma-separated) — never with `Host`,
//! which the web host rewrites. Requests carrying neither header are not
//! browser requests (helper CLIs, the Token Pool client, hooks) and pass; the
//! CSRF double-submit and runtime-token checks still apply to them.
//!
//! Local mode never installs these layers.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;

/// Environment variable: the origin the product is served from
/// (e.g. `https://y.talkseeds.com`).
pub const PUBLIC_ORIGIN_ENV: &str = "AIONUI_PUBLIC_ORIGIN";
/// Environment variable: extra trusted origins (comma-separated); also the
/// CORS allow-list. Empty/unset = no cross-origin browser client.
pub const ALLOWED_ORIGINS_ENV: &str = "AIONUI_ALLOWED_ORIGINS";

/// Trusted browser origins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OriginPolicy {
    /// The product's own origin, normalised.
    pub public_origin: Option<String>,
    /// The CORS allow-list, normalised.
    pub allowed_origins: Vec<String>,
}

/// Lower-case the scheme/host and drop a trailing slash; `None` for blanks.
pub fn normalize_origin(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

impl OriginPolicy {
    pub fn new(public_origin: Option<&str>, allowed_origins: &[&str]) -> Self {
        let mut allowed: Vec<String> = Vec::new();
        for origin in allowed_origins.iter().filter_map(|o| normalize_origin(o)) {
            if !allowed.contains(&origin) {
                allowed.push(origin);
            }
        }
        Self {
            public_origin: public_origin.and_then(normalize_origin),
            allowed_origins: allowed,
        }
    }

    /// Read [`PUBLIC_ORIGIN_ENV`] and [`ALLOWED_ORIGINS_ENV`].
    pub fn from_env() -> Self {
        let public = std::env::var(PUBLIC_ORIGIN_ENV).ok();
        let allowed = std::env::var(ALLOWED_ORIGINS_ENV).unwrap_or_default();
        let list: Vec<&str> = allowed.split(',').collect();
        Self::new(public.as_deref(), &list)
    }

    /// Whether `origin` is the public origin or on the allow-list.
    pub fn is_trusted(&self, origin: &str) -> bool {
        let Some(origin) = normalize_origin(origin) else {
            return false;
        };
        self.public_origin.as_deref() == Some(origin.as_str()) || self.allowed_origins.contains(&origin)
    }

    /// Whether `origin` is on the CORS allow-list (the public origin is
    /// same-origin and needs no CORS).
    pub fn is_cors_allowed(&self, origin: &str) -> bool {
        normalize_origin(origin).is_some_and(|origin| self.allowed_origins.contains(&origin))
    }

    fn origin_trusted(&self, headers: &HeaderMap) -> Option<bool> {
        headers
            .get(header::ORIGIN)
            .map(|value| value.to_str().map(|origin| self.is_trusted(origin)).unwrap_or(false))
    }

    /// Change 3b — an unsafe-method request is allowed when:
    /// - `Sec-Fetch-Site` is `same-origin` or `none`, or
    /// - `Sec-Fetch-Site` is another value and `Origin` is trusted, or
    /// - `Sec-Fetch-Site` is absent and `Origin` is absent (not a browser) or
    ///   trusted.
    pub fn allows_unsafe_request(&self, headers: &HeaderMap) -> bool {
        match fetch_site(headers) {
            Some("same-origin" | "none") => true,
            Some(_) => self.origin_trusted(headers) == Some(true),
            None => self.origin_trusted(headers).unwrap_or(true),
        }
    }

    /// Change 4b — a WebSocket upgrade is refused when `Origin` is present and
    /// not trusted, or when `Sec-Fetch-Site` is `cross-site`/`same-site` and
    /// `Origin` is not trusted.
    pub fn allows_websocket(&self, headers: &HeaderMap) -> bool {
        let origin = self.origin_trusted(headers);
        if origin == Some(false) {
            return false;
        }
        match fetch_site(headers) {
            Some("cross-site" | "same-site") => origin == Some(true),
            _ => true,
        }
    }
}

fn fetch_site(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// Middleware (change 4b) for every WebSocket upgrade (`/ws`,
/// `/api/stt/stream`, `/api/voice/ws`, …): refuse a browser upgrade from an
/// untrusted origin with 403. Non-upgrade requests pass untouched.
pub async fn websocket_origin_middleware(
    State(policy): State<Arc<OriginPolicy>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if is_websocket_upgrade(request.headers()) && !policy.allows_websocket(request.headers()) {
        tracing::warn!(path = %request.uri().path(), "websocket upgrade refused: untrusted origin");
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "ORIGIN_FORBIDDEN",
            "Cross-origin WebSocket refused.",
            None,
        ));
    }
    Ok(next.run(request).await)
}

/// Whether a method changes state (CSRF and change 3b apply).
pub fn is_unsafe_method(method: &Method) -> bool {
    matches!(*method, Method::POST | Method::PUT | Method::DELETE | Method::PATCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn policy() -> OriginPolicy {
        OriginPolicy::new(Some("https://y.talkseeds.com/"), &["https://app.example", " "])
    }

    #[test]
    fn origins_are_normalised_and_deduplicated() {
        let policy = OriginPolicy::new(Some(" HTTPS://Y.talkseeds.com/ "), &["https://a", "https://a/", ""]);
        assert_eq!(policy.public_origin.as_deref(), Some("https://y.talkseeds.com"));
        assert_eq!(policy.allowed_origins, ["https://a"]);
        assert!(policy.is_trusted("https://y.talkseeds.com"));
        assert!(
            !policy.is_cors_allowed("https://y.talkseeds.com"),
            "same-origin needs no CORS"
        );
    }

    #[test]
    fn unsafe_requests_follow_sec_fetch_site_then_origin() {
        let p = policy();
        assert!(p.allows_unsafe_request(&headers(&[])), "not a browser");
        assert!(p.allows_unsafe_request(&headers(&[("sec-fetch-site", "same-origin")])));
        assert!(p.allows_unsafe_request(&headers(&[("sec-fetch-site", "none")])));
        assert!(!p.allows_unsafe_request(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(!p.allows_unsafe_request(&headers(&[
            ("sec-fetch-site", "same-site"),
            ("origin", "https://x.y.talkseeds.com")
        ])));
        assert!(p.allows_unsafe_request(&headers(&[
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://app.example")
        ])));
        assert!(p.allows_unsafe_request(&headers(&[("origin", "https://y.talkseeds.com")])));
        assert!(!p.allows_unsafe_request(&headers(&[("origin", "https://evil.example")])));
        assert!(!p.allows_unsafe_request(&headers(&[("origin", "null")])));
    }

    #[test]
    fn websocket_upgrades_refuse_untrusted_origins() {
        let p = policy();
        assert!(p.allows_websocket(&headers(&[])));
        assert!(p.allows_websocket(&headers(&[
            ("origin", "https://y.talkseeds.com"),
            ("sec-fetch-site", "same-origin")
        ])));
        assert!(!p.allows_websocket(&headers(&[("origin", "https://x.y.talkseeds.com")])));
        assert!(!p.allows_websocket(&headers(&[("sec-fetch-site", "same-site")])));
        assert!(p.allows_websocket(&headers(&[
            ("origin", "https://app.example"),
            ("sec-fetch-site", "cross-site")
        ])));
    }
}
