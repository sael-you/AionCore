//! The Speech-to-Text OpenAI key (STT key hotfix, 2026-11-17): the STT
//! settings may leave their own key empty; then the user's OpenAI key from
//! Settings → Model is used, resolved exactly like voice resolves it
//! (`aionui-voice` `CredentialPolicy::resolve_openai`, wired in `aionui-app`).

use aionui_api_types::{SpeechToTextConfig, SpeechToTextProvider};

/// The user's OpenAI key from Settings → Model.
#[derive(Clone)]
pub struct SttFallbackKey {
    pub api_key: String,
    pub base_url: String,
}

impl std::fmt::Debug for SttFallbackKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttFallbackKey")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[async_trait::async_trait]
pub trait SttKeyFallback: Send + Sync {
    async fn openai_key(&self, user_id: &str) -> Option<SttFallbackKey>;
}

/// An OpenAI STT config with an empty key gets the Model-page key (and its
/// base URL when the STT settings name none). Anything else is unchanged.
pub async fn fill_openai_key(
    mut config: SpeechToTextConfig,
    fallback: Option<&dyn SttKeyFallback>,
    user_id: &str,
) -> SpeechToTextConfig {
    let (Some(fallback), SpeechToTextProvider::Openai) = (fallback, &config.provider) else {
        return config;
    };
    let Some(openai) = config.openai.as_mut().filter(|c| c.api_key.trim().is_empty()) else {
        return config;
    };
    if let Some(key) = fallback.openai_key(user_id).await {
        openai.api_key = key.api_key;
        if openai.base_url.as_deref().is_none_or(|url| url.trim().is_empty()) {
            openai.base_url = Some(key.base_url);
        }
        tracing::info!("stt: using the OpenAI key from the model settings");
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::OpenAISpeechToTextConfig;

    struct Fixed(Option<&'static str>);

    #[async_trait::async_trait]
    impl SttKeyFallback for Fixed {
        async fn openai_key(&self, _user_id: &str) -> Option<SttFallbackKey> {
            self.0.map(|key| SttFallbackKey {
                api_key: key.to_owned(),
                base_url: "https://api.openai.com/v1".into(),
            })
        }
    }

    fn config(key: &str, base_url: Option<&str>) -> SpeechToTextConfig {
        SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Openai,
            auto_send: None,
            openai: Some(OpenAISpeechToTextConfig {
                api_key: key.into(),
                base_url: base_url.map(str::to_owned),
                model: "gpt-4o-transcribe".into(),
                language: None,
                prompt: None,
                temperature: None,
            }),
            deepgram: None,
        }
    }

    #[tokio::test]
    async fn an_empty_stt_key_uses_the_model_page_key() {
        let filled = fill_openai_key(config("", None), Some(&Fixed(Some("sk-model-page"))), "u").await;
        let openai = filled.openai.unwrap();
        assert_eq!(openai.api_key, "sk-model-page");
        assert_eq!(openai.base_url.as_deref(), Some("https://api.openai.com/v1"));
    }

    #[tokio::test]
    async fn an_own_key_or_no_model_key_changes_nothing() {
        let own = fill_openai_key(config("sk-own", None), Some(&Fixed(Some("sk-model-page"))), "u").await;
        assert_eq!(own.openai.unwrap().api_key, "sk-own");
        let none = fill_openai_key(config("", None), Some(&Fixed(None)), "u").await;
        assert_eq!(none.openai.unwrap().api_key, "", "still not configured");
        let custom = fill_openai_key(
            config(" ", Some("https://proxy.example/v1")),
            Some(&Fixed(Some("sk-model-page"))),
            "u",
        )
        .await;
        assert_eq!(
            custom.openai.unwrap().base_url.as_deref(),
            Some("https://proxy.example/v1"),
            "the STT base URL stays when set"
        );
    }
}
