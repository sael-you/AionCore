//! Voice Concierge composition (card C9): adapters for the voice-owned ports
//! and the guarded user router. Everything voice-specific lives in
//! `aionui-voice`; this file only wires it to the app's services.

use std::sync::Arc;

use aionui_auth::{AuthIdentityMode, AuthState, OriginPolicy};
use aionui_db::{SqliteProviderRepository, SqliteSidebarStore, SqliteVoiceRepository};
use aionui_realtime::WebSocketManager;
use aionui_system::ProviderService;
use aionui_voice::{
    ApiKey, CredentialPolicy, CredentialPort, OpenAiRealtimeEngine, ProviderCredential, ProviderSpeechFactory,
    SessionTokenVerifier, VoiceHttpGuards, VoiceRouterState, VoiceService, VoiceServiceDeps, VoiceSockets,
    VoiceTimings,
};

use crate::config::{IdentityMode, derive_encryption_key};
use crate::services::AppServices;

/// Extra comma-separated browser origins allowed on `/api/voice/*` (PLAN §3.5
/// `voice.allowed_origins`), on top of the app's `OriginPolicy`
/// (`AIONUI_PUBLIC_ORIGIN` + `AIONUI_ALLOWED_ORIGINS`, card C11).
pub const VOICE_ALLOWED_ORIGINS_ENV: &str = "AIONUI_VOICE_ALLOWED_ORIGINS";

/// Backend-only credential adapter: decrypted provider rows straight from
/// `ProviderService::backend_credentials` — never the `/api/providers` DTO.
struct ProviderCredentialAdapter {
    providers: ProviderService,
}

#[async_trait::async_trait]
impl CredentialPort for ProviderCredentialAdapter {
    async fn provider_credentials(&self, user_id: &str) -> Result<Vec<ProviderCredential>, String> {
        let rows = self
            .providers
            .backend_credentials(user_id)
            .await
            .map_err(|e| format!("provider lookup failed: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|row| ProviderCredential {
                api_key: ApiKey::new(row.api_key()),
                provider_id: row.provider_id,
                platform: row.platform,
                base_url: row.base_url,
                enabled: row.enabled,
            })
            .collect())
    }
}

/// Identity for voice routes: a validated session cookie in every mode. In
/// `--local` the rest of the app injects the default user; voice does not.
fn voice_auth_state(services: &AppServices) -> AuthState {
    AuthState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
        identity_mode: if services.identity_mode == IdentityMode::AionPro {
            AuthIdentityMode::AionPro
        } else {
            AuthIdentityMode::UserSession
        },
        runtime_token_verifier: None,
        cookie_names: services.cookie_config.names,
    }
}

pub fn build_voice_state(services: &AppServices) -> VoiceRouterState {
    let pool = services.database.pool().clone();
    let voice_repo = Arc::new(SqliteVoiceRepository::new(pool.clone()));
    let providers = ProviderService::new(
        Arc::new(SqliteProviderRepository::new(pool.clone())),
        derive_encryption_key(&services.encryption_secret_raw),
    );
    let credentials: Arc<dyn CredentialPort> = Arc::new(ProviderCredentialAdapter { providers });
    let policy = CredentialPolicy::from_env();
    let timings = VoiceTimings::default();
    let service = VoiceService::new(VoiceServiceDeps {
        settings: voice_repo.clone(),
        sessions: voice_repo.clone(),
        audit: voice_repo.clone(),
        credentials: credentials.clone(),
        credential_policy: policy.clone(),
        engine: Arc::new(OpenAiRealtimeEngine::new(timings.engine)),
        verifier: Arc::new(SessionTokenVerifier::new(voice_auth_state(services))),
        // Voice sockets get their own manager so app-wide broadcasts never reach them.
        sockets: VoiceSockets::new(Arc::new(WebSocketManager::new())),
        timings,
        conversations: Arc::new(super::voice_conversations::VoiceConversationAdapter {
            service: services.conversation_service.clone(),
            task_manager: services.worker_task_manager.clone(),
            sidebar: Arc::new(SqliteSidebarStore::new(pool)),
        }),
        drafts: voice_repo,
        speech: Arc::new(ProviderSpeechFactory {
            credentials,
            policy,
            http: reqwest::Client::new(),
        }),
    });
    VoiceRouterState {
        service,
        session_cookie: services.cookie_config.names.session,
    }
}

pub fn voice_guards(services: &AppServices) -> VoiceHttpGuards {
    let app = &services.origin_policy;
    let extra = std::env::var(VOICE_ALLOWED_ORIGINS_ENV).unwrap_or_default();
    let mut allowed: Vec<&str> = app.allowed_origins.iter().map(String::as_str).collect();
    allowed.extend(extra.split(','));
    let origins = OriginPolicy::new(app.public_origin.as_deref(), &allowed);
    static WARN_ONCE: std::sync::Once = std::sync::Once::new();
    if origins.public_origin.is_none() && origins.allowed_origins.is_empty() {
        WARN_ONCE.call_once(|| {
            tracing::warn!(
                env = VOICE_ALLOWED_ORIGINS_ENV,
                "voice: no allowed origins configured; browser voice requests carrying an Origin are refused"
            );
        });
    }
    VoiceHttpGuards {
        auth_state: voice_auth_state(services),
        cookie_config: services.cookie_config.clone(),
        // Non-local modes already run the global CSRF layer.
        add_csrf: services.identity_mode.is_local(),
        origins: Arc::new(origins),
    }
}
