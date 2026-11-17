use std::sync::Arc;

use aionui_system::ClientPrefService;

use crate::shell::ShellService;
use crate::stt::SttService;
use crate::stt_key::SttKeyFallback;

#[derive(Clone)]
pub struct ShellRouterState {
    pub shell_service: Arc<ShellService>,
    pub stt_service: Arc<SttService>,
    pub client_pref_service: ClientPrefService,
    /// The Model-page OpenAI key for STT settings without their own key.
    pub stt_key_fallback: Option<Arc<dyn SttKeyFallback>>,
}
