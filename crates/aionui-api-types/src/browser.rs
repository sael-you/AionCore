//! Server-side in-app browser (C30, TASK-111): the wire types of
//! `WS /api/browser/ws` and `/api/browser/*`. snake_case on the wire, matched
//! field by field by the AionUi renderer.
//!
//! Never on this wire: the CDP port, the bridge token, cookies or storage.

use serde::{Deserialize, Serialize};

/// Error codes of the browser routes, the panel socket and the agent bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BrowserErrorCode {
    /// Chrome is not installed (yet), or the host self-check refused to start it.
    BrowserUnavailable,
    /// Chrome did not start or exited.
    BrowserStartFailed,
    /// The command needs a running browser.
    BrowserNotRunning,
    /// The navigation guard refused the URL (AionUi origin, private IPs,
    /// `file:`, `chrome:` …).
    BrowserUrlBlocked,
    /// "Agents can use the browser" is off: the bridge refuses agent CDP.
    BrowserAgentsDisabled,
    /// A CDP method agents may not call (cookie / storage APIs).
    BrowserMethodBlocked,
    /// `tab_new` (panel or agent) beyond `max_tabs`.
    BrowserTabLimit,
    /// An instance number above `max_instances` (or outside 1..=5).
    BrowserInstanceLimit,
    /// A settings value outside its range (HTTP 400).
    BrowserSettingOutOfRange,
    BrowserTabNotFound,
    /// A panel message that does not parse or is out of range.
    BrowserBadMessage,
    /// `clear` while that profile is being wiped already.
    BrowserBusy,
    /// Extension pairing code unknown, used or expired (or too many tries).
    BrowserPairingInvalid,
    /// The device's token was revoked (or never existed).
    BrowserDeviceRevoked,
    /// No such paired device.
    BrowserDeviceNotFound,
    /// The owner denied an action on a high-value origin.
    BrowserApprovalDenied,
    /// Nobody answered the approval in time.
    BrowserApprovalTimeout,
    /// The approval is no longer pending (answered or expired).
    BrowserApprovalExpired,
    /// Another instance of this user already has that name (case-insensitive).
    BrowserProfileNameTaken,
    /// Instance 1 (the panel default with the owner's logins) cannot be deleted.
    BrowserInstanceProtected,
    /// No instance with that number exists.
    BrowserInstanceNotFound,
}

/// Hard upper bound of `max_instances` (one bridge port per possible instance).
pub const BROWSER_MAX_INSTANCES_LIMIT: u8 = 5;
/// Hard upper bound of `max_tabs`.
pub const BROWSER_MAX_TABS_LIMIT: u8 = 20;
/// The AionUi browser extension (manifest key pinned; C34). The extension
/// socket only accepts `Origin: chrome-extension://<this id>`.
pub const BROWSER_EXTENSION_ID: &str = "bdgcogndgcbcmaijgnhahjddbaclknbb";
/// Palette ids an instance colour may take (the UI's project palette).
pub const BROWSER_INSTANCE_COLORS: &[&str] = &[
    "red", "orange", "amber", "lime", "green", "teal", "cyan", "blue", "indigo", "violet", "pink", "slate",
];
/// Longest instance name (characters, after trimming).
pub const BROWSER_INSTANCE_NAME_MAX: usize = 40;
/// Longest instance emoji (UTF-8 bytes).
pub const BROWSER_INSTANCE_EMOJI_MAX_BYTES: usize = 16;

/// Default name of instance `n`: "Main" for 1, else "Instance n".
pub fn browser_instance_default_name(n: u8) -> String {
    if n == 1 {
        "Main".to_owned()
    } else {
        format!("Instance {n}")
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// The browser engines an instance can run (C35, all of the Chromium family:
/// same CDP bridge, filters, guard and panel). Wire ids are kebab-case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserEngine {
    /// Google Chrome stable (the default; installed on first use).
    #[default]
    Chrome,
    ChromeForTesting,
    Brave,
    Edge,
    /// `chrome-headless-shell`: no window, lighter; its User-Agent says
    /// HeadlessChrome, so it is for agent work, not for sign-ins.
    HeadlessShell,
}

/// Every engine, in display order.
pub const BROWSER_ENGINES: [BrowserEngine; 5] = [
    BrowserEngine::Chrome,
    BrowserEngine::ChromeForTesting,
    BrowserEngine::Brave,
    BrowserEngine::Edge,
    BrowserEngine::HeadlessShell,
];

impl BrowserEngine {
    /// The wire id (also a safe directory name).
    pub fn id(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::ChromeForTesting => "chrome-for-testing",
            Self::Brave => "brave",
            Self::Edge => "edge",
            Self::HeadlessShell => "headless-shell",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        BROWSER_ENGINES.into_iter().find(|e| e.id() == id)
    }
}

/// What AionCore could verify about an engine download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserEngineVerification {
    /// Pinned vendor key → signed apt `InRelease` → `Packages` sha256 →
    /// package sha256 + size.
    SignedRepository,
    /// The vendor publishes no signature or checksum: HTTPS from the vendor's
    /// hosts and the version from the vendor's JSON only. The sha256 of what
    /// was downloaded is recorded next to the install.
    TlsOnly,
}

