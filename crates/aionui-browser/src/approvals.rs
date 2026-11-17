//! High-value-origin pause (C34 owner decision (a), TASK-125): in the owner's
//! own browser, on a tab where ANY frame is on a high-value origin (Google
//! account / mail / pay, PayPal, Apple ID, Microsoft account, `*.bank`, …),
//! every CDP method waits for the owner's OK in AionUi except a read-only
//! allowlist ([`is_read_only_method`]: screenshots, DOM / accessibility / CSS
//! getters, `*.enable` / `*.disable`). Default deny (train-9 review): script
//! can run through many methods (`Runtime.compileScript` + `runScript`,
//! `Page.addScriptToEvaluateOnNewDocument`, `Debugger.evaluateOnCallFrame`,
//! `DOM.setOuterHTML` …), so a list of gated methods could never be complete.
//!
//! - A pending approval waits [`APPROVAL_TIMEOUT`] (120 s); no answer refuses
//!   the command with `BROWSER_APPROVAL_TIMEOUT`.
//! - An approve unlocks THAT device + tab + origin for [`GRANT_TTL`] (60 s),
//!   so one OK covers a burst (typing sends one `Input.dispatchKeyEvent` per
//!   key). A deny refuses only that command (`BROWSER_APPROVAL_DENIED`).
//! - Events go to the owning user only: `browser.approvalRequested` (the
//!   approval row) and `browser.approvalResolved` `{approval_id, approved}`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aionui_api_types::{BrowserApproval, BrowserApprovalResolved, BrowserErrorCode};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::devices::now_ms;
use crate::error::BrowserError;

pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
pub const GRANT_TTL: Duration = Duration::from_secs(60);
pub const EVENT_APPROVAL_REQUESTED: &str = "browser.approvalRequested";
pub const EVENT_APPROVAL_RESOLVED: &str = "browser.approvalResolved";
pub const EVENT_DEVICES_CHANGED: &str = "browser.devicesChanged";
const SUMMARY_CHARS: usize = 120;

/// Exact hosts (and their subdomains) that pause agent input.
const HIGH_VALUE_HOSTS: &[&str] = &[
    "accounts.google.com",
    "myaccount.google.com",
    "mail.google.com",
    "pay.google.com",
    "payments.google.com",
    "admin.google.com",
    "paypal.com",
    "appleid.apple.com",
    "account.apple.com",
    "icloud.com",
    "login.microsoftonline.com",
    "login.live.com",
    "account.microsoft.com",
    "account.live.com",
    "outlook.live.com",
    "dashboard.stripe.com",
    "wise.com",
    "revolut.com",
    "n26.com",
];

/// Top-level domains that are banks by definition.
const HIGH_VALUE_TLDS: &[&str] = &["bank"];

/// Where approval events go (the app sends them to the user's own sockets).
pub trait BrowserEventSink: Send + Sync {
    fn emit(&self, user_id: &str, name: &str, data: Value);
}

/// Drops every event (tests, or before the app wires the bus).
pub struct NoopEventSink;

impl BrowserEventSink for NoopEventSink {
    fn emit(&self, _: &str, _: &str, _: Value) {}
}

/// Read-only methods that pass on a high-value origin without asking. They
/// observe the page (or switch event domains on and off) and cannot run
/// script, change the DOM, type, click or navigate.
const READ_ONLY_METHODS: &[&str] = &[
    "Page.captureScreenshot",
    "Page.getNavigationHistory",
    "Page.getLayoutMetrics",
    "Page.getFrameTree",
    "Page.setLifecycleEventsEnabled",
    "DOM.getDocument",
    "DOM.getFlattenedDocument",
    "DOM.querySelector",
    "DOM.querySelectorAll",
    "DOM.describeNode",
    "DOM.getBoxModel",
    "DOM.getContentQuads",
    "DOM.getOuterHTML",
    "DOM.getAttributes",
    "DOM.requestChildNodes",
    "DOM.getNodeForLocation",
    "Accessibility.getFullAXTree",
    "Accessibility.getPartialAXTree",
    "Accessibility.queryAXTree",
    "Accessibility.getRootAXNode",
    "Accessibility.getChildAXNodes",
    "Accessibility.getAXNodeAndAncestors",
    "CSS.getComputedStyleForNode",
    "CSS.getMatchedStylesForNode",
    "CSS.getInlineStylesForNode",
    "Runtime.getProperties",
    "Runtime.releaseObject",
    "Runtime.releaseObjectGroup",
    "Runtime.runIfWaitingForDebugger",
    "Target.getTargets",
    "Target.getTargetInfo",
];

/// Whether `method` may run on a high-value origin without the owner's OK.
/// Everything else (including methods unknown today) waits for it.
pub fn is_read_only_method(method: &str) -> bool {
    method.ends_with(".enable") || method.ends_with(".disable") || READ_ONLY_METHODS.contains(&method)
}

