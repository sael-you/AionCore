//! Add-ons (C38 §5/§6, TASK-136 K5): Artemis and Mobilerun run their own
//! agent loop on a phone. They live where adb runs — on the owner's Mac helper —
//! so every step here is a relay verb (`addon_install`, `addon_remove`,
//! `addon_run`, `addon_stop`) plus the gates core owns:
//!
//! - install / remove from the Devices page ([`AddonInstaller`], 21-minute verbs);
//! - a run starts only when the add-on's switch and the phone's switch are on
//!   and the **owner approved the task text** (per-tap gates are impossible
//!   inside the add-on's loop) — one store, [`DevicesService::ask_owner`];
//! - the run is stopped when either switch goes off or its time is up;
//! - audit rows keep the task (clipped), the result, the duration and the
//!   trace link.
//!
//! Keys: an add-on's model keys stay on the Mac (the helper's `keys.env`, set
//! there, "never leave the Mac"); W2 has no verb that carries one, so core
//! stores none, logs none and can return none.

use std::sync::{Arc, Weak};
use std::time::Duration;

use aionui_api_types::devices::{DeviceAuditOutcome, DeviceErrorCode, DeviceKind, RELAY_DEVICE_MAC, RelayClientKind};
use aionui_common::now_ms;
use aionui_db::DeviceRow;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::approvals::OwnerQuestion;
use crate::error::DevicesError;
use crate::relay::{RelayClient, relay_error_code};
use crate::service::{AuditNote, DeviceInstaller, DevicesService, LIBRARY_ADDONS, ProgressFn};

pub const VERB_INSTALL: &str = "addon_install";
pub const VERB_REMOVE: &str = "addon_remove";
pub const VERB_RUN: &str = "addon_run";
pub const VERB_STOP: &str = "addon_stop";
pub const EVENT_PROGRESS: &str = "addon_progress";

pub const TASK_MAX_CHARS: usize = 4000;
pub const DEFAULT_MINUTES: u32 = 10;
pub const MAX_MINUTES: u32 = 30;
/// How often a running task re-checks the two switches.
const SWITCH_POLL: Duration = Duration::from_secs(2);
/// The helper ends a run at `max_minutes`; core gives up this much later.
const END_GRACE: Duration = Duration::from_secs(60);
const AUDIT_TASK_CHARS: usize = 200;
const TRACE_URL_CHARS: usize = 500;

/// One task for an add-on (the agent tool `addon_run_task`).
#[derive(Debug, Clone)]
pub struct AddonTask {
    pub user_id: String,
    /// The installed add-on's device id (`addon:<pairing>:<name>`).
    pub addon_id: String,
    /// The phone it acts on (`phone:<pairing>:<serial>`), reached by the same Mac helper.
    pub phone_id: String,
    /// What the add-on should do: shown to the owner in full before it starts.
    pub task: String,
    /// 1–30, default 10.
    pub max_minutes: Option<u32>,
    pub conversation_id: Option<String>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddonRunOutcome {
    pub run_id: String,
    /// `done` | `failed` | `stopped`.
    pub state: String,
    pub message: Option<String>,
    /// Link to the add-on's own trace of the run (http/https only).
    pub trace_url: Option<String>,
    pub duration_ms: i64,
}

fn refused(message: &str) -> DevicesError {
    DevicesError::coded(DeviceErrorCode::DeviceVerbRefused, message)
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        flat
    }
}

/// Only an http(s) link is kept (it is shown as a link in the audit).
fn trace_url(data: &Value) -> Option<String> {
    data.get("trace_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| (u.starts_with("https://") || u.starts_with("http://")) && u.chars().count() <= TRACE_URL_CHARS)
        .filter(|u| !u.chars().any(char::is_whitespace))
        .map(str::to_owned)
}

/// `addon:<name>` (a Library card, not installed) → the library name.
fn library_name(device_id: &str) -> Option<&'static str> {
    let name = device_id.strip_prefix("addon:")?;
    LIBRARY_ADDONS.iter().map(|(n, _)| *n).find(|n| *n == name)
}

