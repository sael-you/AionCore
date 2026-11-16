//! Server browser (C30, TASK-111) as seen by conversations: the CDP bridge env
//! an agent gets, and whether the `browser-safety` skill is delivered.
//! Implemented by the app over `aionui-browser`.

/// Built-in skill delivered only while agents may use the browser.
pub const BROWSER_SAFETY_SKILL: &str = "browser-safety";

/// Env keys this port owns (stripped before re-applying, so a rebuilt agent
/// never keeps a stale bridge after the switch was turned off).
pub const BROWSER_ENV_KEYS: &[&str] = &[
    "AIONUI_CDP_ACTIVE_PORT",
    "AIONUI_CDP_ACTIVE_TOKEN",
    "AIONUI_CDP_PORTS",
    "PLAYWRIGHT_MCP_CDP_HEADERS",
];

#[async_trait::async_trait]
pub trait ConversationBrowserPort: Send + Sync {
    /// Env pairs for an agent of `user_id`; empty while agents may not use the browser.
    async fn agent_env(&self, user_id: &str) -> Vec<(String, String)>;
    /// "Agents can use the browser" for `user_id`.
    fn agents_enabled(&self, user_id: &str) -> bool;
}