/// The origin (`scheme://host[:port]`) of a URL, lower-case.
pub fn origin_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    Some(match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    })
}

pub fn is_high_value_url(url: &str) -> bool {
    let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase()))
    else {
        return false;
    };
    HIGH_VALUE_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
        || HIGH_VALUE_TLDS.iter().any(|t| host.rsplit('.').next() == Some(*t))
}

/// `Fetch.enable` patterns that pause top-level navigations to a high-value
/// host, so the relay learns the tab's new origin before the page commits.
pub fn high_value_fetch_patterns() -> Vec<Value> {
    HIGH_VALUE_HOSTS
        .iter()
        .flat_map(|h| [format!("*://{h}/*"), format!("*://*.{h}/*")])
        .chain(HIGH_VALUE_TLDS.iter().map(|t| format!("*://*.{t}/*")))
        .map(|p| serde_json::json!({"urlPattern": p, "resourceType": "Document", "requestStage": "Request"}))
        .collect()
}

/// The method plus at most 120 characters of what it would type or run.
pub fn summarize(method: &str, params: &Value) -> String {
    let detail = params
        .get("expression")
        .or_else(|| params.get("functionDeclaration"))
        .or_else(|| params.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let detail: String = detail.chars().take(SUMMARY_CHARS).collect();
    if detail.is_empty() {
        method.to_owned()
    } else {
        format!("{method}: {detail}")
    }
}

/// The question for one command.
#[derive(Debug, Clone)]
pub struct ApprovalAsk {
    pub user_id: String,
    pub device_id: String,
    pub device_name: String,
    pub tab_id: i64,
    pub url: String,
    pub method: String,
    pub summary: String,
}

/// How a gated command went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// Not a high-value origin, or not a gated method.
    NotNeeded,
    /// A recent OK for this device + tab + origin covers it.
    Granted,
    /// The owner said yes just now.
    Approved,
}

struct Pending {
    user_id: String,
    row: BrowserApproval,
    reply: oneshot::Sender<bool>,
}

type GrantKey = (String, String, i64, String);

pub struct ApprovalBroker {
    pending: std::sync::Mutex<HashMap<String, Pending>>,
    grants: std::sync::Mutex<HashMap<GrantKey, Instant>>,
    sink: std::sync::RwLock<Arc<dyn BrowserEventSink>>,
    timeout: Duration,
    grant_ttl: Duration,
}

impl Default for ApprovalBroker {
    fn default() -> Self {
        Self::new(APPROVAL_TIMEOUT, GRANT_TTL)
    }
}

impl ApprovalBroker {
    pub fn new(timeout: Duration, grant_ttl: Duration) -> Self {
        Self {
            pending: std::sync::Mutex::new(HashMap::new()),
            grants: std::sync::Mutex::new(HashMap::new()),
            sink: std::sync::RwLock::new(Arc::new(NoopEventSink)),
            timeout,
            grant_ttl,
        }
    }

    pub fn set_sink(&self, sink: Arc<dyn BrowserEventSink>) {
        *self.sink.write().expect("sink lock") = sink;
    }

    pub fn emit(&self, user_id: &str, name: &str, data: Value) {
        let sink = self.sink.read().expect("sink lock").clone();
        sink.emit(user_id, name, data);
    }

    /// Whether `ask` needs the owner, without asking.
    pub fn needs_owner(ask_method: &str, url: &str) -> bool {
        !is_read_only_method(ask_method) && is_high_value_url(url)
    }

