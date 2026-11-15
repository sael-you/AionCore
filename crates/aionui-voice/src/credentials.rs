//! Voice credential resolver policy (§2 "Provider keys", §4 test seams).
//!
//! Picks the OpenAI key used for Realtime calls from the user's own provider
//! rows: enabled, platform OpenAI, the **official** base URL and a non-empty
//! key. Custom/proxy base URLs are refused so a key is never sent to a
//! third-party host. The fake-provider override exists only in builds with
//! the `voice-test` feature.

use crate::error::VoiceError;
use crate::ports::{ApiKey, ProviderCredential};

/// Official OpenAI API origin (without `/v1`).
pub const OPENAI_OFFICIAL_BASE_URL: &str = "https://api.openai.com";

/// Environment variable honoured only with `--features voice-test`.
pub const FAKE_PROVIDER_URL_ENV: &str = "AIONUI_VOICE_FAKE_PROVIDER_URL";

/// The credential a call is made with. `base_url` has no trailing `/v1`.
#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub provider_id: String,
    pub base_url: String,
    pub api_key: ApiKey,
}

#[derive(Debug, Clone, Default)]
pub struct CredentialPolicy {
    fake_provider_url: Option<String>,
}

impl CredentialPolicy {
    /// Production policy: official OpenAI URL only.
    pub fn official_only() -> Self {
        Self::default()
    }

    /// Reads [`FAKE_PROVIDER_URL_ENV`] in `voice-test` builds; official-only otherwise.
    pub fn from_env() -> Self {
        #[cfg(feature = "voice-test")]
        {
            if let Ok(url) = std::env::var(FAKE_PROVIDER_URL_ENV)
                && !url.trim().is_empty()
            {
                tracing::warn!("voice: fake provider URL active (voice-test build)");
                return Self::with_fake_provider_url(url);
            }
        }
        Self::official_only()
    }

    /// Route every call to a fake provider (tests only).
    #[cfg(feature = "voice-test")]
    pub fn with_fake_provider_url(url: impl Into<String>) -> Self {
        Self {
            fake_provider_url: Some(url.into().trim_end_matches('/').to_owned()),
        }
    }

    /// Select the credential for an OpenAI Realtime call.
    ///
    /// `preferred` pins a provider row id (voice setting); without it the
    /// first eligible row wins.
    pub fn resolve_openai(
        &self,
        credentials: Vec<ProviderCredential>,
        preferred: Option<&str>,
    ) -> Result<ResolvedCredential, VoiceError> {
        let eligible = credentials.into_iter().filter(|cred| {
            cred.enabled
                && cred.platform.trim().eq_ignore_ascii_case("openai")
                && !cred.api_key.is_empty()
                && (self.fake_provider_url.is_some() || is_official_openai_url(&cred.base_url))
        });
        let chosen = match preferred {
            Some(id) => eligible.into_iter().find(|cred| cred.provider_id == id),
            None => eligible.into_iter().next(),
        }
        .ok_or(VoiceError::NoProvider)?;

        let base_url = match &self.fake_provider_url {
            Some(fake) => fake.clone(),
            None => OPENAI_OFFICIAL_BASE_URL.to_owned(),
        };
        Ok(ResolvedCredential {
            provider_id: chosen.provider_id,
            base_url,
            api_key: chosen.api_key,
        })
    }
}

/// True for `https://api.openai.com`, optionally followed by `/` or `/v1`.
fn is_official_openai_url(raw: &str) -> bool {
    let normalized = aionui_shell::resolve_openai_base_url(Some(raw));
    let Ok(url) = url::Url::parse(normalized) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("api.openai.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path(), "" | "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(id: &str, platform: &str, base_url: &str, enabled: bool, key: &str) -> ProviderCredential {
        ProviderCredential {
            provider_id: id.into(),
            platform: platform.into(),
            base_url: base_url.into(),
            enabled,
            api_key: ApiKey::new(key),
        }
    }

    #[test]
    fn official_urls_are_recognised() {
        for url in [
            "https://api.openai.com",
            "https://api.openai.com/",
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            // Blank resolves to the official default (shared STT behaviour);
            // safe because voice only ever calls OPENAI_OFFICIAL_BASE_URL.
            "",
        ] {
            assert!(is_official_openai_url(url), "{url}");
        }
        for url in [
            "http://api.openai.com/v1",
            "https://api.openai.com.evil.example/v1",
            "https://evil.example/api.openai.com",
            "https://api.openai.com:8443/v1",
            "https://user:pw@api.openai.com/v1",
            "https://api.novita.ai/openai/v1",
            "https://api.openai.com/v1/proxy",
        ] {
            assert!(!is_official_openai_url(url), "{url}");
        }
    }

    #[test]
    fn picks_first_eligible_openai_row() {
        let policy = CredentialPolicy::official_only();
        let creds = vec![
            cred("disabled", "OpenAI", "https://api.openai.com/v1", false, "k1"),
            cred("proxy", "OpenAI", "https://proxy.example/v1", true, "k2"),
            cred("anthropic", "anthropic", "https://api.anthropic.com", true, "k3"),
            cred("nokey", "openai", "https://api.openai.com/v1", true, "  "),
            cred("good", "OpenAI", "https://api.openai.com/v1", true, "k5"),
        ];
        let resolved = policy.resolve_openai(creds, None).unwrap();
        assert_eq!(resolved.provider_id, "good");
        assert_eq!(resolved.base_url, OPENAI_OFFICIAL_BASE_URL);
        assert_eq!(resolved.api_key.expose(), "k5");
    }

    #[test]
    fn preferred_provider_must_itself_be_eligible() {
        let policy = CredentialPolicy::official_only();
        let creds = vec![
            cred("a", "OpenAI", "https://api.openai.com/v1", true, "ka"),
            cred("proxy", "OpenAI", "https://proxy.example/v1", true, "kp"),
        ];
        assert_eq!(
            policy.resolve_openai(creds.clone(), Some("a")).unwrap().provider_id,
            "a"
        );
        assert!(matches!(
            policy.resolve_openai(creds, Some("proxy")),
            Err(VoiceError::NoProvider)
        ));
    }

    #[test]
    fn no_eligible_row_is_no_provider() {
        let policy = CredentialPolicy::official_only();
        assert!(matches!(
            policy.resolve_openai(vec![], None),
            Err(VoiceError::NoProvider)
        ));
    }

    #[cfg(feature = "voice-test")]
    #[test]
    fn fake_url_redirects_only_in_test_builds() {
        let policy = CredentialPolicy::with_fake_provider_url("http://127.0.0.1:9/");
        let creds = vec![cred("a", "OpenAI", "https://proxy.example/v1", true, "ka")];
        let resolved = policy.resolve_openai(creds, None).unwrap();
        assert_eq!(resolved.base_url, "http://127.0.0.1:9");
    }
}
