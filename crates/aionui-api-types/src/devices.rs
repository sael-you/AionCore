//! Devices (C38 / TASK-136): every agent-reachable device on one page
//! (server browsers, the owner's own browsers, phones, the Mac, add-ons), the
//! device relay the Mac helper and the Android companion app connect to, and
//! the approvals and audit around them.
//!
//! Wire contracts W1 (REST + events) and W2 (relay WebSocket) of
//! `design/mobile/C38-ANDROID-ACCESS.md` §6. Additions to W2 agreed with the
//! lanes: every frame carries `type`; a first `pair` frame (like C34) next to
//! `hello`; an `inventory` frame lists what a relay client reaches.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeviceErrorCode {
    /// No such device (or another user's).
    DeviceNotFound,
    /// Pairing code unknown, used, expired or burned after wrong attempts.
    DevicePairingInvalid,
    /// The relay token was revoked (or never existed).
    DeviceRevoked,
    /// The device's agent switch is off or its timeout passed.
    DeviceDisabled,
    /// The device (or the relay that reaches it) is not connected.
    DeviceOffline,
    /// A verb agents may never use, or the peer refused it.
    DeviceVerbRefused,
    /// The owner denied the action.
    DeviceApprovalDenied,
    /// Nobody answered the approval in time.
    DeviceApprovalTimeout,
    /// The approval is no longer pending.
    DeviceApprovalExpired,
    /// A frame or request that does not parse / is out of range.
    DeviceBadMessage,
    /// Too many calls (rate limit) or a job already running for the device.
    DeviceBusy,
    /// No such install/remove job.
    DeviceJobNotFound,
    /// Remove refused: something still uses it ("switch the instance first").
    DeviceInUse,
    /// The relay socket refuses browser origins.
    DeviceOriginRefused,
}

// ---------------------------------------------------------------------------
// W1 · registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    ServerBrowser,
    MyBrowser,
    Phone,
    Mac,
    Addon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    On,
    Off,
    Offline,
    NeedsPairing,
    Installing,
    Removing,
    Error,
}

/// How a phone is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRouteKind {
    /// USB / Wi-Fi debugging through the paired Mac helper (route 1).
    MacRelay,
    /// The AionUi companion app on the phone (route 3).
    Companion,
    /// Wireless debugging from the server (route 2, experimental until T0).
    WifiDirect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRoute {
    pub route: DeviceRouteKind,
    pub enabled: bool,
    pub status: DeviceStatus,
    pub experimental: bool,
}

/// `GET /api/devices` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Stable: `server_browser:<engine>`, `my_browser:<dev_…>`, `phone:<id>`,
    /// `mac:<id>`, `addon:<name>`.
    pub id: String,
    pub kind: DeviceKind,
    pub name: String,
    pub status: DeviceStatus,
    /// The agent switch.
    pub enabled: bool,
    /// Epoch ms when the switch turns itself off (phones / mac), else null.
    pub enabled_until: Option<i64>,
    pub installed: bool,
    pub disk_bytes: Option<u64>,
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<DeviceRoute>>,
    /// Add-ons: the device that runs them (a Mac helper, or `server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addon_host: Option<String>,
}

/// Timeouts a phone / Mac switch may take (minutes).
pub const DEVICE_TIMEOUT_MINUTES: &[u32] = &[15, 30, 60];

/// `PUT /api/devices/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceUpdate {
    pub enabled: bool,
    /// Phones / Mac only: 15, 30 or 60. Ignored for other kinds.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
}

/// `PUT /api/devices/{id}/routes/{route}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceRouteUpdate {
    pub enabled: bool,
}

/// `POST /api/devices/{id}/install|remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceJobStarted {
    pub job_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceJobState {
    Queued,
    Running,
    Done,
    Failed,
}

/// `GET /api/devices/jobs/{job_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceJob {
    pub job_id: String,
    pub device_id: String,
    pub state: DeviceJobState,
    /// 0..=100.
    pub progress: u8,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePairingKind {
    MacHelper,
    Companion,
    WifiDirect,
}

