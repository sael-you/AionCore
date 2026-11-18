//! Devices (C38 / TASK-136): every agent-reachable device on one page
//! (server browsers, the owner's own browsers, phones, the Mac, add-ons), the
//! device relay the Mac helper and the Android companion app connect to, and
//! the approvals and audit around them.
//!
//! Wire contracts W1 (REST + events) and W2 (relay WebSocket) of
//! `design/mobile/C38-ANDROID-ACCESS.md` §6, with the lane deltas agreed on
//! 2026-11-17 (ui cb2473cd, Mac helper 2268a071, Android 77160c5e): every W2
//! frame carries `type`; a first `pair` frame (like C34) next to `hello`; an
//! `inventory` frame lists what a relay client reaches.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Errors (ApiResponse codes and the relay `error` frame)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeviceErrorCode {
    /// No such device (or another user's). HTTP 404.
    DeviceNotFound,
    /// Pairing code unknown, used, expired or burned after wrong attempts.
    DevicePairingInvalid,
    /// The relay token was revoked (or never existed).
    DeviceRevoked,
    /// The device's agent switch is off or its timeout passed. HTTP 403.
    DeviceDisabled,
    /// The device (or the relay that reaches it) is not connected. HTTP 503.
    DeviceOffline,
    /// A verb agents may never use, or the peer refused it. HTTP 403.
    DeviceVerbRefused,
    /// The owner denied the action.
    DeviceApprovalDenied,
    /// Nobody answered the approval in time.
    DeviceApprovalTimeout,
    /// The approval is no longer pending. HTTP 410.
    DeviceApprovalExpired,
    /// A frame or request that does not parse / is out of range. HTTP 400.
    DeviceBadMessage,
    /// Too many calls (rate limit) or a job already running. HTTP 409 / 429.
    DeviceBusy,
    /// No such install/remove job. HTTP 404.
    DeviceJobNotFound,
    /// Remove refused: something still uses it ("switch the instance first"). HTTP 409.
    DeviceInUse,
    /// The relay socket refuses browser origins. HTTP 403.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAvailability {
    Now,
    /// Shown as a "later" card (Camoufox until C35 v2, the Mac until M3).
    Later,
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

/// `GET /api/devices` row; also the answer of `PUT /api/devices/{id}` and
/// `PUT /api/devices/{id}/routes/{route}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Stable and opaque: `server_browser:<engine>`, `my_browser:<dev_…>`,
    /// `phone:<id>`, `mac:<id>`, `addon:<name>`.
    pub id: String,
    pub kind: DeviceKind,
    /// The id in the owning subsystem (C34 `device_id`, engine id, relay
    /// phone serial); opaque to the UI.
    pub source_id: Option<String>,
    pub name: String,
    pub status: DeviceStatus,
    pub available: DeviceAvailability,
    /// The agent switch.
    pub enabled: bool,
    /// Epoch ms when the switch turns itself off (phones / Mac), else null.
    pub enabled_until: Option<i64>,
    pub installed: bool,
    pub disk_bytes: Option<u64>,
    pub version: Option<String>,
    /// The running install/remove job while `installing` / `removing`.
    pub job_id: Option<String>,
    pub last_seen_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<DeviceRoute>>,
    /// Add-ons: the device that runs them (a Mac helper id, or `server`).
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
    /// Phones / Mac: 15, 30 or 60 (required when enabling them). Ignored for
    /// other kinds.
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

/// `GET /api/devices/jobs/{job_id}`. A job running when AionCore restarted is
/// `failed`.
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
    /// Companion: what the app's pairing QR encodes (opaque to the UI):
    /// `aionui-pair:v1?host=<public host>&code=<CODE>`.
    pub qr_payload: Option<String>,
}

/// `GET /api/devices/approvals` row; also inside `devices.approval`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceApproval {
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    pub verb: String,
    /// What it would do, at most 200 characters.
    pub summary: String,
    /// Add-on runs: the full task text the owner approves.
    pub task: Option<String>,
    /// The app it acts on (crucial-package gate).
    pub package: Option<String>,
    /// Phone Chrome: the high-value origin.
    pub origin: Option<String>,
    pub conversation_id: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    /// When set, an allow also covers reads and actions on the same device
    /// and app for this many minutes ("allow for 5 min").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_minutes: Option<u32>,
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
    Refused,
    Approved,
    Denied,
    Timeout,
    Error,
}

