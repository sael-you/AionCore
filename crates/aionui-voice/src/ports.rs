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

/// The owner's messaging channel (Telegram, Lark, … from `aionui-channel`):
/// an agent's `say` / `ask` goes there as text when no voice device is
/// connected (§3.4 "No armed device → Channels message if configured").
#[async_trait::async_trait]
pub trait ChannelPort: Send + Sync {
    /// Send `text` to the user's paired chat. `Ok(false)` when no channel is
    /// configured and running for this user.
    async fn send_text(&self, user_id: &str, text: &str) -> Result<bool, String>;
}

/// No messaging channel: `say` / `ask` keep returning `no_device`.
pub struct NoChannels;

#[async_trait::async_trait]
impl ChannelPort for NoChannels {
    async fn send_text(&self, _user_id: &str, _text: &str) -> Result<bool, String> {
        Ok(false)
    }
}

/// The user's skill library (D47): the concierge instructions are compiled
/// from the skill `cs-voice-concierge` when it exists (§3.7).
#[async_trait::async_trait]
pub trait SkillPort: Send + Sync {
    /// Full `SKILL.md` text of the user's skill `name`, if it exists.
    async fn skill_markdown(&self, user_id: &str, name: &str) -> Option<String>;
}

/// No skill library: Jarvis keeps the built-in instructions.
pub struct NoSkills;

#[async_trait::async_trait]
impl SkillPort for NoSkills {
    async fn skill_markdown(&self, _user_id: &str, _name: &str) -> Option<String> {
        None
    }
}