impl DevicesService {
    /// The user's connected Mac helper that hosts add-ons (serves `verb`);
    /// `pairing_id` picks one, else the first.
    async fn addon_host(
        &self,
        user_id: &str,
        pairing_id: Option<&str>,
        verb: &str,
    ) -> Result<Arc<RelayClient>, DevicesError> {
        let pairings = self.repo().list_pairings(user_id).await?;
        let mut offline = false;
        for p in pairings.iter().filter(|p| p.revoked_at.is_none()) {
            if pairing_id.is_some_and(|want| want != p.id) {
                continue;
            }
            match self.relay_client(user_id, &p.id) {
                Some(client) if client.kind == RelayClientKind::MacHelper && client.serves(verb) => return Ok(client),
                Some(_) => {}
                None => offline = true,
            }
        }
        Err(if offline {
            DevicesError::coded(DeviceErrorCode::DeviceOffline, "the Mac helper is offline")
        } else {
            refused("add-ons need the Mac helper (USB / Wi-Fi debugging via my Mac)")
        })
    }

    async fn addon_row(&self, user_id: &str, id: &str) -> Result<DeviceRow, DevicesError> {
        self.repo()
            .get_device(user_id, id)
            .await?
            .filter(|row| row.kind == "addon")
            .ok_or_else(DevicesError::not_found)
    }

    async fn addon_audit(&self, t: &AddonTask, outcome: DeviceAuditOutcome, detail: &Value) {
        self.record(
            &t.user_id,
            &t.addon_id,
            AuditNote {
                verb: VERB_RUN,
                package: None,
                origin: None,
                outcome,
                conversation_id: t.conversation_id.as_deref(),
                detail: Some(&detail.to_string()),
            },
        )
        .await;
    }

    /// Both switches on and not expired.
    async fn addon_switches_on(&self, t: &AddonTask) -> bool {
        matches!(self.is_enabled(&t.user_id, &t.addon_id).await, Ok(true))
            && matches!(self.is_enabled(&t.user_id, &t.phone_id).await, Ok(true))
    }

