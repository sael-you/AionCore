//! `aioncore mcp-devices-stdio`: the built-in `aionui-devices` MCP server
//! (C38 W3). One tool per phone / Mac verb plus `device_list`.
//!
//! The bridge holds no logic and no authority: every tool call is one HTTP
//! request to AionCore's `/api/runtime/devices/*` with the conversation's
//! runtime token (env `AIONUI_BASE_URL`, `AIONUI_USER_ID`,
//! `AIONUI_CONVERSATION_ID`, `AIONUI_RUNTIME_TOKEN`, set in the MCP server
//! spec). AionCore decides: device switch and timeout, verb allowlist, rate
//! limit, owner approval on crucial apps, audit. A call may wait for the
//! owner (up to 2 minutes) or for an install (up to 16 minutes).

use std::process::ExitCode;
use std::time::Duration;

use crate::commands::error::{CliBoundaryCode, CliBoundaryError, missing_env};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::Deserialize;
use serde_json::{Value, json};

const SUBCOMMAND: &str = "mcp-devices-stdio";
const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";
/// Longest wait of one call: owner approval (2 min) + an add-on run (30 min
/// + 1) or install_apk (16 min).
const CALL_TIMEOUT: Duration = Duration::from_secs(34 * 60);

pub async fn run_devices_stdio() -> ExitCode {
    let server = match DevicesStdioServer::from_env() {
        Ok(server) => server,
        Err(err) => {
            eprintln!("{}", err.stderr_line());
            return err.exit_code();
        }
    };
    match server.serve(transport::io::stdio()).await {
        Ok(peer) => {
            if peer.waiting().await.is_err() {
                let err = CliBoundaryError::new(
                    CliBoundaryCode::McpSessionEndedWithError,
                    SUBCOMMAND,
                    "MCP stdio session ended with an error",
                );
                eprintln!("{}", err.stderr_line());
                err.exit_code()
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(_) => {
            let err = CliBoundaryError::new(
                CliBoundaryCode::McpStdioServeFailed,
                SUBCOMMAND,
                "failed to start MCP stdio server",
            );
            eprintln!("{}", err.stderr_line());
            err.exit_code()
        }
    }
}

#[derive(Clone)]
struct DevicesStdioServer {
    base_url: String,
    user_id: String,
    conversation_id: String,
    token: String,
    http: reqwest::Client,
}

impl DevicesStdioServer {
    fn from_env() -> Result<Self, CliBoundaryError> {
        let var = |name: &'static str| std::env::var(name).map_err(|_| missing_env(SUBCOMMAND, name));
        Ok(Self {
            base_url: var(ENV_BASE_URL)?.trim_end_matches('/').to_owned(),
            user_id: var(ENV_USER_ID)?,
            conversation_id: var(ENV_CONVERSATION_ID)?,
            token: var(ENV_RUNTIME_TOKEN)?,
            http: reqwest::Client::builder()
                .timeout(CALL_TIMEOUT)
                .build()
                .unwrap_or_default(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base_url))
            .header("x-aionui-user-id", &self.user_id)
            .header("x-aionui-conversation-id", &self.conversation_id)
            .header("x-aionui-runtime-token", &self.token)
    }

    /// The JSON body AionCore answered, or a transport error text.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, String> {
        let response = request.send().await.map_err(|_| "AionUi is not reachable".to_owned())?;
        response
            .json::<Value>()
            .await
            .map_err(|_| "AionUi sent an unreadable answer".to_owned())
    }

    async fn call(&self, device_id: String, verb: &str, args: Value) -> CallToolResult {
        let body = json!({"device_id": device_id, "verb": verb, "args": args});
        let answer = self
            .send(
                self.request(reqwest::Method::POST, "/api/runtime/devices/call")
                    .json(&body),
            )
            .await;
        tool_result(verb, answer)
    }
}

fn error_text(code: &str, message: &str) -> String {
    let hint = match code {
        "DEVICE_DISABLED" => " Ask the owner to switch this device on in Settings → Devices; do not retry.",
        "DEVICE_APPROVAL_DENIED" | "DEVICE_APPROVAL_TIMEOUT" => " Stop and ask the owner; never retry.",
        "DEVICE_OFFLINE" => " Tell the owner the device is offline; do not use another device instead.",
        "DEVICE_BUSY" => " Slow down.",
        _ => "",
    };
    format!("{code}: {message}{hint}")
}

/// Turn AionCore's `{ok, result | code + message}` into a tool result.
/// A screenshot (`data_base64`) becomes an image block.
fn tool_result(verb: &str, answer: Result<Value, String>) -> CallToolResult {
    let body = match answer {
        Ok(body) => body,
        Err(text) => return CallToolResult::error(vec![Content::text(text)]),
    };
    if body["ok"].as_bool() != Some(true) {
        let code = body["code"].as_str().unwrap_or("ERROR");
        let message = body["message"].as_str().unwrap_or("the device call failed");
        return CallToolResult::error(vec![Content::text(error_text(code, message))]);
    }
    let mut result = body.get("result").cloned().unwrap_or(Value::Null);
    if verb.ends_with("screenshot")
        && let Some(data) = result.get("data_base64").and_then(Value::as_str).map(str::to_owned)
    {
        let mime = if result["format"].as_str() == Some("jpeg") {
            "image/jpeg"
        } else {
            "image/png"
        };
        if let Some(map) = result.as_object_mut() {
            map.remove("data_base64");
        }
        return CallToolResult::success(vec![Content::image(data, mime), Content::text(result.to_string())]);
    }
    CallToolResult::success(vec![Content::text(result.to_string())])
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeviceOnly {
    /// The device_id from device_list.
    device_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenshotParams {
    /// The device_id from device_list.
    device_id: String,
    /// Downscale factor, 0 < scale <= 1 (default 1).
    #[serde(default)]
    scale: Option<f64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PhoneScreenshotParams {
    /// The device_id from device_list.
    device_id: String,
    /// Downscale factor, 0 < scale <= 1 (default 1).
    #[serde(default)]
    scale: Option<f64>,
    /// "png" or "jpeg" (default: png at scale 1, jpeg when scaled).
    #[serde(default)]
    format: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PhoneTextParams {
    device_id: String,
    /// Text typed into the focused field.
    value: String,
    /// true = replace what the field holds; false or absent = append.
    #[serde(default)]
    replace: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ClickTextParams {
    device_id: String,
    /// The exact visible text or content description of the element (1-500 characters).
    text: String,
    /// Which match to click when several elements carry that text, 0-99 (default 0).
    #[serde(default)]
    index: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetTextParams {
    device_id: String,
    /// The new content of the focused field (at most 10000 characters; may be empty).
    value: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobalParams {
    device_id: String,
    /// "notifications", "quick_settings" or "lock".
    action: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenUrlParams {
    device_id: String,
    /// An http, https or market link (at most 4096 characters, no blanks).
    url: String,
}

/// `{key: value}` pairs without the absent optional ones.
fn compact(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(
        pairs
            .into_iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PointParams {
    device_id: String,
    /// Pixels from the left edge.
    x: i64,
    /// Pixels from the top edge.
    y: i64,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SwipeParams {
    device_id: String,
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
    /// Duration in milliseconds (default 300).
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TextParams {
    device_id: String,
    /// Text typed into the focused field.
    value: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct KeyParams {
    device_id: String,
    /// Android KeyEvent code (3 HOME, 4 BACK, 66 ENTER, 187 APP_SWITCH) or a macOS virtual key code.
    code: i64,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct LaunchParams {
    device_id: String,
    /// Android package name, e.g. com.android.chrome.
    package: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct InstallApkParams {
    device_id: String,
    /// https URL of the APK.
    url: String,
    /// SHA-256 of the APK (64 hex characters); the install is refused when it does not match.
    sha256: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct LogcatParams {
    device_id: String,
    /// Only lines of this app.
    package: String,
    /// Number of latest lines (default 200).
    #[serde(default)]
    lines: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenAppParams {
    device_id: String,
    /// macOS bundle id, e.g. com.apple.Safari.
    bundle_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddonRunParams {
    /// The add-on's device_id from device_list (kind addon).
    addon_id: String,
    /// The phone it acts on: a device_id from device_list reached by the same Mac helper.
    phone_id: String,
    /// The whole task in plain words (1-4000 characters). The owner reads exactly this text before it starts.
    task: String,
    /// Time limit in minutes, 1-30 (default 10).
    #[serde(default)]
    max_minutes: Option<u32>,
}

#[tool_router]
impl DevicesStdioServer {
    #[tool(
        name = "addon_run_task",
        description = "Let an installed add-on (Artemis, Mobilerun) carry out a whole flow on the phone with its own model loop. The owner must approve the task text first, every time; the call returns when the run ended (done, failed or stopped) with a message and a trace link. Prefer the phone_* tools for single steps."
    )]
    async fn addon_run_task(&self, Parameters(p): Parameters<AddonRunParams>) -> CallToolResult {
        let body = json!({
            "addon_id": p.addon_id, "phone_id": p.phone_id, "task": p.task, "max_minutes": p.max_minutes,
        });
        let answer = self
            .send(
                self.request(reqwest::Method::POST, "/api/runtime/devices/addon-run")
                    .json(&body),
            )
            .await;
        tool_result("addon_run_task", answer)
    }

    #[tool(
        name = "device_list",
        description = "List the owner's phones and Macs you may use: device_id, name, status (on = usable now; off = the owner has not enabled it; offline), enabled_until and the verbs each supports. Call this first. A device that is off or offline cannot be used: tell the owner, do not retry."
    )]
    async fn device_list(&self) -> CallToolResult {
        let answer = self
            .send(self.request(reqwest::Method::GET, "/api/runtime/devices"))
            .await;
        match answer {
            Ok(body) if body["ok"].as_bool() == Some(true) => {
                CallToolResult::success(vec![Content::text(body["devices"].to_string())])
            }
            other => tool_result("device_list", other),
        }
    }

    #[tool(
        name = "phone_screenshot",
        description = "Screenshot of the phone. While a sensitive app is in front (banking, payments, messages, email, settings) it waits for the owner's OK; a password manager's screen is never returned."
    )]
    async fn phone_screenshot(&self, Parameters(p): Parameters<PhoneScreenshotParams>) -> CallToolResult {
        let args = compact(vec![("scale", json!(p.scale)), ("format", json!(p.format))]);
        self.call(p.device_id, "screenshot", args).await
    }

    #[tool(
        name = "phone_ui_tree",
        description = "The phone's current UI as uiautomator XML (text, resource-id, bounds of every node). Prefer it over screenshots to find what to tap."
    )]
    async fn phone_ui_tree(&self, Parameters(p): Parameters<DeviceOnly>) -> CallToolResult {
        self.call(p.device_id, "ui_tree", json!({})).await
    }

    #[tool(name = "phone_tap", description = "Tap the phone screen at (x, y) in pixels.")]
    async fn phone_tap(&self, Parameters(p): Parameters<PointParams>) -> CallToolResult {
        self.call(p.device_id, "tap", json!({"x": p.x, "y": p.y})).await
    }

    #[tool(name = "phone_swipe", description = "Swipe on the phone from (x1, y1) to (x2, y2).")]
    async fn phone_swipe(&self, Parameters(p): Parameters<SwipeParams>) -> CallToolResult {
        self.call(
            p.device_id,
            "swipe",
            json!({"x1": p.x1, "y1": p.y1, "x2": p.x2, "y2": p.y2, "duration_ms": p.duration_ms.unwrap_or(300)}),
        )
        .await
    }

    #[tool(
        name = "phone_text",
        description = "Type text into the focused field of the phone (appends; replace=true replaces the field's content, companion app 0.2+)."
    )]
    async fn phone_text(&self, Parameters(p): Parameters<PhoneTextParams>) -> CallToolResult {
        let args = compact(vec![("value", json!(p.value)), ("replace", json!(p.replace))]);
        self.call(p.device_id, "text", args).await
    }

    #[tool(
        name = "phone_click_text",
        description = "Click the element that shows this exact text (or has it as content description) on the phone; no coordinates needed. Returns how many elements matched. Needs the companion app 0.2+."
    )]
    async fn phone_click_text(&self, Parameters(p): Parameters<ClickTextParams>) -> CallToolResult {
        let args = compact(vec![("text", json!(p.text)), ("index", json!(p.index))]);
        self.call(p.device_id, "click_text", args).await
    }

    #[tool(
        name = "phone_set_text",
        description = "Replace the content of the focused text field on the phone. Fails when no editable field has focus. Needs the companion app 0.2+."
    )]
    async fn phone_set_text(&self, Parameters(p): Parameters<SetTextParams>) -> CallToolResult {
        self.call(p.device_id, "set_text", json!({"value": p.value})).await
    }

    #[tool(
        name = "phone_global",
        description = "A system action on the phone: \"notifications\" or \"quick_settings\" (both wait for the owner's OK) or \"lock\". Needs the companion app 0.2+."
    )]
    async fn phone_global(&self, Parameters(p): Parameters<GlobalParams>) -> CallToolResult {
        self.call(p.device_id, "global", json!({"action": p.action})).await
    }

    #[tool(
        name = "phone_open_url",
        description = "Open an http, https or market link on the phone in whichever app handles it. Always waits for the owner's OK; private and local addresses are refused. Needs the companion app 0.2+."
    )]
    async fn phone_open_url(&self, Parameters(p): Parameters<OpenUrlParams>) -> CallToolResult {
        self.call(p.device_id, "open_url", json!({"url": p.url})).await
    }

    #[tool(
        name = "phone_key",
        description = "Press a key on the phone by Android KeyEvent code (3 HOME, 4 BACK, 66 ENTER, 187 APP_SWITCH)."
    )]
    async fn phone_key(&self, Parameters(p): Parameters<KeyParams>) -> CallToolResult {
        self.call(p.device_id, "key", json!({"code": p.code})).await
    }

    #[tool(name = "phone_launch", description = "Open an app on the phone by package name.")]
    async fn phone_launch(&self, Parameters(p): Parameters<LaunchParams>) -> CallToolResult {
        self.call(p.device_id, "launch", json!({"package": p.package})).await
    }

    #[tool(
        name = "phone_foreground",
        description = "The package name of the app in front on the phone."
    )]
    async fn phone_foreground(&self, Parameters(p): Parameters<DeviceOnly>) -> CallToolResult {
        self.call(p.device_id, "foreground", json!({})).await
    }

    #[tool(
        name = "phone_install_apk",
        description = "Install a test APK on the phone from an https URL (sha256 required). Always waits for the owner's OK. Only through the Mac relay route."
    )]
    async fn phone_install_apk(&self, Parameters(p): Parameters<InstallApkParams>) -> CallToolResult {
        self.call(p.device_id, "install_apk", json!({"url": p.url, "sha256": p.sha256}))
            .await
    }

    #[tool(
        name = "phone_logcat",
        description = "The latest logcat lines of ONE app on the phone. Only through the Mac relay route."
    )]
    async fn phone_logcat(&self, Parameters(p): Parameters<LogcatParams>) -> CallToolResult {
        self.call(
            p.device_id,
            "logcat",
            json!({"package": p.package, "lines": p.lines.unwrap_or(200)}),
        )
        .await
    }

    #[tool(name = "mac_screenshot", description = "Screenshot of the owner's Mac.")]
    async fn mac_screenshot(&self, Parameters(p): Parameters<ScreenshotParams>) -> CallToolResult {
        self.call(p.device_id, "mac_screenshot", json!({"scale": p.scale}))
            .await
    }

    #[tool(
        name = "mac_ax_tree",
        description = "The Mac's accessibility tree of the front app (role, title, frame)."
    )]
    async fn mac_ax_tree(&self, Parameters(p): Parameters<DeviceOnly>) -> CallToolResult {
        self.call(p.device_id, "mac_ax_tree", json!({})).await
    }

    #[tool(name = "mac_click", description = "Click on the Mac at (x, y) in screen points.")]
    async fn mac_click(&self, Parameters(p): Parameters<PointParams>) -> CallToolResult {
        self.call(p.device_id, "mac_click", json!({"x": p.x, "y": p.y})).await
    }

    #[tool(name = "mac_type", description = "Type text into the focused field on the Mac.")]
    async fn mac_type(&self, Parameters(p): Parameters<TextParams>) -> CallToolResult {
        self.call(p.device_id, "mac_type", json!({"value": p.value})).await
    }

    #[tool(name = "mac_key", description = "Press a key on the Mac by virtual key code.")]
    async fn mac_key(&self, Parameters(p): Parameters<KeyParams>) -> CallToolResult {
        self.call(p.device_id, "mac_key", json!({"code": p.code})).await
    }

    #[tool(name = "mac_open_app", description = "Open an app on the Mac by bundle id.")]
    async fn mac_open_app(&self, Parameters(p): Parameters<OpenAppParams>) -> CallToolResult {
        self.call(p.device_id, "mac_open_app", json!({"bundle_id": p.bundle_id}))
            .await
    }
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for DevicesStdioServer {}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(result: &CallToolResult) -> String {
        serde_json::to_string(&result.content).unwrap_or_default()
    }

    #[test]
    fn errors_carry_the_code_and_tell_the_agent_not_to_retry() {
        let denied = tool_result(
            "tap",
            Ok(json!({"ok": false, "code": "DEVICE_APPROVAL_DENIED", "message": "the owner denied this action"})),
        );
        assert_eq!(denied.is_error, Some(true));
        let text = text_of(&denied);
        assert!(
            text.contains("DEVICE_APPROVAL_DENIED") && text.contains("never retry"),
            "{text}"
        );
        let off = text_of(&tool_result(
            "tap",
            Ok(json!({"ok": false, "code": "DEVICE_DISABLED", "message": "off"})),
        ));
        assert!(off.contains("Settings → Devices"), "{off}");
        let down = tool_result("tap", Err("AionUi is not reachable".into()));
        assert_eq!(down.is_error, Some(true));
    }

    #[test]
    fn screenshots_become_an_image_block_without_the_base64_in_text() {
        let shot = tool_result(
            "screenshot",
            Ok(json!({"ok": true, "result": {"format": "jpeg", "width": 10, "height": 20, "data_base64": "QUJD"}})),
        );
        assert_ne!(shot.is_error, Some(true));
        let text = text_of(&shot);
        assert!(text.contains("image/jpeg") && text.contains("QUJD"), "{text}");
        assert_eq!(
            text.matches("QUJD").count(),
            1,
            "the image data is not repeated as text"
        );
        let tree = text_of(&tool_result(
            "ui_tree",
            Ok(json!({"ok": true, "result": {"xml": "<hierarchy/>"}})),
        ));
        assert!(tree.contains("hierarchy"));
    }

    #[test]
    fn the_router_lists_one_tool_per_verb() {
        let names: Vec<String> = DevicesStdioServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for tool in [
            "device_list",
            "addon_run_task",
            "phone_screenshot",
            "phone_ui_tree",
            "phone_tap",
            "phone_swipe",
            "phone_text",
            "phone_key",
            "phone_launch",
            "phone_foreground",
            "phone_click_text",
            "phone_set_text",
            "phone_global",
            "phone_open_url",
            "phone_install_apk",
            "phone_logcat",
            "mac_screenshot",
            "mac_ax_tree",
            "mac_click",
            "mac_type",
            "mac_key",
            "mac_open_app",
        ] {
            assert!(names.iter().any(|n| n == tool), "{tool} missing from {names:?}");
        }
    }
}
