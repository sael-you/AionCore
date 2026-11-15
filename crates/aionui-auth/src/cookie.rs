use aionui_common::constants::{
    COOKIE_MAX_AGE_DAYS, COOKIE_NAME, CSRF_COOKIE_NAME, REFRESH_COOKIE_MAX_AGE_DAYS, REFRESH_COOKIE_NAME,
    REFRESH_COOKIE_PATH,
};

/// The cookie names in use.
///
/// Webui mode over HTTPS uses prefixed names (card C11, change 6):
/// `__Host-` cookies must be `Secure`, `Path=/` and carry no `Domain`, so a
/// dev server on a sibling sub-domain cannot plant ("toss") a session or CSRF
/// cookie the backend would read; the path-scoped refresh cookie can only be
/// `__Secure-`. Un-prefixed names are then never read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CookieNames {
    pub session: &'static str,
    pub csrf: &'static str,
    pub refresh: &'static str,
}

impl CookieNames {
    /// The historical names (local mode, plain-HTTP webui, AionPro).
    pub const LEGACY: CookieNames = CookieNames {
        session: COOKIE_NAME,
        csrf: CSRF_COOKIE_NAME,
        refresh: REFRESH_COOKIE_NAME,
    };
    /// Webui over HTTPS.
    pub const PREFIXED: CookieNames = CookieNames {
        session: "__Host-aionui-session",
        csrf: "__Host-aionui-csrf-token",
        refresh: "__Secure-aionui-refresh",
    };
}

/// Cookie security configuration derived from the deployment environment.
#[derive(Debug, Clone)]
pub struct CookieConfig {
    /// Whether to set the `Secure` flag on cookies (HTTPS only).
    pub secure: bool,
    /// `SameSite` policy: `"Strict"` for HTTPS, `"Lax"` for HTTP.
    pub same_site: &'static str,
    /// Names written and read.
    pub names: CookieNames,
}

impl CookieConfig {
    /// Cookie config for an identity mode: `https` sets `Secure` and
    /// `SameSite=Strict`; webui over HTTPS also switches to the prefixed
    /// names (the prefixes require `Secure`).
    pub fn for_identity(webui: bool, https: bool) -> Self {
        Self {
            secure: https,
            same_site: if https { "Strict" } else { "Lax" },
            names: if webui && https {
                CookieNames::PREFIXED
            } else {
                CookieNames::LEGACY
            },
        }
    }

