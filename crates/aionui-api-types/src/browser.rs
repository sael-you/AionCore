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
}

/// Hard upper bound of `max_instances` (one bridge port per possible instance).
pub const BROWSER_MAX_INSTANCES_LIMIT: u8 = 5;
/// Hard upper bound of `max_tabs`.
pub const BROWSER_MAX_TABS_LIMIT: u8 = 20;

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// `GET /api/browser/settings` and the answer of `PUT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSettings {
    /// "Agents can use the browser" (default true).
    pub agents_enabled: bool,
    /// 1..=5, default 3. Instance 1 is the panel's (owner logins).
    pub max_instances: u8,
    /// 1..=20, default 10, per instance.
    pub max_tabs: u8,
    /// One row per instance 1..=max_instances.
    pub instances: Vec<BrowserInstanceSettings>,
}

/// Per-instance settings. A change applies at the instance's next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserInstanceSettings {
    pub instance: u8,
    /// "Hide that the browser is automated" (default false): launches with
    /// `--disable-blink-features=AutomationControlled`, nothing else changes.
    pub hide_automation: bool,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        let max_instances = 3;
        Self {
            agents_enabled: true,
            max_instances,
            max_tabs: 10,
            instances: (1..=max_instances)
                .map(|instance| BrowserInstanceSettings {
                    instance,
                    hide_automation: false,
                })
                .collect(),
        }
    }
}

/// `PUT /api/browser/settings`: only the given fields change. Out of range →
/// 400 `BROWSER_SETTING_OUT_OF_RANGE`. Lowering `max_instances` stops running
/// instances above the new limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSettingsUpdate {
    #[serde(default)]
    pub agents_enabled: Option<bool>,
    #[serde(default)]
    pub max_instances: Option<u8>,
    #[serde(default)]
    pub max_tabs: Option<u8>,
    /// Only the listed instances change (instance 1..=5).
    #[serde(default)]
    pub instances: Option<Vec<BrowserInstanceSettings>>,
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
    /// One row per instance 1..=max_instances.
    pub instances: Vec<BrowserInstanceStatus>,
    pub idle_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserInstanceStatus {
    /// 1 = the panel's instance with the owner logins.
    pub instance: u8,
    pub running: bool,
    pub tabs: u16,
    pub agent_active: bool,
    /// The value the running instance was launched with (false when stopped).
    pub hide_automation_active: bool,
}

/// Query of `WS /api/browser/ws?instance=<n>` (default 1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserWsQuery {
    #[serde(default)]
    pub instance: Option<u8>,
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
            json!({"agents_enabled": true, "max_instances": 3, "max_tabs": 10, "instances": [
                {"instance": 1, "hide_automation": false},
                {"instance": 2, "hide_automation": false},
                {"instance": 3, "hide_automation": false}
            ]})
        );
    }
}
