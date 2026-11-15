use crate::service::VoiceService;

/// Router state of the voice user routes.
#[derive(Clone)]
pub struct VoiceRouterState {
    pub service: VoiceService,
    /// Session cookie name in use (`CookieConfig::names.session`: prefixed
    /// over HTTPS in webui mode).
    pub session_cookie: &'static str,
}