    /// Whether `AIONUI_HTTPS=true` is set.
    pub fn https_from_env() -> bool {
        std::env::var("AIONUI_HTTPS")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    /// Create cookie config from environment variables.
    ///
    /// - `AIONUI_HTTPS=true` → Secure flag, SameSite=Strict
    /// - Otherwise → no Secure flag, SameSite=Lax (for remote HTTP access)
    pub fn from_env() -> Self {
        Self::for_identity(false, Self::https_from_env())
    }

    /// Build `Set-Cookie` header value for the session token.
    ///
    /// Attributes: HttpOnly, SameSite, Secure (if HTTPS), Max-Age=30d.
    pub fn build_session_cookie(&self, token: &str) -> String {
        let max_age = u64::from(COOKIE_MAX_AGE_DAYS) * 24 * 60 * 60;
        format!(
            "{}={token}; Path=/; HttpOnly; SameSite={}{}; Max-Age={max_age}",
            self.names.session,
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Build `Set-Cookie` header value that clears the session cookie.
    pub fn clear_session_cookie(&self) -> String {
        format!(
            "{}=; Path=/; HttpOnly; SameSite={}{}; Max-Age=0",
            self.names.session,
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Build `Set-Cookie` header value for the CSRF token.
    ///
    /// NOT HttpOnly — JavaScript must read this value to include it
    /// in the `x-csrf-token` request header (Double Submit Cookie pattern).
    pub fn build_csrf_cookie(&self, token: &str) -> String {
        let max_age = u64::from(COOKIE_MAX_AGE_DAYS) * 24 * 60 * 60;
        format!(
            "{}={token}; Path=/; SameSite={}{}; Max-Age={max_age}",
            self.names.csrf,
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Build `Set-Cookie` header value for the refresh token.
    ///
    /// Scoped to [`REFRESH_COOKIE_PATH`] so the browser only attaches it when
    /// calling the refresh endpoint — keeping this long-lived credential off
    /// every ordinary API/WebSocket request. Always `HttpOnly`: unlike the CSRF
    /// cookie, JavaScript must never read the refresh token.
    pub fn build_refresh_cookie(&self, token: &str) -> String {
        let max_age = u64::from(REFRESH_COOKIE_MAX_AGE_DAYS) * 24 * 60 * 60;
        format!(
            "{}={token}; Path={REFRESH_COOKIE_PATH}; HttpOnly; SameSite={}{}; Max-Age={max_age}",
            self.names.refresh,
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Build `Set-Cookie` header value that clears the refresh cookie.
    ///
    /// Must repeat the exact `Path` of [`CookieConfig::build_refresh_cookie`]:
    /// browsers match deletion on name + path, so a mismatched path would leave
    /// the refresh cookie in place.
    pub fn clear_refresh_cookie(&self) -> String {
        format!(
            "{}=; Path={REFRESH_COOKIE_PATH}; HttpOnly; SameSite={}{}; Max-Age=0",
            self.names.refresh,
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_config() -> CookieConfig {
        CookieConfig {
            secure: false,
            same_site: "Lax",
            names: CookieNames::LEGACY,
        }
    }

    fn https_config() -> CookieConfig {
        CookieConfig {
            secure: true,
            same_site: "Strict",
            names: CookieNames::LEGACY,
        }
    }

    #[test]
    fn session_cookie_http() {
        let cookie = http_config().build_session_cookie("my_token");
        assert!(cookie.contains("aionui-session=my_token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age="));
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn session_cookie_https() {
        let cookie = https_config().build_session_cookie("my_token");
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("; Secure"));
    }

    #[test]
    fn clear_session_cookie_sets_max_age_zero() {
        let cookie = http_config().clear_session_cookie();
        assert!(cookie.contains("aionui-session="));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
    }

    #[test]
    fn csrf_cookie_not_http_only() {
        let cookie = http_config().build_csrf_cookie("csrf_abc");
        assert!(cookie.contains("aionui-csrf-token=csrf_abc"));
        assert!(!cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age="));
    }

    #[test]
    fn csrf_cookie_https_has_secure() {
        let cookie = https_config().build_csrf_cookie("csrf_abc");
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("SameSite=Strict"));
    }

    #[test]
    fn session_cookie_max_age_30_days() {
        let cookie = http_config().build_session_cookie("t");
        let expected = 30 * 24 * 60 * 60;
        assert!(cookie.contains(&format!("Max-Age={expected}")));
    }

    #[test]
    fn refresh_cookie_http() {
        let cookie = http_config().build_refresh_cookie("refresh_token");
        assert!(cookie.contains("aionui-refresh=refresh_token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age="));
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn refresh_cookie_https() {
        let cookie = https_config().build_refresh_cookie("refresh_token");
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("; Secure"));
    }

    #[test]
    fn refresh_cookie_is_scoped_to_refresh_path() {
        // The long-lived credential must not ride along on every request: it is
        // pinned to the refresh endpoint's path, never the root path.
        let cookie = http_config().build_refresh_cookie("t");
        assert!(cookie.contains("Path=/api/auth/refresh"));
        assert!(!cookie.contains("Path=/;"));
    }

    #[test]
    fn refresh_cookie_max_age_30_days() {
        let cookie = http_config().build_refresh_cookie("t");
        let expected = 30 * 24 * 60 * 60;
        assert!(cookie.contains(&format!("Max-Age={expected}")));
    }

    #[test]
    fn clear_refresh_cookie_sets_max_age_zero_on_same_path() {
        let cookie = http_config().clear_refresh_cookie();
        assert!(cookie.contains("aionui-refresh="));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
        // Deletion only takes effect when the path matches the set cookie.
        assert!(cookie.contains("Path=/api/auth/refresh"));
    }

    #[test]
    fn prefixed_names_are_secure_host_cookies_without_domain() {
        let config = CookieConfig {
            secure: true,
            same_site: "Strict",
            names: CookieNames::PREFIXED,
        };
        let session = config.build_session_cookie("t");
        assert!(session.starts_with("__Host-aionui-session=t; Path=/;"));
        assert!(session.contains("; Secure") && !session.contains("Domain"));
        let csrf = config.build_csrf_cookie("c");
        assert!(csrf.starts_with("__Host-aionui-csrf-token=c; Path=/;"));
        assert!(csrf.contains("; Secure") && !csrf.contains("Domain"));
        let refresh = config.build_refresh_cookie("r");
        assert!(refresh.starts_with("__Secure-aionui-refresh=r; Path=/api/auth/refresh;"));
        assert!(refresh.contains("; Secure"));
        assert!(config.clear_session_cookie().starts_with("__Host-aionui-session=;"));
        assert!(config.clear_refresh_cookie().starts_with("__Secure-aionui-refresh=;"));
    }
}