    /// Wait for the owner when needed. `on_pause` runs once when the command
    /// is actually paused (for the activity log).
    pub async fn gate(&self, ask: ApprovalAsk, on_pause: impl FnOnce()) -> Result<GateOutcome, BrowserError> {
        if !Self::needs_owner(&ask.method, &ask.url) {
            return Ok(GateOutcome::NotNeeded);
        }
        let origin = origin_of(&ask.url).unwrap_or_default();
        let key: GrantKey = (ask.user_id.clone(), ask.device_id.clone(), ask.tab_id, origin.clone());
        {
            let mut grants = self.grants.lock().expect("grants lock");
            let now = Instant::now();
            grants.retain(|_, until| *until > now);
            if grants.contains_key(&key) {
                return Ok(GateOutcome::Granted);
            }
        }
        on_pause();
        let approval_id = format!("apr_{}", uuid::Uuid::new_v4().simple());
        let requested_at = now_ms();
        let row = BrowserApproval {
            approval_id: approval_id.clone(),
            device_id: ask.device_id.clone(),
            device_name: ask.device_name.clone(),
            tab_id: ask.tab_id,
            origin,
            url: ask.url.clone(),
            method: ask.method.clone(),
            summary: ask.summary.clone(),
            requested_at,
            expires_at: requested_at + i64::try_from(self.timeout.as_millis()).unwrap_or(0),
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(
            approval_id.clone(),
            Pending {
                user_id: ask.user_id.clone(),
                row: row.clone(),
                reply: tx,
            },
        );
        tracing::info!(user_id = %ask.user_id, device_id = %ask.device_id, %approval_id, method = %ask.method, "browser: agent action paused for the owner");
        self.emit(
            &ask.user_id,
            EVENT_APPROVAL_REQUESTED,
            serde_json::to_value(&row).unwrap_or(Value::Null),
        );
        let answer = tokio::time::timeout(self.timeout, rx).await;
        let approved = match answer {
            Ok(Ok(yes)) => Some(yes),
            // Timeout, or the broker dropped the request.
            _ => None,
        };
        if approved.is_none() {
            // Still pending: it expired. Tell the UI to drop the card.
            if self
                .pending
                .lock()
                .expect("pending lock")
                .remove(&approval_id)
                .is_some()
            {
                self.resolved(&ask.user_id, &approval_id, false);
            }
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserApprovalTimeout,
                "the owner did not answer in time",
            ));
        }
        if approved == Some(true) {
            self.grants
                .lock()
                .expect("grants lock")
                .insert(key, Instant::now() + self.grant_ttl);
            Ok(GateOutcome::Approved)
        } else {
            Err(BrowserError::coded(
                BrowserErrorCode::BrowserApprovalDenied,
                "the owner denied this action",
            ))
        }
    }

    fn resolved(&self, user_id: &str, approval_id: &str, approved: bool) {
        self.emit(
            user_id,
            EVENT_APPROVAL_RESOLVED,
            serde_json::to_value(BrowserApprovalResolved {
                approval_id: approval_id.to_owned(),
                approved,
            })
            .unwrap_or(Value::Null),
        );
    }

    /// The user's pending approvals, oldest first.
    pub fn list(&self, user_id: &str) -> Vec<BrowserApproval> {
        let mut rows: Vec<BrowserApproval> = self
            .pending
            .lock()
            .expect("pending lock")
            .values()
            .filter(|p| p.user_id == user_id)
            .map(|p| p.row.clone())
            .collect();
        rows.sort_by_key(|r| r.requested_at);
        rows
    }

    /// The owner's answer. Unknown, answered, expired or another user's
    /// approval → `BROWSER_APPROVAL_EXPIRED` (no existence leak).
    pub fn decide(&self, user_id: &str, approval_id: &str, approve: bool) -> Result<(), BrowserError> {
        let pending = {
            let mut map = self.pending.lock().expect("pending lock");
            match map.get(approval_id) {
                Some(p) if p.user_id == user_id => map.remove(approval_id),
                _ => None,
            }
        };
        let Some(p) = pending else {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserApprovalExpired,
                "the approval is no longer pending",
            ));
        };
        let _ = p.reply.send(approve);
        tracing::info!(user_id, approval_id, approve, "browser: owner answered an approval");
        self.resolved(user_id, approval_id, approve);
        Ok(())
    }

    /// Drop the pending approvals and grants of a device (revoke / offline).
    pub fn forget_device(&self, user_id: &str, device_id: &str) {
        let dropped: Vec<String> = {
            let mut map = self.pending.lock().expect("pending lock");
            let ids: Vec<String> = map
                .iter()
                .filter(|(_, p)| p.user_id == user_id && p.row.device_id == device_id)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &ids {
                map.remove(id);
            }
            ids
        };
        for id in dropped {
            self.resolved(user_id, &id, false);
        }
        self.grants
            .lock()
            .expect("grants lock")
            .retain(|(u, d, _, _), _| !(u == user_id && d == device_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<(String, String, Value)>>);

    impl BrowserEventSink for Recorder {
        fn emit(&self, user_id: &str, name: &str, data: Value) {
            self.0.lock().unwrap().push((user_id.into(), name.into(), data));
        }
    }

    fn ask(url: &str, method: &str) -> ApprovalAsk {
        ApprovalAsk {
            user_id: "u1".into(),
            device_id: "dev_1".into(),
            device_name: "Mac".into(),
            tab_id: 7,
            url: url.into(),
            method: method.into(),
            summary: method.into(),
        }
    }

    #[test]
    fn high_value_hosts_and_gated_methods() {
        assert!(is_high_value_url("https://accounts.google.com/signin"));
        assert!(is_high_value_url("https://www.paypal.com/x"));
        assert!(is_high_value_url("https://online.example.bank/"));
        assert!(!is_high_value_url("https://google.com/"));
        assert!(!is_high_value_url("https://notpaypal.com/"));
        assert!(!is_read_only_method("Input.dispatchKeyEvent"));
        assert!(is_read_only_method("Page.captureScreenshot"));
        assert_eq!(
            summarize("Runtime.evaluate", &serde_json::json!({"expression": "x".repeat(300)})).len(),
            "Runtime.evaluate: ".len() + 120
        );
    }

    #[tokio::test]
    async fn approve_grants_a_window_deny_refuses_once_and_events_are_per_user() {
        let broker = Arc::new(ApprovalBroker::new(Duration::from_secs(5), Duration::from_secs(60)));
        let rec = Arc::new(Recorder::default());
        broker.set_sink(rec.clone());
        assert_eq!(
            broker
                .gate(ask("https://example.com/", "Input.dispatchKeyEvent"), || {})
                .await
                .unwrap(),
            GateOutcome::NotNeeded
        );
        let b = broker.clone();
        let waiting = tokio::spawn(async move {
            b.gate(ask("https://mail.google.com/x", "Input.insertText"), || {})
                .await
        });
        let id = loop {
            if let Some(r) = broker.list("u1").first() {
                break r.approval_id.clone();
            }
            tokio::task::yield_now().await;
        };
        assert!(broker.list("u2").is_empty(), "other users see nothing");
        assert_eq!(
            broker.decide("u2", &id, true).unwrap_err().code(),
            Some(BrowserErrorCode::BrowserApprovalExpired)
        );
        broker.decide("u1", &id, true).unwrap();
        assert_eq!(waiting.await.unwrap().unwrap(), GateOutcome::Approved);
        // Same device + tab + origin: covered by the grant.
        assert_eq!(
            broker
                .gate(ask("https://mail.google.com/y", "Input.dispatchKeyEvent"), || {})
                .await
                .unwrap(),
            GateOutcome::Granted
        );
        assert_eq!(
            broker.decide("u1", &id, true).unwrap_err().code(),
            Some(BrowserErrorCode::BrowserApprovalExpired),
            "answered twice"
        );
        // Another origin asks again; a deny refuses.
        let b = broker.clone();
        let waiting = tokio::spawn(async move {
            b.gate(ask("https://accounts.google.com/", "Runtime.evaluate"), || {})
                .await
        });
        let id2 = loop {
            if let Some(r) = broker.list("u1").first() {
                break r.approval_id.clone();
            }
            tokio::task::yield_now().await;
        };
        broker.decide("u1", &id2, false).unwrap();
        assert_eq!(
            waiting.await.unwrap().unwrap_err().code(),
            Some(BrowserErrorCode::BrowserApprovalDenied)
        );
        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().all(|(u, _, _)| u == "u1"));
        let names: Vec<&str> = events.iter().map(|(_, n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                EVENT_APPROVAL_REQUESTED,
                EVENT_APPROVAL_RESOLVED,
                EVENT_APPROVAL_REQUESTED,
                EVENT_APPROVAL_RESOLVED
            ]
        );
        assert_eq!(events[1].2, serde_json::json!({"approval_id": id, "approved": true}));
        assert_eq!(events[0].2["origin"], "https://mail.google.com");
    }

    #[tokio::test]
    async fn no_answer_times_out_and_clears_the_card() {
        let broker = ApprovalBroker::new(Duration::from_millis(50), Duration::from_secs(60));
        let rec = Arc::new(Recorder::default());
        broker.set_sink(rec.clone());
        let mut paused = false;
        let err = broker
            .gate(ask("https://pay.google.com/", "Input.dispatchMouseEvent"), || {
                paused = true
            })
            .await
            .unwrap_err();
        assert!(paused);
        assert_eq!(err.code(), Some(BrowserErrorCode::BrowserApprovalTimeout));
        assert!(broker.list("u1").is_empty());
        let events = rec.0.lock().unwrap().clone();
        assert_eq!(events.last().unwrap().1, EVENT_APPROVAL_RESOLVED);
        assert_eq!(events.last().unwrap().2["approved"], false);
    }

    /// Train-9 review fixture: the listed script / DOM-changing methods pause,
    /// the read-only allowlist passes (default deny on high-value origins).
    #[test]
    fn high_value_gate_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/high_value_gate.cases.json")).unwrap();
        for m in fixture["gated"].as_array().unwrap() {
            let m = m.as_str().unwrap();
            assert!(
                ApprovalBroker::needs_owner(m, "https://mail.google.com/"),
                "{m} must wait for the owner"
            );
            assert!(
                !ApprovalBroker::needs_owner(m, "https://example.com/"),
                "{m}: ordinary origins never ask"
            );
        }
        for m in fixture["allowed"].as_array().unwrap() {
            let m = m.as_str().unwrap();
            assert!(
                !ApprovalBroker::needs_owner(m, "https://mail.google.com/"),
                "{m} is read-only"
            );
        }
    }
}