    /// Run one task with an add-on and wait for its end.
    ///
    /// Order: both switches → the owner approves the task text → the run →
    /// audit. A denied, unanswered or switched-off task never reaches the Mac.
    pub async fn addon_run_task(&self, t: AddonTask) -> Result<AddonRunOutcome, DevicesError> {
        let task = t.task.trim().to_owned();
        if task.is_empty() || task.chars().count() > TASK_MAX_CHARS {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceBadMessage,
                format!("task must be 1-{TASK_MAX_CHARS} characters"),
            ));
        }
        let minutes = t.max_minutes.unwrap_or(DEFAULT_MINUTES);
        if !(1..=MAX_MINUTES).contains(&minutes) {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceBadMessage,
                format!("max_minutes must be 1-{MAX_MINUTES}"),
            ));
        }
        let addon = self.addon_row(&t.user_id, &t.addon_id).await?;
        let phone = self
            .repo()
            .get_device(&t.user_id, &t.phone_id)
            .await?
            .filter(|row| row.kind == "phone")
            .ok_or_else(DevicesError::not_found)?;
        // The add-on drives the phone over the adb of the Mac it is installed on.
        let pairing = match (&addon.pairing_id, &phone.pairing_id) {
            (Some(a), Some(p)) if a == p => a.clone(),
            _ => return Err(refused("the add-on and the phone must be on the same Mac helper")),
        };
        let audit_task = clip(&task, AUDIT_TASK_CHARS);
        if !self.addon_switches_on(&t).await {
            self.addon_audit(
                &t,
                DeviceAuditOutcome::Refused,
                &json!({"task": audit_task, "reason": "switch_off"}),
            )
            .await;
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceDisabled,
                "the add-on or the phone is switched off",
            ));
        }
        let host = self.addon_host(&t.user_id, Some(&pairing), VERB_RUN).await?;

        // The owner sees the full task text and decides.
        let question = OwnerQuestion {
            user_id: t.user_id.clone(),
            device_id: t.addon_id.clone(),
            device_name: addon.name.clone(),
            verb: VERB_RUN.to_owned(),
            summary: format!("{} on {}: {}", addon.name, phone.name, task),
            task: Some(task.clone()),
            package: None,
            origin: None,
            conversation_id: t.conversation_id.clone(),
            grant_key: None,
            sensitive: false,
        };
        if let Err(error) = self.ask_owner(question).await {
            let outcome = match error.code() {
                Some(DeviceErrorCode::DeviceApprovalTimeout) => DeviceAuditOutcome::Timeout,
                _ => DeviceAuditOutcome::Denied,
            };
            self.addon_audit(&t, outcome, &json!({"task": audit_task})).await;
            return Err(error);
        }
        self.addon_audit(
            &t,
            DeviceAuditOutcome::Approved,
            &json!({"task": audit_task, "phone": phone.name, "max_minutes": minutes}),
        )
        .await;
        // The approval can take two minutes: a switch may have gone off meanwhile.
        if !self.addon_switches_on(&t).await {
            self.addon_audit(
                &t,
                DeviceAuditOutcome::Refused,
                &json!({"task": audit_task, "reason": "switch_off"}),
            )
            .await;
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceDisabled,
                "the add-on or the phone was switched off",
            ));
        }

        // Listen before the run starts, so its end is never missed.
        let mut events = self.subscribe_relay_events();
        let started = now_ms();
        let args = json!({"name": addon.source_id, "task": task, "device": phone.source_id, "max_minutes": minutes});
        let run_id = match host.call(&phone.source_id, VERB_RUN, args).await {
            Ok(result) => result
                .get("run_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            Err(error) => {
                let code = relay_error_code(&error);
                self.addon_audit(
                    &t,
                    DeviceAuditOutcome::Error,
                    &json!({"task": audit_task, "error": error.code}),
                )
                .await;
                return Err(DevicesError::coded(code, error.message));
            }
        };
        let Some(run_id) = run_id else {
            self.addon_audit(
                &t,
                DeviceAuditOutcome::Error,
                &json!({"task": audit_task, "error": "no_run_id"}),
            )
            .await;
            return Err(refused("the Mac helper did not start the run"));
        };
        info!(user_id = %t.user_id, addon = %addon.source_id, run_id = %run_id, max_minutes = minutes, "add-on run started");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(minutes) * 60) + END_GRACE;
        let mut poll = tokio::time::interval(SWITCH_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stop = |why: &'static str| {
            let host = host.clone();
            let (device, run) = (phone.source_id.clone(), run_id.clone());
            async move {
                if let Err(error) = host.call(&device, VERB_STOP, json!({"run_id": run})).await {
                    warn!(run_id = %run, error = %error.code, why, "add-on run stop was not confirmed");
                }
            }
        };
        let (state, message, trace) = loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(e) if e.pairing_id == pairing && e.name == EVENT_PROGRESS
                        && e.data.get("run_id").and_then(Value::as_str) == Some(run_id.as_str()) =>
                    {
                        let state = e.data.get("state").and_then(Value::as_str).unwrap_or("");
                        if matches!(state, "done" | "failed" | "stopped") {
                            let message = e.data.get("message").and_then(Value::as_str).map(|m| clip(m, 300));
                            break (state.to_owned(), message, trace_url(&e.data));
                        }
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break ("failed".to_owned(), Some("the relay closed".to_owned()), None),
                },
                _ = poll.tick() => {
                    if !self.addon_switches_on(&t).await {
                        stop("switch_off").await;
                        break ("stopped".to_owned(), Some("switched off".to_owned()), None);
                    }
                    if self.relay_client(&t.user_id, &pairing).is_none() {
                        break ("failed".to_owned(), Some("the Mac helper went offline".to_owned()), None);
                    }
                    if tokio::time::Instant::now() >= deadline {
                        stop("deadline").await;
                        break ("failed".to_owned(), Some("max_minutes reached".to_owned()), None);
                    }
                }
            }
        };
        let duration_ms = now_ms() - started;
        let outcome = if state == "done" {
            DeviceAuditOutcome::Ok
        } else {
            DeviceAuditOutcome::Error
        };
        self.addon_audit(
            &t,
            outcome,
            &json!({"task": audit_task, "state": state, "duration_ms": duration_ms,
                    "message": message, "trace_url": trace}),
        )
        .await;
        info!(user_id = %t.user_id, addon = %addon.source_id, run_id = %run_id, state = %state, duration_ms, "add-on run ended");
        Ok(AddonRunOutcome {
            run_id,
            state,
            message,
            trace_url: trace,
            duration_ms,
        })
    }
}