/// One engine in `BrowserStatusResponse.engines`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserEngineStatus {
    pub engine: BrowserEngine,
    /// The installed version; `None` = not downloaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// A download or update of this engine is running.
    pub installing: bool,
    /// Why the last download failed (cleared by the next success).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub verification: BrowserEngineVerification,
    /// No window and a headless User-Agent: meant for agent-only instances.
    pub agents_only: bool,
}

/// `GET /api/browser/settings` and the answer of `PUT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSettings {
    /// "Agents can use the browser" (default true).
    pub agents_enabled: bool,
    /// The most instances (profiles) a user may have: 1..=5, default 3; never
    /// below the number of existing instances.
    pub max_instances: u8,
    /// 1..=20, default 10, per instance.
    pub max_tabs: u8,
    /// "Agents may use my Mac browser" (C34, default false). Device commands
    /// also need `agents_enabled`.
    #[serde(default)]
    pub devices_agents_enabled: bool,
    /// The EXISTING instances, ascending. Instance 1 always exists.
    pub instances: Vec<BrowserInstanceSettings>,
}

/// One instance (a named browser profile). A `hide_automation` change applies
/// at the instance's next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserInstanceSettings {
    pub instance: u8,
    /// 1..=40 characters, unique per user (case-insensitive).
    pub name: String,
    /// A palette id from [`BROWSER_INSTANCE_COLORS`]; `None` = neutral.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// At most 16 bytes of UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// "Hide that the browser is automated" (default false): launches with
    /// `--disable-blink-features=AutomationControlled`, nothing else changes.
    pub hide_automation: bool,
    /// The engine this instance runs (default Chrome). A change applies at
    /// the instance's next start; the profile directory is kept.
    #[serde(default)]
    pub engine: BrowserEngine,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        let max_instances = 3;
        Self {
            agents_enabled: true,
            max_instances,
            max_tabs: 10,
            devices_agents_enabled: false,
            instances: (1..=max_instances)
                .map(|instance| BrowserInstanceSettings {
                    instance,
                    name: browser_instance_default_name(instance),
                    color: None,
                    emoji: None,
                    hide_automation: false,
                    engine: BrowserEngine::default(),
                })
                .collect(),
        }
    }
}

/// `PUT /api/browser/settings`: only the given fields change. Out of range →
/// 400 `BROWSER_SETTING_OUT_OF_RANGE` (also `max_instances` below the number
/// of existing instances).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSettingsUpdate {
    #[serde(default)]
    pub agents_enabled: Option<bool>,
    #[serde(default)]
    pub max_instances: Option<u8>,
    #[serde(default)]
    pub max_tabs: Option<u8>,
    #[serde(default)]
    pub devices_agents_enabled: Option<bool>,
    /// Existing instances only (unknown → 404 `BROWSER_INSTANCE_NOT_FOUND`).
    #[serde(default)]
    pub instances: Option<Vec<BrowserInstanceUpdate>>,
}

/// One row of `BrowserSettingsUpdate.instances`: absent = unchanged; `""`
/// for `color` / `emoji` clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserInstanceUpdate {
    pub instance: u8,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub hide_automation: Option<bool>,
    /// Unknown id → 400 (serde); applies at the next start.
    #[serde(default)]
    pub engine: Option<BrowserEngine>,
}

/// `PATCH /api/browser/instances/{n}`: absent = unchanged; `""` clears
/// `color` / `emoji`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserInstancePatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub hide_automation: Option<bool>,
    /// Unknown id → 400 (serde); applies at the next start.
    #[serde(default)]
    pub engine: Option<BrowserEngine>,
}

