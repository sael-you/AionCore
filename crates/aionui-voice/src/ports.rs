//! Voice-owned ports. `aionui-app` supplies the adapters (composition layer),
//! so this crate never reaches into other domains' concrete services.

use std::fmt;

/// The user a session token belongs to, after the full session check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedUser {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthRejected {
    /// Token invalid, expired, blacklisted, user inactive or session revoked.
    #[error("session rejected")]
    Rejected,
    /// The check itself could not run (e.g. database unavailable).
    #[error("session check unavailable")]
    Unavailable,
}

/// Re-validates the session cookie of live sockets and calls every 60 s
/// (§3.5 identity row): same checks as `auth_middleware`.
#[async_trait::async_trait]
pub trait AuthVerifier: Send + Sync {
    async fn verify(&self, token: &str) -> Result<VerifiedUser, AuthRejected>;
}

/// A provider API key. `Debug`/`Display` never print it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// A stored provider credential, as handed over by the backend-only adapter.
#[derive(Debug, Clone)]
pub struct ProviderCredential {
    pub provider_id: String,
    pub platform: String,
    pub base_url: String,
    pub enabled: bool,
    pub api_key: ApiKey,
}

/// Backend-only access to the user's provider credentials. Implementations
/// must never go through the `/api/providers` response DTO.
#[async_trait::async_trait]
pub trait CredentialPort: Send + Sync {
    async fn provider_credentials(&self, user_id: &str) -> Result<Vec<ProviderCredential>, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_debug_is_redacted() {
        let key = ApiKey::new("sk-live-secret");
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        let cred = ProviderCredential {
            provider_id: "p".into(),
            platform: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            enabled: true,
            api_key: key,
        };
        assert!(!format!("{cred:?}").contains("sk-live"));
    }
}
