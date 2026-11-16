//! Server-side in-app browser (C30, TASK-111): one Chrome per AionUi user and
//! instance, streamed into the WebUI panel, drivable by agents through a
//! token-gated CDP bridge.

pub mod bridge;
pub mod cdp;
pub mod error;
pub mod filter;
pub mod guard;
pub mod host;
pub mod install;
pub mod instance;
pub mod layout;
pub mod mux;
pub mod panel;
pub mod proxy;
pub mod routes;
pub mod service;
pub mod settings;

pub use error::BrowserError;
pub use guard::{GuardDecision, NavigationGuard};
pub use host::HostPaths;
pub use layout::{BROWSER_USER, BrowserLayout, DEFAULT_BROWSER_ROOT};
pub use routes::{BrowserRouterState, browser_routes};
pub use service::{BrowserService, BrowserServiceConfig, DEFAULT_IDLE_TIMEOUT};
pub use settings::SettingsStore;