/// `POST /api/browser/instances`: a new instance at the lowest free number
/// (409 `BROWSER_INSTANCE_LIMIT` when `max_instances` are in use).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserInstanceCreate {
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub hide_automation: Option<bool>,
    /// Unknown id → 400 (serde); applies at the next start.
    #[serde(default)]
    pub engine: Option<BrowserEngine>,
}

/// `GET /api/browser/status`, also the answer of
/// `POST /api/browser/instances/{n}/clear` and `/stop`. Never a port or token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserStatusResponse {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<BrowserErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_version: Option<String>,
    /// One row per existing instance, ascending.
    pub instances: Vec<BrowserInstanceStatus>,
    pub idle_timeout_secs: u64,
    /// Every engine with its install state (C35), in display order.
    #[serde(default)]
    pub engines: Vec<BrowserEngineStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserInstanceStatus {
    /// 1 = the panel's instance with the owner logins.
    pub instance: u8,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    pub running: bool,
    pub tabs: u16,
    pub agent_active: bool,
    /// The value the running instance was launched with (false when stopped).
    pub hide_automation_active: bool,
    /// The configured engine of this instance.
    #[serde(default)]
    pub engine: BrowserEngine,
    /// The engine the running instance was launched with (`None` when stopped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_active: Option<BrowserEngine>,
}

/// Query of `WS /api/browser/ws?instance=<n>` (default 1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserWsQuery {
    #[serde(default)]
    pub instance: Option<u8>,
}

// ---------------------------------------------------------------------------
// C34: the owner's own browser through the AionUi extension
// ---------------------------------------------------------------------------

/// `POST /api/browser/devices/pairing-code`: one-time, 5 minutes; a new code
/// replaces the previous one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserPairingCode {
    pub code: String,
    /// Epoch ms.
    pub expires_at: i64,
}

/// `GET /api/browser/devices` row. Never the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDevice {
    pub device_id: String,
    pub name: String,
    pub browser: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    pub online: bool,
    /// The extension's "agents may open new tabs" switch.
    pub new_tabs: bool,
    pub shared_tabs: u16,
}

/// A paused agent action on a high-value origin, waiting for the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserApproval {
    pub approval_id: String,
    pub device_id: String,
    pub device_name: String,
    pub tab_id: i64,
    pub origin: String,
    pub url: String,
    pub method: String,
    /// The method plus at most 120 characters of the expression / text.
    pub summary: String,
    pub requested_at: i64,
    pub expires_at: i64,
}

/// `POST /api/browser/approvals/{approval_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserApprovalDecision {
    pub approve: bool,
}

/// Payload of the `browser.approvalResolved` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserApprovalResolved {
    pub approval_id: String,
    pub approved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserActivityOutcome {
    Ok,
    Refused,
    Paused,
    Approved,
    Denied,
    Timeout,
}

