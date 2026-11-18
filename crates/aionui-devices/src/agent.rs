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
//!    possible); `install_apk` asks every time;
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
        let crucial =
            verb == "install_apk" || (gated && (!front_known || checked.iter().any(|p| is_crucial_package(p))));
        let mut approved = false;
        if crucial {
            let summary = match (verb.as_str(), package.as_deref()) {
                ("install_apk", _) => format!(
                    "install an app from {}",
                    args.get("url").and_then(Value::as_str).unwrap_or("a link")
                ),
                (v, Some(p)) => format!("{v} in {p}"),
                (v, None) => v.to_owned(),
            };
            // One OK covers this conversation + device + the apps involved for
            // 5 minutes. No grant when the front app is unknown (single use)
            // and none for installs.
            let grant_key = (verb != "install_apk" && front_known).then(|| {
                format!(
                    "{user_id}|{}|{device_id}|{}",
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
    /// `Ok(None)` when the device answers but cannot say (then the call is
    /// treated as crucial); `Err` when it is offline or does not answer
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
                .filter(|p| !p.trim().is_empty())),
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