/// `POST /api/devices/pairing-codes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePairingCodeRequest {
    pub kind: DevicePairingKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePairingCode {
    /// `ABCD-2345` (one use, 5 minutes; a new code replaces the old one).
    pub code: String,
    pub expires_at: i64,
    /// Companion: short-lived signed APK link (the UI renders it as a QR).
    pub download_url: Option<String>,
}

/// `GET /api/devices/approvals` row; also the payload of
/// `devices.approval {type: requested}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceApproval {
    pub approval_id: String,
    pub device_id: String,
    pub device_name: String,
    pub verb: String,
    /// What it would do, at most 200 characters (the task text for add-ons).
    pub summary: String,
    /// Why it needs an OK: the package / origin / verb class.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub requested_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceApprovalVerdict {
    Allow,
    Deny,
}

/// `POST /api/devices/approvals/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceApprovalDecision {
    pub decision: DeviceApprovalVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAuditOutcome {
    Ok,
    Error,
    Refused,
    Paused,
    Approved,
    Denied,
    Timeout,
}

/// `GET /api/devices/{id}/audit?cursor=` row. Never screenshots, typed text
/// or command output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAuditEntry {
    pub id: i64,
    pub at: i64,
    pub device_id: String,
    pub verb: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub outcome: DeviceAuditOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceAuditQuery {
    /// Opaque: the `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAuditPage {
    pub items: Vec<DeviceAuditEntry>,
    pub next_cursor: Option<String>,
}

/// WS event names (existing user WebSocket, owning user only).
pub const EVENT_DEVICES_CHANGED: &str = "devices.changed";
pub const EVENT_DEVICES_APPROVAL: &str = "devices.approval";

/// Payload of `devices.approval`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceApprovalEvent {
    Requested { approval: DeviceApproval },
    Resolved { approval_id: String, approved: bool },
}

// ---------------------------------------------------------------------------
// W2 · relay WebSocket `/api/devices/relay`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayClientKind {
    MacHelper,
    Companion,
}

/// A phone a relay client reaches (Mac helper: one per `adb devices` entry;
/// companion: exactly the phone it runs on).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPhone {
    /// Stable per client: adb serial (Mac helper) or `self` (companion).
    pub serial: String,
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub android_version: Option<String>,
    /// Chrome DevTools socket reachable (Mac helper via `adb forward`).
    #[serde(default)]
    pub chrome: bool,
    /// The peer's own switch for this phone (defence in depth).
    #[serde(default)]
    pub enabled: bool,
}

/// An add-on installed on a relay client (Mac helper).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayAddon {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub disk_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayError {
    pub code: String,
    pub message: String,
}