/// `GET /api/devices/{id}/audit?cursor=` row, newest first. Never
/// screenshots, typed text or command output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAuditEntry {
    pub at: i64,
    pub verb: String,
    pub package: Option<String>,
    pub origin: Option<String>,
    pub outcome: DeviceAuditOutcome,
    pub conversation_id: Option<String>,
    /// A short reason (refusal or error code), never payload.
    pub detail: Option<String>,
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
    pub rows: Vec<DeviceAuditEntry>,
    pub next_cursor: Option<String>,
}

/// WS event names (existing user WebSocket, owning user only).
pub const EVENT_DEVICES_CHANGED: &str = "devices.changed";
pub const EVENT_DEVICES_APPROVAL: &str = "devices.approval";

/// Payload of `devices.changed`: the device that changed, or null = reload
/// the whole list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicesChangedEvent {
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceApprovalState {
    Requested,
    Resolved,
}

/// Payload of `devices.approval`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceApprovalEvent {
    pub approval: DeviceApproval,
    pub state: DeviceApprovalState,
    /// Set when `resolved`: true = allowed; false = denied or expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
}

// ---------------------------------------------------------------------------
// W2 · relay WebSocket `/api/devices/relay`
//
// - Non-browser clients only: any `Origin` header is refused. First frame
//   `pair` or `hello` within 5 s; the token is never in the URL.
// - Text frames only, JSON, at most `RELAY_MAX_FRAME_BYTES`; binary data
//   (screenshots) as base64 inside the JSON result.
// - `ping` every 20 s; AionCore closes a client silent for 60 s.
// - Phones are addressed by `serial` from `inventory`; the companion app uses
//   `self`; the Mac the helper runs on is `mac`.
// ---------------------------------------------------------------------------

pub const RELAY_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const RELAY_PING_SECS: u64 = 20;
pub const RELAY_IDLE_CLOSE_SECS: u64 = 60;
pub const RELAY_DEVICE_SELF: &str = "self";
pub const RELAY_DEVICE_MAC: &str = "mac";

/// `RelayError.code` values a peer answers with.
pub mod relay_error {
    /// The peer's local switch for that device is off.
    pub const SWITCH_OFF: &str = "switch_off";
    pub const UNKNOWN_DEVICE: &str = "unknown_device";
    /// The peer refuses this verb / these args.
    pub const REFUSED: &str = "refused";
    pub const TIMEOUT: &str = "timeout";
    pub const BUSY: &str = "busy";
    pub const BAD_ARGS: &str = "bad_args";
    /// adb / the OS call failed (the message says why).
    pub const FAILED: &str = "failed";
}

/// `RelayInbound::Event.name` values.
pub mod relay_event {
    /// The owner switched the device off locally (phone chip Stop, menu bar).
    pub const LOCAL_SWITCH_OFF: &str = "local_switch_off";
    /// data: `{package}`.
    pub const FOREGROUND: &str = "foreground";
    /// data: `{run_id, state: running|done|failed|stopped, message?, trace_url?}`.
    pub const ADDON_PROGRESS: &str = "addon_progress";
    /// data: `{session, message}`: a CDP frame from the phone's Chrome;
    /// `message` is the frame as a JSON-encoded string (exact bytes).
    pub const CDP_FRAME: &str = "cdp_frame";
    /// data: `{session}`: the phone's Chrome closed the session.
    pub const CDP_CLOSED: &str = "cdp_closed";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayClientKind {
    MacHelper,
    Companion,
}

/// A phone a relay client reaches (Mac helper: one per `adb devices` entry;
/// companion: exactly the phone it runs on, serial `self`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPhone {
    pub serial: String,
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub android_version: Option<String>,
    /// `usb` | `wifi` (Mac helper), `self` (companion).
    #[serde(default)]
    pub transport: Option<String>,
    /// The Chrome DevTools socket is reachable (Mac helper via `adb forward`).
    #[serde(default)]
    pub chrome: bool,
    /// The peer's own switch for this phone (defence in depth).
    #[serde(default)]
    pub enabled: bool,
    /// Mac helper: the `client_id` of the companion app installed on this
    /// phone (read over adb from the app's pairing provider), so AionCore
    /// shows ONE card for a phone reached both ways. Omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub companion_client_id: Option<String>,
}

