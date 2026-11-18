//! What an agent may do on a phone or the Mac (C38 §3, K3). Every call of the
//! `aionui-devices` MCP goes through [`DevicesService::agent_call`]:
//!
//! 1. only the constrained verbs (no raw shell, no `cdp_*`, no add-on verbs);
//! 2. the device's switch is on and its timeout has not passed;
//! 3. a per-device rate limit;
//! 4. **ask before crucial** (owner decisions 2026-11-17): while a crucial
//!    app is in front — banking / payment / wallet, messaging / email, app
//!    stores, system settings, password managers — every action (`tap`,
//!    `swipe`, `text`, `key`, `launch`, the `mac_*` input verbs) AND every
//!    read (`screenshot`, `ui_tree`, `logcat`, `mac_screenshot`,
//!    `mac_ax_tree`: the screen would go to a model provider) waits for the
//!    owner's OK. One OK covers reads and actions on the same device + app
//!    for 5 minutes. Reads of a password manager are always refused (no OK
//!    possible); `install_apk` asks every time; so does `open_url` (the app
//!    that opens the link is unknown; private / local addresses and AionUi
//!    itself are refused like in the browser), and `global` notifications /
//!    quick settings ask whatever is in front;
//!    The app in front is ALWAYS asked from the device (phone `foreground`,
//!    Mac `mac_foreground`), never taken from the agent's arguments; an app
//!    the call names (`launch`, `logcat`, `mac_open_app`) is checked in
//!    addition. Unknown front app = crucial, and that OK is single-use. The
//!    OK is per conversation; the front app is checked again right before
//!    the call runs;
//! 5. the relay call, and one audit row whatever the outcome (verb, app
//!    package, outcome; never screenshots, typed text or results).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use aionui_api_types::devices::{DeviceAuditOutcome, DeviceErrorCode};
use serde_json::Value;

use crate::approvals::OwnerQuestion;
use crate::error::DevicesError;
use crate::service::{AuditNote, DevicesService};

/// Verbs agents may call on a phone.
pub const PHONE_VERBS: &[&str] = &[
    "screenshot",
    "ui_tree",
    "tap",
    "swipe",
    "text",
    "key",
    "launch",
    "foreground",
    "click_text",
    "set_text",
    "global",
    "open_url",
    "install_apk",
    "logcat",
];
/// Verbs agents may call on the Mac.
pub const MAC_VERBS: &[&str] = &[
    "mac_screenshot",
    "mac_ax_tree",
    "mac_click",
    "mac_type",
    "mac_key",
    "mac_open_app",
];
/// Verbs that change something (the crucial gate applies to these).
const ACTING_VERBS: &[&str] = &[
    "tap",
    "swipe",
    "text",
    "key",
    "launch",
    "click_text",
    "set_text",
    "global",
    "open_url",
    "mac_click",
    "mac_type",
    "mac_key",
    "mac_open_app",
];
/// Verbs that show the screen or app data to the agent.
const READING_VERBS: &[&str] = &["screenshot", "ui_tree", "logcat", "mac_screenshot", "mac_ax_tree"];
/// Password managers and authenticators: their screens are never read.
const SECRET_STORE_MARKERS: &[&str] = &[
    "keychain",
    "password",
    "authenticator",
    "1password",
    "bitwarden",
    "lastpass",
    "keepass",
    "dashlane",
    "enpass",
    "nordpass",
    "protonpass",
];

/// Whether `package` is a password manager / authenticator.
pub fn is_secret_store(package: &str) -> bool {
    let p = package.to_ascii_lowercase();
    SECRET_STORE_MARKERS.iter().any(|m| p.contains(m))
}

/// Calls per device and minute.
pub const CALLS_PER_MINUTE: usize = 120;