/// Client → AionCore. The first frame is `pair` or `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayInbound {
    /// First connect: one-time code from `POST /api/devices/pairing-codes`.
    Pair {
        code: String,
        kind: RelayClientKind,
        name: String,
        version: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    /// Later connects: the token from `welcome` (never in the URL).
    Hello {
        token: String,
        kind: RelayClientKind,
        version: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    /// What the client reaches now; sent after `welcome` and on every change.
    Inventory {
        #[serde(default)]
        phones: Vec<RelayPhone>,
        #[serde(default)]
        addons: Vec<RelayAddon>,
    },
    Response {
        id: u64,
        ok: bool,
        #[serde(default)]
        result: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<RelayError>,
    },
    /// `device` = the phone serial (or `self`); `name` e.g. `addon_progress`,
    /// `foreground`, `stopped` (the owner pressed Stop on the phone / Mac).
    Event {
        device: String,
        name: String,
        #[serde(default)]
        data: serde_json::Value,
    },
    Ping,
}

/// AionCore → client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayOutbound {
    /// `token` only in the reply to `pair`.
    Welcome {
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Then the socket closes.
    Error { code: DeviceErrorCode },
    /// `device` = phone serial (or `self`); verbs listed in [`RELAY_VERBS`].
    Request {
        id: u64,
        device: String,
        verb: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// The owner switched this phone off / its timeout passed: stop at once.
    Stop { device: String },
    Pong,
}

/// W2 verbs. Phone: screenshot{scale} · ui_tree · tap{x,y} ·
/// swipe{x1,y1,x2,y2,duration_ms} · text{value} · key{code} ·
/// launch{package} · foreground → {package} · install_apk{url, sha256} ·
/// logcat{package, lines} · cdp{session, message} (mac_relay only) · stop.
/// Mac: mac_screenshot · mac_ax_tree · mac_click · mac_type · mac_key ·
/// mac_open_app. Add-on host: addon_install{name, version} ·
/// addon_remove{name} · addon_run{name, task, device, max_minutes} ·
/// addon_stop{run_id}.
pub const RELAY_VERBS: &[&str] = &[
    "screenshot",
    "ui_tree",
    "tap",
    "swipe",
    "text",
    "key",
    "launch",
    "foreground",
    "install_apk",
    "logcat",
    "cdp",
    "stop",
    "mac_screenshot",
    "mac_ax_tree",
    "mac_click",
    "mac_type",
    "mac_key",
    "mac_open_app",
    "addon_install",
    "addon_remove",
    "addon_run",
    "addon_stop",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn w1_shapes() {
        let d = Device {
            id: "phone:p1".into(),
            kind: DeviceKind::Phone,
            name: "Pixel 9".into(),
            status: DeviceStatus::NeedsPairing,
            enabled: false,
            enabled_until: None,
            installed: true,
            disk_bytes: None,
            version: None,
            routes: Some(vec![DeviceRoute {
                route: DeviceRouteKind::WifiDirect,
                enabled: false,
                status: DeviceStatus::Off,
                experimental: true,
            }]),
            addon_host: None,
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({"id": "phone:p1", "kind": "phone", "name": "Pixel 9", "status": "needs_pairing",
                "enabled": false, "enabled_until": null, "installed": true, "disk_bytes": null, "version": null,
                "routes": [{"route": "wifi_direct", "enabled": false, "status": "off", "experimental": true}]})
        );
        assert!(serde_json::from_value::<DeviceUpdate>(json!({"enabled": true, "x": 1})).is_err());
        let ev = DeviceApprovalEvent::Resolved {
            approval_id: "a".into(),
            approved: false,
        };
        assert_eq!(
            serde_json::to_value(ev).unwrap(),
            json!({"type": "resolved", "approval_id": "a", "approved": false})
        );
        assert_eq!(
            serde_json::to_value(DeviceErrorCode::DeviceInUse).unwrap(),
            json!("DEVICE_IN_USE")
        );
    }

    #[test]
    fn w2_frames() {
        let hello: RelayInbound = serde_json::from_value(json!({
            "type": "hello", "token": "t", "kind": "mac_helper", "version": "0.1.0", "capabilities": ["tap"]
        }))
        .unwrap();
        assert!(matches!(hello, RelayInbound::Hello { kind: RelayClientKind::MacHelper, .. }));
        let resp: RelayInbound = serde_json::from_value(json!({
            "type": "response", "id": 4, "ok": false, "error": {"code": "ADB_FAILED", "message": "no device"}
        }))
        .unwrap();
        assert!(matches!(resp, RelayInbound::Response { id: 4, ok: false, .. }));
        let req = RelayOutbound::Request {
            id: 4,
            device: "emulator-5554".into(),
            verb: "tap".into(),
            args: json!({"x": 1, "y": 2}),
        };
        assert_eq!(
            serde_json::to_value(req).unwrap(),
            json!({"type": "request", "id": 4, "device": "emulator-5554", "verb": "tap", "args": {"x": 1, "y": 2}})
        );
        assert_eq!(
            serde_json::to_value(RelayOutbound::Error {
                code: DeviceErrorCode::DeviceRevoked
            })
            .unwrap(),
            json!({"type": "error", "code": "DEVICE_REVOKED"})
        );
    }
}