/// `GET /api/browser/devices/{device_id}/activity` row: method names only,
/// never params or results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDeviceActivity {
    pub at: i64,
    pub conversation_id: Option<String>,
    pub tab_id: i64,
    pub origin: String,
    pub method: String,
    pub outcome: BrowserActivityOutcome,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserActivityQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A tab the owner shared from the extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionTab {
    pub tab_id: i64,
    pub url: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// `WS /api/browser/extension`, extension → AionCore. The first frame is
/// `pair` or `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionInbound {
    Pair {
        code: String,
        device_name: String,
        #[serde(default)]
        browser: String,
    },
    Hello {
        token: String,
        #[serde(default)]
        browser: String,
        #[serde(default)]
        version: Option<u32>,
    },
    /// The FULL list of shared tabs, on every change.
    State {
        new_tabs: bool,
        tabs: Vec<ExtensionTab>,
    },
    Result {
        id: u64,
        #[serde(default)]
        result: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<ExtensionError>,
    },
    Event {
        tab_id: i64,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    Detached {
        tab_id: i64,
        #[serde(default)]
        reason: Option<String>,
    },
    Ping,
}

/// `WS /api/browser/extension`, AionCore → extension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionOutbound {
    /// `token` only in the reply to `pair`.
    Welcome {
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    Error {
        code: BrowserErrorCode,
    },
    Command {
        id: u64,
        tab_id: i64,
        method: String,
        params: serde_json::Value,
    },
    CreateTab {
        id: u64,
        url: String,
    },
    CloseTab {
        id: u64,
        tab_id: i64,
    },
    Pong,
}

// ---------------------------------------------------------------------------
// WS /api/browser/ws — server → client
// ---------------------------------------------------------------------------

/// The JSON header of a binary screencast frame. Binary message layout:
/// `[u32 big-endian meta_len][meta JSON, meta_len bytes][JPEG bytes]`.
/// Values come from CDP `Page.screencastFrame.metadata` (CSS pixels).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserFrameMeta {
    pub seq: u64,
    pub tab_id: String,
    /// `deviceWidth` / `deviceHeight`.
    pub width: f64,
    pub height: f64,
    /// The panel's own value from the last `resize`.
    pub device_scale_factor: f64,
    pub page_scale_factor: f64,
    pub offset_top: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_y: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserServerMessage {
    State(BrowserState),
    Dialog(BrowserDialog),
    FileChooser(BrowserFileChooser),
    Error(BrowserWsError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserRunStatus {
    Starting,
    Running,
    Stopped,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserState {
    pub instance: u8,
    pub max_tabs: u8,
    pub status: BrowserRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<BrowserErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_version: Option<String>,
    pub tabs: Vec<BrowserTab>,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    /// An agent sent CDP within the last few seconds ("agent is driving").
    pub agent_active: bool,
    /// The tab an agent last acted on (the panel may follow it live).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_tab_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTab {
    pub id: String,
    pub url: String,
    pub title: String,
    pub active: bool,
    pub loading: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDialogKind {
    Alert,
    Confirm,
    Prompt,
    Beforeunload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDialog {
    pub tab_id: String,
    pub kind: BrowserDialogKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserFileChooserMode {
    Single,
    Multiple,
}

/// v1: the panel says "file upload not supported"; the server cancels it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserFileChooser {
    pub tab_id: String,
    pub mode: BrowserFileChooserMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserWsError {
    pub code: BrowserErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// WS /api/browser/ws — client → server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserClientMessage {
    Start,
    Navigate {
        url: String,
    },
    Back,
    Forward,
    Reload,
    /// Stop loading the active tab (not the browser).
    Stop,
    TabNew {
        #[serde(default)]
        url: Option<String>,
    },
    TabSelect {
        id: String,
    },
    TabClose {
        id: String,
    },
    Resize {
        width: u32,
        height: u32,
        device_scale_factor: f64,
    },
    Mouse(BrowserMouseInput),
    Key(BrowserKeyInput),
    /// Paste / IME commit (CDP `Input.insertText`).
    InsertText {
        text: String,
    },
    DialogReply {
        tab_id: String,
        accept: bool,
        #[serde(default)]
        prompt_text: Option<String>,
    },
    /// Client-paced screencast: at most 2 frames in flight.
    FrameAck {
        seq: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMouseEventKind {
    Move,
    Down,
    Up,
    Wheel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMouseButton {
    None,
    Left,
    Middle,
    Right,
}

/// Coordinates in CSS pixels of the frame (`BrowserFrameMeta`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserMouseInput {
    pub event: BrowserMouseEventKind,
    pub x: f64,
    pub y: f64,
    pub button: BrowserMouseButton,
    #[serde(default)]
    pub click_count: u32,
    /// CDP bitmask: Alt=1, Ctrl=2, Meta=4, Shift=8.
    #[serde(default)]
    pub modifiers: u32,
    #[serde(default)]
    pub delta_x: Option<f64>,
    #[serde(default)]
    pub delta_y: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserKeyEventKind {
    Down,
    Up,
    Char,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserKeyInput {
    pub event: BrowserKeyEventKind,
    pub key: String,
    pub code: String,
    #[serde(default)]
    pub text: Option<String>,
    /// `windowsVirtualKeyCode`.
    #[serde(default)]
    pub key_code: u32,
    #[serde(default)]
    pub modifiers: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_messages_parse_the_panel_shapes() {
        let mouse: BrowserClientMessage = serde_json::from_value(json!({
            "type": "mouse", "event": "down", "x": 10.5, "y": 20.0, "button": "left",
            "click_count": 1, "modifiers": 8
        }))
        .unwrap();
        assert!(
            matches!(mouse, BrowserClientMessage::Mouse(m) if m.button == BrowserMouseButton::Left && m.modifiers == 8)
        );
        let tab: BrowserClientMessage = serde_json::from_value(json!({"type": "tab_new"})).unwrap();
        assert_eq!(tab, BrowserClientMessage::TabNew { url: None });
        let ack: BrowserClientMessage = serde_json::from_value(json!({"type": "frame_ack", "seq": 7})).unwrap();
        assert_eq!(ack, BrowserClientMessage::FrameAck { seq: 7 });
        assert!(serde_json::from_value::<BrowserClientMessage>(json!({"type": "eval", "js": "1"})).is_err());
    }

    #[test]
    fn server_messages_and_codes_serialize_as_agreed() {
        let err = BrowserServerMessage::Error(BrowserWsError {
            code: BrowserErrorCode::BrowserUrlBlocked,
            message: None,
        });
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"type": "error", "code": "BROWSER_URL_BLOCKED"})
        );
        let chooser = BrowserServerMessage::FileChooser(BrowserFileChooser {
            tab_id: "t1".into(),
            mode: BrowserFileChooserMode::Single,
        });
        assert_eq!(
            serde_json::to_value(&chooser).unwrap(),
            json!({"type": "file_chooser", "tab_id": "t1", "mode": "single"})
        );
        assert!(serde_json::from_value::<BrowserSettingsUpdate>(json!({"max_tabs": 5, "x": 1})).is_err());
        assert_eq!(
            serde_json::to_value(BrowserSettings::default()).unwrap(),
            json!({"agents_enabled": true, "max_instances": 3, "max_tabs": 10, "devices_agents_enabled": false, "instances": [
                {"instance": 1, "name": "Main", "hide_automation": false},
                {"instance": 2, "name": "Instance 2", "hide_automation": false},
                {"instance": 3, "name": "Instance 3", "hide_automation": false}
            ]})
        );
    }

    #[test]
    fn extension_frames_match_the_agreed_wire() {
        let pair: ExtensionInbound = serde_json::from_value(
            json!({"type": "pair", "code": "ABCD-2345", "device_name": "Mac", "browser": "Chrome 157"}),
        )
        .unwrap();
        assert!(matches!(pair, ExtensionInbound::Pair { ref code, .. } if code == "ABCD-2345"));
        let hello: ExtensionInbound =
            serde_json::from_value(json!({"type": "hello", "token": "t", "browser": "Chrome", "version": 1})).unwrap();
        assert!(matches!(hello, ExtensionInbound::Hello { version: Some(1), .. }));
        let state: ExtensionInbound = serde_json::from_value(json!({
            "type": "state", "new_tabs": true, "tabs": [{"tab_id": 123, "url": "https://a.test/", "title": "A"}]
        }))
        .unwrap();
        assert!(matches!(state, ExtensionInbound::State { new_tabs: true, ref tabs } if tabs[0].tab_id == 123));
        let res: ExtensionInbound = serde_json::from_value(
            json!({"type": "result", "id": 7, "error": {"message": "off", "code": "new_tabs_disabled"}}),
        )
        .unwrap();
        assert!(
            matches!(res, ExtensionInbound::Result { id: 7, error: Some(ref e), .. } if e.code.as_deref() == Some("new_tabs_disabled"))
        );
        let ping: ExtensionInbound = serde_json::from_value(json!({"type": "ping"})).unwrap();
        assert_eq!(ping, ExtensionInbound::Ping);

        assert_eq!(
            serde_json::to_value(ExtensionOutbound::Welcome {
                device_id: "dev_1".into(),
                token: None
            })
            .unwrap(),
            json!({"type": "welcome", "device_id": "dev_1"})
        );
        assert_eq!(
            serde_json::to_value(ExtensionOutbound::Error {
                code: BrowserErrorCode::BrowserPairingInvalid
            })
            .unwrap(),
            json!({"type": "error", "code": "BROWSER_PAIRING_INVALID"})
        );
        assert_eq!(
            serde_json::to_value(ExtensionOutbound::Command {
                id: 7,
                tab_id: 123,
                method: "Runtime.evaluate".into(),
                params: json!({"expression": "1"})
            })
            .unwrap(),
            json!({"type": "command", "id": 7, "tab_id": 123, "method": "Runtime.evaluate", "params": {"expression": "1"}})
        );
        assert_eq!(
            serde_json::to_value(ExtensionOutbound::CreateTab {
                id: 8,
                url: "https://a.test/".into()
            })
            .unwrap(),
            json!({"type": "create_tab", "id": 8, "url": "https://a.test/"})
        );
        assert_eq!(
            serde_json::to_value(ExtensionOutbound::Pong).unwrap(),
            json!({"type": "pong"})
        );
        assert!(serde_json::from_value::<BrowserApprovalDecision>(json!({"approve": true, "x": 1})).is_err());
        assert_eq!(
            serde_json::to_value(BrowserActivityOutcome::Timeout).unwrap(),
            json!("timeout")
        );
    }
}
