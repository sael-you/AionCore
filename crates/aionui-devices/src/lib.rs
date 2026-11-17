//! Settings → Devices (C38, TASK-136): one registry over every
//! agent-reachable device. Server browsers and the owner's own browsers come
//! from `aionui-browser` (through [`ports::BrowserDevices`], no second store);
//! phones, the Mac and add-ons live in migration 911.

pub mod addons;
pub mod agent;
pub mod approvals;
pub mod companion;
pub mod error;
pub mod ports;
pub mod relay;
pub mod routes;
pub mod runtime_routes;
pub mod service;
pub mod state;

pub use addons::{AddonRunOutcome, AddonTask};
pub use agent::AgentCall;
pub use approvals::OwnerQuestion;
pub use error::DevicesError;
pub use ports::{BrowserDevices, BrowserFacts, DevicesEventSink, MyBrowserFacts, NoopDevicesEventSink};
pub use routes::{devices_relay_routes, devices_routes};
pub use runtime_routes::{DevicesRuntimeRouterState, devices_runtime_routes};
pub use service::DevicesService;
pub use state::DevicesRouterState;
