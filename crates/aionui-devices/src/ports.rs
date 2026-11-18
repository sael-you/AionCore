//! What the devices registry needs from other domains. Implemented in
//! `aionui-app` (same-layer domains talk through traits only).

use serde_json::Value;

/// The managed server Chrome (C30) as one card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserFacts {
    pub installed: bool,
    pub version: Option<String>,
    /// "Agents can use the browser".
    pub agents_enabled: bool,
    /// At least one instance is running.
    pub running: bool,
}

/// One engine of the server browser (C35 / K7): a registry row
/// `server_browser:<engine_id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFacts {
    /// The engine's wire id (`chrome`, `brave`, …).
    pub engine_id: String,
    pub name: String,
    pub installed: bool,
    pub version: Option<String>,
    pub disk_bytes: Option<u64>,
    /// A download or update runs (also one started outside the registry).
    pub installing: bool,
    /// Why the last download failed.
    pub error: Option<String>,
}

/// One paired browser of the owner (C34 extension device).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MyBrowserFacts {
    pub device_id: String,
    pub name: String,
    pub browser: String,
    pub online: bool,
    pub last_seen_at: Option<i64>,
    pub extension_version: Option<String>,
}

#[async_trait::async_trait]
pub trait BrowserDevices: Send + Sync {
    fn server_browser(&self, user_id: &str) -> BrowserFacts;
    /// Every engine of the server browser, in display order. The default is
    /// the one managed Chrome of [`Self::server_browser`].
    fn server_engines(&self, user_id: &str) -> Vec<EngineFacts> {
        let f = self.server_browser(user_id);
        vec![EngineFacts {
            engine_id: "chrome".to_owned(),
            name: "Chrome".to_owned(),
            installed: f.installed,
            version: f.version,
            disk_bytes: None,
            installing: false,
            error: None,
        }]
    }
    async fn set_server_browser_enabled(&self, user_id: &str, enabled: bool) -> Result<(), String>;
    fn my_browsers(&self, user_id: &str) -> Vec<MyBrowserFacts>;
    /// "Agents may use my Mac browser" (C34): one switch for every paired browser.
    fn my_browsers_enabled(&self, user_id: &str) -> bool;
    async fn set_my_browsers_enabled(&self, user_id: &str, enabled: bool) -> Result<(), String>;
    /// Whether the browser's navigation guard (C30) blocks `url`: AionUi's
    /// own origin, loopback, private and link-local addresses. Used for
    /// `open_url` on a phone.
    fn url_blocked(&self, _url: &str) -> bool {
        false
    }
}

/// Where `devices.*` events go (the app sends them to the user's sockets).
pub trait DevicesEventSink: Send + Sync {
    fn emit(&self, user_id: &str, name: &str, data: Value);
}

pub struct NoopDevicesEventSink;

impl DevicesEventSink for NoopDevicesEventSink {
    fn emit(&self, _: &str, _: &str, _: Value) {}
}
