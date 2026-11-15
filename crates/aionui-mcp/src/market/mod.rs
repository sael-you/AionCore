//! MCP market (card C15, design/mcp/PLAN.md with "Review R1"): templates from
//! the catalogue extension, market instances (market columns of
//! `mcp_servers`), write-only encrypted secrets, assignments, status, the
//! kit-lock snapshot for `studio sync`, and the launcher core used by
//! `aioncore mcp exec` (launch resolution + filtering stdio proxy).

mod assignments;
pub mod error;
pub mod launch;
pub mod launcher;
pub mod proxy;
pub mod rules;
mod secrets;
mod service;
mod snapshot;
pub mod stored;
pub mod template;

pub use error::MarketError;
pub use launch::{
    LaunchPlan, LaunchRequest, LaunchScope, child_env, child_env_with, inherit_env_allowed, resolve_launch,
    still_allowed,
};
pub use launcher::{LAUNCHER_ENV, launcher_program, resolve_placeholder_row, resolve_placeholder_transport};
pub use proxy::{ProxyExit, ToolPolicy, run_stdio_proxy};
pub use service::{IMcpAssistantBinder, McpMarketService};
pub use template::{CATALOG_EXTENSION, IMcpTemplateCatalog, StaticTemplateCatalog, parse_template};