/// Substrings of app packages / bundle ids that need the owner's OK.
const CRUCIAL_MARKERS: &[&str] = &[
    // banking, payment, wallets, trading
    "bank",
    "pay",
    "wallet",
    "revolut",
    "wise",
    "n26",
    "monzo",
    "stripe",
    "venmo",
    "cashapp",
    "klarna",
    "coinbase",
    "binance",
    "kraken",
    "crypto",
    "broker",
    "trading",
    "finanz",
    "sparkasse",
    // messaging and email
    "whatsapp",
    "telegram",
    "signal",
    "messenger",
    "messaging",
    "com.google.android.gm",
    "mail",
    "outlook",
    "sms",
    "mms",
    "discord",
    "slack",
    "viber",
    "wechat",
    "threema",
    "com.apple.mobilesms",
    "com.apple.ichat",
    // app stores and installers
    "com.android.vending",
    "packageinstaller",
    "appstore",
    "galaxystore",
    "aurora",
    // macOS: Mail and Messages match above; Terminal and shells
    "com.apple.terminal",
    "iterm",
    // system settings (uninstall, permissions, accounts)
    "com.android.settings",
    "com.apple.systempreferences",
    "com.apple.systemsettings",
    "permissioncontroller",
    "keychain",
    "password",
    "authenticator",
    "1password",
    "bitwarden",
];

/// System overlays that sit in front of the real app (a game toolbar, the
/// notification shade / lock screen): while the device names one of these,
/// the app underneath is NOT known, so the call is treated as "unknown app".
const OVERLAY_PACKAGES: &[&str] = &["com.oplus.games", "com.android.systemui"];

/// Whether `package` is an app where agent actions need an OK.
pub fn is_crucial_package(package: &str) -> bool {
    let p = package.to_ascii_lowercase();
    CRUCIAL_MARKERS.iter().any(|m| p.contains(m))
}

/// One agent call.
#[derive(Debug, Clone)]
pub struct AgentCall {
    pub user_id: String,
    pub conversation_id: Option<String>,
    pub device_id: String,
    pub verb: String,
    pub args: Value,
}

/// Who did what, for the audit row of one call.
struct Who<'a> {
    user_id: &'a str,
    device_id: &'a str,
    verb: &'a str,
    conversation_id: Option<&'a str>,
}

fn refused(message: &str) -> DevicesError {
    DevicesError::coded(DeviceErrorCode::DeviceVerbRefused, message)
}

/// `global` actions agents may ask for.
pub const GLOBAL_ACTIONS: &[&str] = &["notifications", "quick_settings", "lock"];
/// Schemes `open_url` takes (`market:` opens the app store).
const OPEN_URL_SCHEMES: &[&str] = &["http", "https", "market"];
const MAX_URL: usize = 4096;

