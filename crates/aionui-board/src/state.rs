//! Router states (dependency carriers; built by `aionui-app`).

use std::sync::Arc;

use aionui_ai_agent::RuntimeTokenService;

use crate::service::BoardService;

/// State of the user router.
#[derive(Clone)]
pub struct BoardRouterState {
    pub service: BoardService,
}

/// State of the agent router: the service plus the runtime-token validator.
#[derive(Clone)]
pub struct BoardRuntimeRouterState {
    pub service: BoardService,
    pub runtime_token_service: Arc<RuntimeTokenService>,
}
