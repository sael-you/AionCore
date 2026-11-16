//! Server browser composition (C30, TASK-111): the service built from the
//! app config, and the conversation port over it (agent bridge env and the
//! `browser-safety` skill gate).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_browser::{
    BrowserLayout, BrowserService, BrowserServiceConfig, DEFAULT_BROWSER_ROOT, DEFAULT_IDLE_TIMEOUT, HostPaths,
    NavigationGuard,
};
use aionui_conversation::ConversationBrowserPort;

/// Browser root (`AIONUI_BROWSER_ROOT` overrides it for dev/tests; production
/// needs no env). The host hardening only runs for the real root under `/root`.
pub(crate) fn build_browser_service(
    data_dir: &Path,
    origin_policy: &aionui_auth::OriginPolicy,
) -> anyhow::Result<Arc<BrowserService>> {
    let override_root = std::env::var_os("AIONUI_BROWSER_ROOT").map(PathBuf::from);
    let root = override_root
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BROWSER_ROOT));
    let host = if override_root.is_none() {
        HostPaths::for_browser_root(&root)
    } else {
        None
    };
    let origins: Vec<&str> = origin_policy
        .public_origin
        .iter()
        .chain(origin_policy.allowed_origins.iter())
        .map(String::as_str)
        .collect();
    let service = BrowserService::new(BrowserServiceConfig {
        layout: BrowserLayout::new(root, data_dir),
        host,
        idle_timeout: DEFAULT_IDLE_TIMEOUT,
        guard: NavigationGuard::from_origins(origins),
        chrome_override: std::env::var_os("AIONUI_BROWSER_CHROME").map(PathBuf::from),
        run_as_override: None,
    })
    .map_err(|e| anyhow::anyhow!("browser service: {e}"))?;
    Ok(service)
}

pub(crate) struct BrowserConversationPort {
    pub service: Arc<BrowserService>,
}

#[async_trait::async_trait]
impl ConversationBrowserPort for BrowserConversationPort {
    async fn agent_env(&self, user_id: &str) -> Vec<(String, String)> {
        self.service.agent_env(user_id).await
    }

    fn agents_enabled(&self, user_id: &str) -> bool {
        self.service.agents_enabled(user_id)
    }
}
