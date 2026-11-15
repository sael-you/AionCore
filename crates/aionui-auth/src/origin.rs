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

/// Parse one configured origin strictly: `scheme://host[:port]` with scheme
/// `http`/`https`, no path, query, fragment or user info (a single trailing
/// `/` is tolerated). `null` and `*` are refused. Returns the normalised
/// (lower-case) origin.
pub fn parse_origin(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    let value = value.strip_suffix('/').unwrap_or(value);
    if value == "null" || value == "*" {
        return Err(format!(
            "origin `{value}` is not allowed (use an explicit scheme://host[:port])"
        ));
    }
    let Some((scheme, authority)) = value.split_once("://") else {
        return Err(format!("origin `{value}` must look like scheme://host[:port]"));
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!("origin `{value}`: the scheme must be http or https"));
    }
    if authority.is_empty()
        || authority
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '@' | '*' | ',') || c.is_whitespace())
    {
        return Err(format!(
            "origin `{value}` must be scheme://host[:port] without path, query or user info"
        ));
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err(format!("origin `{value}`: unterminated IPv6 host"));
        };
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() || (host.contains(':') && !authority.starts_with('[')) {
        return Err(format!("origin `{value}`: invalid host"));
    }
    if let Some(port) = port
        && !port.parse::<u16>().is_ok_and(|port| port > 0)
    {
        return Err(format!("origin `{value}`: invalid port"));
    }
    Ok(format!("{scheme}://{}", authority.to_ascii_lowercase()))
}

impl OriginPolicy {
    /// Strict construction for startup: every value must pass
    /// [`parse_origin`]; blanks in the comma-separated list are skipped.
    pub fn parse(public_origin: Option<&str>, allowed_origins: &str) -> Result<Self, String> {
        let public_origin = match public_origin.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => Some(parse_origin(value).map_err(|error| format!("{PUBLIC_ORIGIN_ENV}: {error}"))?),
            None => None,
        };
        let mut allowed = Vec::new();
        for raw in allowed_origins.split(',').filter(|value| !value.trim().is_empty()) {
            let origin = parse_origin(raw).map_err(|error| format!("{ALLOWED_ORIGINS_ENV}: {error}"))?;
            if !allowed.contains(&origin) {
                allowed.push(origin);
            }
        }
        Ok(Self {
            public_origin,
            allowed_origins: allowed,
        })
    }

    /// [`parse`](Self::parse) from [`PUBLIC_ORIGIN_ENV`] and
    /// [`ALLOWED_ORIGINS_ENV`].
    pub fn from_env_strict() -> Result<Self, String> {
        let public = std::env::var(PUBLIC_ORIGIN_ENV).ok();
        let allowed = std::env::var(ALLOWED_ORIGINS_ENV).unwrap_or_default();
        Self::parse(public.as_deref(), &allowed)
    }

    /// Whether the public origin is served over HTTPS.
    pub fn public_is_https(&self) -> bool {
        self.public_origin
            .as_deref()
            .is_some_and(|origin| origin.starts_with("https://"))
    }

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
        // Same rule order as writes: a same-origin browser upgrade passes.
        if matches!(fetch_site(headers), Some("same-origin" | "none")) {
            return true;
        }
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

/// Whether a method may change state (CSRF and change 3b apply): every
/// method except GET, HEAD and OPTIONS.
pub fn is_unsafe_method(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

impl OriginPolicy {
    /// Review B-L3 — a cross-site/same-site browser `fetch` of a GET API (not
    /// a top-level navigation) needs a trusted `Origin`: side-effect GETs
    /// (e.g. a channel login kick-off) must not be triggerable from a
    /// sibling dev server. No `Sec-Fetch-Site` = not a browser = allowed.
    pub fn allows_safe_api_request(&self, headers: &HeaderMap) -> bool {
        match fetch_site(headers) {
            Some("cross-site" | "same-site") => {
                let navigation = headers
                    .get("sec-fetch-mode")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|mode| mode.eq_ignore_ascii_case("navigate"));
                navigation || self.origin_trusted(headers) == Some(true)
            }
            _ => true,
        }
    }
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
    fn startup_parsing_is_strict() {
        assert_eq!(
            parse_origin("HTTPS://Y.talkseeds.com/").unwrap(),
            "https://y.talkseeds.com"
        );
        assert_eq!(parse_origin("http://localhost:5173").unwrap(), "http://localhost:5173");
        assert_eq!(parse_origin("http://[::1]:8080").unwrap(), "http://[::1]:8080");
        for bad in [
            "null",
            "*",
            "y.talkseeds.com",
            "ftp://x",
            "https://x/path",
            "https://x?q",
            "https://u@x",
            "https://x:0",
            "https://x:99999",
            "https://",
            "https://*.example",
        ] {
            assert!(parse_origin(bad).is_err(), "{bad}");
        }
        assert!(OriginPolicy::parse(Some("null"), "").is_err());
        assert!(OriginPolicy::parse(None, "https://a, *").is_err());
        let policy = OriginPolicy::parse(Some("https://y.talkseeds.com"), " https://a ,,https://a/").unwrap();
        assert_eq!(policy.allowed_origins, ["https://a"]);
        assert!(policy.public_is_https());
    }

    #[test]
    fn cross_site_api_fetches_need_a_trusted_origin() {
        let p = policy();
        assert!(p.allows_safe_api_request(&headers(&[])), "not a browser");
        assert!(p.allows_safe_api_request(&headers(&[("sec-fetch-site", "same-origin")])));
        assert!(!p.allows_safe_api_request(&headers(&[
            ("sec-fetch-site", "same-site"),
            ("sec-fetch-mode", "no-cors")
        ])));
        assert!(!p.allows_safe_api_request(&headers(&[
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://evil.example")
        ])));
        assert!(p.allows_safe_api_request(&headers(&[
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "navigate")
        ])));
        assert!(p.allows_safe_api_request(&headers(&[
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://app.example")
        ])));
        assert!(is_unsafe_method(&Method::from_bytes(b"PROPFIND").unwrap()));
        assert!(!is_unsafe_method(&Method::OPTIONS));
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