/// Install / remove of add-ons from the Devices page: one relay verb on the
/// Mac helper, answered when the work is finished (up to 21 minutes).
pub struct AddonInstaller {
    service: Weak<DevicesService>,
}

impl AddonInstaller {
    pub fn new(service: &Arc<DevicesService>) -> Self {
        Self {
            service: Arc::downgrade(service),
        }
    }

    fn service(&self) -> Result<Arc<DevicesService>, DevicesError> {
        self.service
            .upgrade()
            .ok_or_else(|| DevicesError::Internal("devices service is gone".into()))
    }
}

/// Register the add-on installer (called once by the app next to `DevicesService::new`).
pub fn register(service: &Arc<DevicesService>) {
    service.register_installer(Arc::new(AddonInstaller::new(service)));
}

#[async_trait::async_trait]
impl DeviceInstaller for AddonInstaller {
    fn handles(&self, device_id: &str) -> bool {
        device_id.starts_with("addon:")
    }

    async fn install(&self, user_id: &str, device_id: &str, progress: &ProgressFn) -> Result<(), DevicesError> {
        let svc = self.service()?;
        // A Library card (`addon:<name>`) or a re-install of an installed one.
        let (name, pairing) = match library_name(device_id) {
            Some(name) => (name.to_owned(), None),
            None => {
                let row = svc.addon_row(user_id, device_id).await?;
                (row.source_id, row.pairing_id)
            }
        };
        let host = svc.addon_host(user_id, pairing.as_deref(), VERB_INSTALL).await?;
        progress(5);
        info!(user_id, addon = %name, "add-on install started on the Mac helper");
        host.call(RELAY_DEVICE_MAC, VERB_INSTALL, json!({"name": name}))
            .await
            .map_err(|e| DevicesError::coded(relay_error_code(&e), e.message))?;
        progress(100);
        // The helper's next inventory carries the installed add-on.
        svc.changed(user_id, None);
        Ok(())
    }

    async fn remove(&self, user_id: &str, device_id: &str, progress: &ProgressFn) -> Result<(), DevicesError> {
        let svc = self.service()?;
        let row = svc.addon_row(user_id, device_id).await?;
        let host = svc.addon_host(user_id, row.pairing_id.as_deref(), VERB_REMOVE).await?;
        progress(5);
        info!(user_id, addon = %row.source_id, "add-on remove started on the Mac helper");
        host.call(RELAY_DEVICE_MAC, VERB_REMOVE, json!({"name": row.source_id}))
            .await
            .map_err(|e| DevicesError::coded(relay_error_code(&e), e.message))?;
        // Its disk is freed: forget the row now (the next inventory agrees).
        svc.repo().delete_device(user_id, device_id).await?;
        progress(100);
        svc.changed(user_id, None);
        Ok(())
    }
}

/// The kind check `start_job` uses: an installed add-on is removed through
/// its installer (the Mac frees the disk), not just forgotten like a phone.
pub(crate) fn removes_through_installer(kind: DeviceKind) -> bool {
    kind == DeviceKind::Addon
}