/// The Mac the helper runs on (M3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayMac {
    pub name: String,
    #[serde(default)]
    pub os_version: Option<String>,
    /// Screen Recording + Accessibility granted to the helper.
    #[serde(default)]
    pub permissions_ok: bool,
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
    /// One of [`relay_error`].
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
    /// What the client reaches now: sent right after `welcome` and again on
    /// every change (phone attached / detached, switch, add-on installed).
    Inventory {
        #[serde(default)]
        phones: Vec<RelayPhone>,
        #[serde(default)]
        mac: Option<RelayMac>,
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
    /// `device` = phone serial, `self` or `mac`; `name` from [`relay_event`].
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
    Error {
        code: DeviceErrorCode,
    },
    Request {
        id: u64,
        device: String,
        /// One of [`RELAY_VERBS`].
        verb: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// The owner switched this device off / its timeout passed: stop at once
    /// (cancel running verbs and add-on runs on it).
    Stop {
        device: String,
    },
    Pong,
}

/// W2 verbs (args → result):
/// - phone: `screenshot{scale?}` → `{format: png|jpeg, width, height, data_base64}` ·
///   `ui_tree` → `{xml}` · `tap{x,y}` · `swipe{x1,y1,x2,y2,duration_ms}` ·
///   `text{value}` · `key{code}` · `launch{package}` · `foreground` → `{package}` ·
///   `install_apk{url, sha256}` · `logcat{package, lines}` → `{lines}` · `stop`
/// - phone Chrome (Mac helper only, over `adb forward localabstract:chrome_devtools_remote`):
///   `cdp_open` → `{session}` · `cdp_frame{session, message: string}` · `cdp_close{session}`;
///   Chrome → AionCore frames arrive as `cdp_frame` events.
/// - Mac (device `mac`): `mac_screenshot` · `mac_ax_tree` · `mac_click{x,y}` ·
///   `mac_type{value}` · `mac_key{code}` · `mac_open_app{bundle_id}`
/// - add-on host: `addon_install{name, version?}` · `addon_remove{name}` ·
///   `addon_run{name, task, device, max_minutes}` → `{run_id}` · `addon_stop{run_id}`
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
    "stop",
    "cdp_open",
    "cdp_frame",
    "cdp_close",
    "mac_foreground",
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
            source_id: None,
            name: "Pixel 9".into(),
            status: DeviceStatus::NeedsPairing,
            available: DeviceAvailability::Now,
            enabled: false,
            enabled_until: None,
            installed: true,
            disk_bytes: None,
            version: None,
            job_id: None,
            last_seen_at: None,
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
            json!({"id": "phone:p1", "kind": "phone", "source_id": null, "name": "Pixel 9",
                "status": "needs_pairing", "available": "now", "enabled": false, "enabled_until": null,
                "installed": true, "disk_bytes": null, "version": null, "job_id": null, "last_seen_at": null,
                "routes": [{"route": "wifi_direct", "enabled": false, "status": "off", "experimental": true}]})
        );
        assert!(serde_json::from_value::<DeviceUpdate>(json!({"enabled": true, "x": 1})).is_err());
        let approval = DeviceApproval {
            id: "a".into(),
            device_id: "phone:p1".into(),
            device_name: "Pixel 9".into(),
            verb: "launch".into(),
            summary: "launch com.bank".into(),
            task: None,
            package: Some("com.bank".into()),
            origin: None,
            conversation_id: None,
            created_at: 1,
            expires_at: 2,
            grant_minutes: Some(5),
        };
        let v = serde_json::to_value(DeviceApprovalEvent {
            approval,
            state: DeviceApprovalState::Resolved,
            approved: Some(false),
        })
        .unwrap();
        assert_eq!(v["state"], "resolved");
        assert_eq!(v["approved"], false);
        assert_eq!(v["approval"]["package"], "com.bank");
        assert!(v["approval"]["task"].is_null());
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
        assert!(matches!(
            hello,
            RelayInbound::Hello {
                kind: RelayClientKind::MacHelper,
                ..
            }
        ));
        let inv: RelayInbound = serde_json::from_value(json!({
            "type": "inventory",
            "phones": [{"serial": "R5CX", "name": "Galaxy", "transport": "usb", "chrome": true, "enabled": true}],
            "mac": {"name": "MacBook", "permissions_ok": false}
        }))
        .unwrap();
        assert!(matches!(inv, RelayInbound::Inventory { ref phones, mac: Some(_), .. } if phones[0].chrome));
        let resp: RelayInbound = serde_json::from_value(json!({
            "type": "response", "id": 4, "ok": false, "error": {"code": "switch_off", "message": "off"}
        }))
        .unwrap();
        assert!(matches!(resp, RelayInbound::Response { id: 4, ok: false, .. }));
        let req = RelayOutbound::Request {
            id: 4,
            device: "R5CX".into(),
            verb: "tap".into(),
            args: json!({"x": 1, "y": 2}),
        };
        assert_eq!(
            serde_json::to_value(req).unwrap(),
            json!({"type": "request", "id": 4, "device": "R5CX", "verb": "tap", "args": {"x": 1, "y": 2}})
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