/// Check the arguments of the companion 0.2 verbs before anything is asked
/// or sent (the app checks them again). The message says what is wrong and
/// never repeats the value.
fn check_args(verb: &str, args: &Value) -> Result<(), &'static str> {
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    match verb {
        "click_text" => {
            let len = text("text").map(|t| t.trim().chars().count()).unwrap_or(0);
            if !(1..=500).contains(&len) {
                return Err("text must have 1 to 500 characters");
            }
            match args.get("index") {
                None | Some(Value::Null) => Ok(()),
                Some(i) if i.as_u64().is_some_and(|i| i <= 99) => Ok(()),
                Some(_) => Err("index must be 0 to 99"),
            }
        }
        "set_text" => match text("value") {
            Some(v) if v.chars().count() <= 10_000 => Ok(()),
            _ => Err("value must be a string of at most 10000 characters"),
        },
        "text" => match args.get("replace") {
            None | Some(Value::Null | Value::Bool(_)) => Ok(()),
            Some(_) => Err("replace must be true or false"),
        },
        "screenshot" => match args.get("format") {
            None | Some(Value::Null) => Ok(()),
            Some(f) if matches!(f.as_str(), Some("png" | "jpeg")) => Ok(()),
            Some(_) => Err("format must be png or jpeg"),
        },
        "global" => match text("action") {
            Some(a) if GLOBAL_ACTIONS.contains(&a) => Ok(()),
            _ => Err("action must be notifications, quick_settings or lock"),
        },
        "open_url" => {
            let Some(url) = text("url") else {
                return Err("url is required");
            };
            let scheme = url.split_once(':').map(|(s, _)| s.to_ascii_lowercase());
            if url.len() > MAX_URL
                || url.chars().any(|c| c.is_whitespace() || c.is_control())
                || !scheme.is_some_and(|s| OPEN_URL_SCHEMES.contains(&s.as_str()))
            {
                return Err("url must be an http, https or market link without blanks (at most 4096 characters)");
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

impl DevicesService {
    fn rate_ok(&self, device_id: &str) -> bool {
        let mut windows = self.rate_windows().lock().expect("rate lock");
        let window: &mut VecDeque<Instant> = windows.entry(device_id.to_owned()).or_default();
        let now = Instant::now();
        while window
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            window.pop_front();
        }
        if window.len() >= CALLS_PER_MINUTE {
            return false;
        }
        window.push_back(now);
        true
    }

    async fn audit_call(
        &self,
        who: &Who<'_>,
        outcome: DeviceAuditOutcome,
        package: Option<&str>,
        detail: Option<&str>,
    ) {
        self.record(
            who.user_id,
            who.device_id,
            AuditNote {
                verb: who.verb,
                package,
                origin: None,
                outcome,
                conversation_id: who.conversation_id,
                detail,
            },
        )
        .await;
    }

    /// Run one agent call through every gate; see the module docs.
    pub async fn agent_call(&self, call: AgentCall) -> Result<Value, DevicesError> {
        let AgentCall {
            user_id,
            conversation_id,
            device_id,
            verb,
            args,
        } = call;
        let row = self
            .repo()
            .get_device(&user_id, &device_id)
            .await?
            .ok_or_else(DevicesError::not_found)?;
        let allowed = match row.kind.as_str() {
            "phone" => PHONE_VERBS.contains(&verb.as_str()),
            "mac" => MAC_VERBS.contains(&verb.as_str()),
            _ => false,
        };
        let who = Who {
            user_id: &user_id,
            device_id: &device_id,
            verb: &verb,
            conversation_id: conversation_id.as_deref(),
        };
        if !allowed {
            self.audit_call(&who, DeviceAuditOutcome::Refused, None, Some("DEVICE_VERB_REFUSED"))
                .await;
            return Err(refused("this action is not available to agents"));
        }
        if !self.is_enabled(&user_id, &device_id).await? {
            self.audit_call(&who, DeviceAuditOutcome::Refused, None, Some("DEVICE_DISABLED"))
                .await;
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceDisabled,
                "the owner has not enabled this device for agents",
            ));
        }
        if !self.rate_ok(&device_id) {
            self.audit_call(&who, DeviceAuditOutcome::Refused, None, Some("DEVICE_BUSY"))
                .await;
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceBusy,
                "too many actions on this device, slow down",
            ));
        }

        if let Err(message) = check_args(&verb, &args) {
            self.audit_call(&who, DeviceAuditOutcome::Refused, None, Some("DEVICE_BAD_MESSAGE"))
                .await;
            return Err(DevicesError::coded(DeviceErrorCode::DeviceBadMessage, message));
        }
        // open_url: the browser's navigation guard applies to the phone too
        // (AionUi's own origin, loopback, private and link-local addresses).
        let opened_url =
            (verb == "open_url").then(|| args.get("url").and_then(Value::as_str).unwrap_or_default().to_owned());
        if let Some(url) = &opened_url
            && !url.to_ascii_lowercase().starts_with("market:")
            && self.browser_port().url_blocked(url)
        {
            self.audit_call(&who, DeviceAuditOutcome::Refused, None, Some("url_blocked"))
                .await;
            return Err(refused("this address is not allowed (private, local or AionUi itself)"));
        }
        // Verbs that are crucial whatever is in front: `open_url` (the app
        // that opens the link is unknown) and the notification shade / quick
        // settings (other apps' messages, system switches).
        let always_crucial = opened_url.is_some()
            || (verb == "global"
                && matches!(
                    args.get("action").and_then(Value::as_str),
                    Some("notifications" | "quick_settings")
                ));

        // Which app is REALLY in front? Always asked from the device, on
        // every gated call: a package / bundle id in the agent's arguments is
        // never trusted for the gate (review part 2: an agent could name a
        // harmless app while a bank or a password manager is in front).
        let acting = ACTING_VERBS.contains(&verb.as_str());
        let reading = READING_VERBS.contains(&verb.as_str());
        let gated = acting || reading || verb == "install_apk";
        let front = if gated {
            match self.front_app(&user_id, &device_id, &row.kind).await {
                Ok(front) => front,
                Err(error) => return Err(self.unreachable(&who, error).await),
            }
        } else {
            None
        };
        // Apps the call names itself (launch / open an app, logcat of an app):
        // checked IN ADDITION to the front app.
        let named: Option<String> = match verb.as_str() {
            "launch" | "logcat" => args.get("package").and_then(Value::as_str).map(str::to_owned),
            "mac_open_app" => args.get("bundle_id").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        };
        let front_known = front.is_some();
        // For the audit row and the prompt: the app acted on.
        let package = named
            .clone()
            .or_else(|| front.clone())
            .or_else(|| gated.then(|| "unknown".to_owned()));
        let checked: Vec<&str> = front.as_deref().into_iter().chain(named.as_deref()).collect();

        if reading && checked.iter().any(|p| is_secret_store(p)) {
            self.audit_call(
                &who,
                DeviceAuditOutcome::Refused,
                package.as_deref(),
                Some("secret_store"),
            )
            .await;
            return Err(refused("a password manager is in front: its screen is never read"));
        }
        let crucial = verb == "install_apk"
            || always_crucial
            || (gated && (!front_known || checked.iter().any(|p| is_crucial_package(p))));
        let mut approved = false;
        if crucial {
            let summary = match (verb.as_str(), package.as_deref()) {
                ("install_apk", _) => format!(
                    "install an app from {}",
                    args.get("url").and_then(Value::as_str).unwrap_or("a link")
                ),
                ("open_url", _) => format!(
                    "open {} on the phone",
                    opened_url
                        .as_deref()
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect::<String>()
                ),
                ("global", Some(p)) if always_crucial => format!(
                    "open the {} (in front: {p})",
                    if args.get("action").and_then(Value::as_str) == Some("notifications") {
                        "notifications"
                    } else {
                        "quick settings"
                    }
                ),
                (v, Some(p)) => format!("{v} in {p}"),
                (v, None) => v.to_owned(),
            };
            // One OK covers this conversation + device + the apps involved for
            // 5 minutes. No grant when the front app is unknown (single use)
            // and none for installs.
            // `open_url` asks every time (each link is its own decision);
            // the shade / quick settings get their own grant.
            let scope = if always_crucial { "|global" } else { "" };
            let grant_key = (verb != "install_apk" && opened_url.is_none() && front_known).then(|| {
                format!(
                    "{user_id}|{}|{device_id}|{}{scope}",
                    conversation_id.as_deref().unwrap_or_default(),
                    checked.join("+")
                )
            });
            let question = OwnerQuestion {
                user_id: user_id.clone(),
                device_id: device_id.clone(),
                device_name: row.name.clone(),
                verb: verb.clone(),
                summary,
                task: None,
                package: package.clone(),
                origin: None,
                conversation_id: conversation_id.clone(),
                grant_key,
            };
            if let Err(error) = self.ask_owner(question).await {
                let outcome = if error.code() == Some(DeviceErrorCode::DeviceApprovalTimeout) {
                    DeviceAuditOutcome::Timeout
                } else {
                    DeviceAuditOutcome::Denied
                };
                self.audit_call(&who, outcome, package.as_deref(), None).await;
                return Err(error);
            }
            approved = true;
            // The switch may have gone off while the owner was asked.
            if !self.is_enabled(&user_id, &device_id).await? {
                self.audit_call(
                    &who,
                    DeviceAuditOutcome::Refused,
                    package.as_deref(),
                    Some("DEVICE_DISABLED"),
                )
                .await;
                return Err(DevicesError::coded(
                    DeviceErrorCode::DeviceDisabled,
                    "the device was switched off",
                ));
            }
        }
        // Re-check right before executing: the owner's OK (or the "not
        // crucial" verdict) was for THAT front app. If another app came to
        // the front meanwhile, nothing runs; the agent looks again.
        if gated && verb != "install_apk" {
            let now_front = match self.front_app(&user_id, &device_id, &row.kind).await {
                Ok(front) => front,
                Err(error) => return Err(self.unreachable(&who, error).await),
            };
            if now_front != front {
                self.audit_call(
                    &who,
                    DeviceAuditOutcome::Refused,
                    now_front.as_deref().or(Some("unknown")),
                    Some("front_app_changed"),
                )
                .await;
                return Err(refused("the app in front changed; look again and retry"));
            }
        }
        match self.routed_call(&user_id, &device_id, &verb, args).await {
            Ok(result) => {
                let outcome = if approved {
                    DeviceAuditOutcome::Approved
                } else {
                    DeviceAuditOutcome::Ok
                };
                self.audit_call(&who, outcome, package.as_deref(), None).await;
                Ok(result)
            }
            Err(error) => {
                let code = error
                    .code()
                    .and_then(|c| serde_json::to_value(c).ok())
                    .and_then(|v| v.as_str().map(str::to_owned));
                self.audit_call(&who, DeviceAuditOutcome::Error, package.as_deref(), code.as_deref())
                    .await;
                Err(error)
            }
        }
    }

    /// The device is not connected or did not answer: one audit row, and the
    /// coded error goes back at once (the owner is never asked about a device
    /// that cannot be reached).
    async fn unreachable(&self, who: &Who<'_>, error: DevicesError) -> DevicesError {
        self.audit_call(who, DeviceAuditOutcome::Error, None, Some("DEVICE_OFFLINE"))
            .await;
        error
    }

    /// The app in front on the device, asked from the device itself: Android
    /// package (`foreground`) or macOS bundle id (`mac_foreground`).
    /// `Ok(None)` when the device answers but cannot say, or names a system
    /// overlay (then the call is treated as crucial); `Err` when it is offline or does not answer
    /// within [`crate::relay::QUICK_TIMEOUT`].
    async fn front_app(&self, user_id: &str, device_id: &str, kind: &str) -> Result<Option<String>, DevicesError> {
        let (verb, field) = if kind == "mac" {
            ("mac_foreground", "bundle_id")
        } else {
            ("foreground", "package")
        };
        match self.routed_call(user_id, device_id, verb, serde_json::json!({})).await {
            Ok(v) => Ok(v
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|p| !p.trim().is_empty() && !OVERLAY_PACKAGES.contains(&p.as_str()))),
            Err(error) if error.code() == Some(DeviceErrorCode::DeviceOffline) => Err(error),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crucial_apps_are_recognised() {
        for p in [
            "com.whatsapp",
            "com.google.android.gm",
            "com.android.vending",
            "com.paypal.android.p2pmobile",
            "de.sparkasse.app",
            "com.revolut.revolut",
            "com.android.settings",
            "com.google.android.apps.messaging",
            "unknown.Bank.App",
        ] {
            assert!(is_crucial_package(p), "{p}");
        }
        for p in [
            "com.android.chrome",
            "com.example.myapp",
            "org.wikipedia",
            "com.google.android.youtube",
        ] {
            assert!(!is_crucial_package(p), "{p}");
        }
        for p in [
            "com.x8bit.bitwarden",
            "com.1password.1password",
            "com.google.android.apps.authenticator2",
        ] {
            assert!(is_secret_store(p) && is_crucial_package(p), "{p}");
        }
        assert!(!is_secret_store("com.whatsapp"));
        assert!(!PHONE_VERBS.contains(&"cdp_frame") && !PHONE_VERBS.contains(&"addon_run"));
        assert!(!PHONE_VERBS.contains(&"stop"));
    }
}
