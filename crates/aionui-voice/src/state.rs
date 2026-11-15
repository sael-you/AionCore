use crate::service::VoiceService;

/// Router state of the voice user routes.
#[derive(Clone)]
pub struct VoiceRouterState {
    pub service: VoiceService,
}
