use std::sync::Arc;

use crate::service::DevicesService;

#[derive(Clone)]
pub struct DevicesRouterState {
    pub service: Arc<DevicesService>,
}
