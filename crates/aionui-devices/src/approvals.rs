//! Owner approvals for device actions (C38 §3 "ask before crucial", K3):
//! `GET /api/devices/approvals`, `POST /api/devices/approvals/{id}` and the
//! `devices.approval` event. One store for every caller (phone verbs, Phone ·
//! Chrome, add-on tasks): [`DevicesService::ask_owner`].
//!
//! A question waits [`APPROVAL_TIMEOUT`]; no answer = refused
//! (`DEVICE_APPROVAL_TIMEOUT`); a deny refuses that action only
//! (`DEVICE_APPROVAL_DENIED`). With a `grant_key`, one allow covers the same
//! key for [`GRANT_TTL`] (5 minutes, owner decision 2026-11-17: reads and
//! actions on the same device + app; the prompt says "allow for 5 min").

use std::time::Duration;

use aionui_api_types::devices::{
    DeviceApproval, DeviceApprovalEvent, DeviceApprovalState, DeviceErrorCode, EVENT_DEVICES_APPROVAL,
};
use aionui_common::approval_queue::Answer;
use aionui_common::now_ms;
use tracing::info;

use crate::error::DevicesError;
use crate::service::DevicesService;

pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
pub const GRANT_TTL: Duration = Duration::from_secs(5 * 60);
const SUMMARY_CHARS: usize = 200;

/// One question for the owner.
#[derive(Debug, Clone, Default)]
pub struct OwnerQuestion {
    pub user_id: String,
    pub device_id: String,
    pub device_name: String,
    pub verb: String,
    /// What it would do (clipped to 200 characters for display).
    pub summary: String,
    /// Add-on runs: the full task text the owner approves.
    pub task: Option<String>,
    pub package: Option<String>,
    pub origin: Option<String>,
    pub conversation_id: Option<String>,
    /// When set, an allow also covers later questions with this key for
    /// [`GRANT_TTL`], and a live grant answers at once.
    pub grant_key: Option<String>,
    /// The app is a banking / payment app or a password manager.
    pub sensitive: bool,
}

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > SUMMARY_CHARS {
        let mut out: String = flat.chars().take(SUMMARY_CHARS - 1).collect();
        out.push('…');
        out
    } else {
        flat
    }
}

impl DevicesService {
    fn approval_event(&self, user_id: &str, approval: &DeviceApproval, state: DeviceApprovalState, ok: Option<bool>) {
        self.emit(
            user_id,
            EVENT_DEVICES_APPROVAL,
            serde_json::to_value(DeviceApprovalEvent {
                approval: approval.clone(),
                state,
                approved: ok,
            })
            .unwrap_or(serde_json::Value::Null),
        );
    }

    /// Ask the owner and wait. `Ok(())` = allowed (now, or by a live grant).
    pub async fn ask_owner(&self, q: OwnerQuestion) -> Result<(), DevicesError> {
        self.ask_owner_within(q, APPROVAL_TIMEOUT).await
    }

    pub(crate) async fn ask_owner_within(&self, q: OwnerQuestion, timeout: Duration) -> Result<(), DevicesError> {
        if let Some(key) = &q.grant_key
            && self.approval_queue().granted(key)
        {
            return Ok(());
        }
        let created_at = now_ms();
        let approval = DeviceApproval {
            id: format!("apr_{}", uuid::Uuid::new_v4().simple()),
            device_id: q.device_id.clone(),
            device_name: q.device_name.clone(),
            verb: q.verb.clone(),
            summary: clip(&q.summary),
            task: q.task.clone(),
            package: q.package.clone(),
            origin: q.origin.clone(),
            conversation_id: q.conversation_id.clone(),
            created_at,
            expires_at: created_at + i64::try_from(timeout.as_millis()).unwrap_or(0),
            grant_minutes: q
                .grant_key
                .is_some()
                .then(|| u32::try_from(GRANT_TTL.as_secs() / 60).unwrap_or(0)),
            sensitive: q.sensitive,
        };
        info!(user_id = %q.user_id, device_id = %q.device_id, approval_id = %approval.id, verb = %q.verb, "device action paused for the owner");
        self.approval_event(&q.user_id, &approval, DeviceApprovalState::Requested, None);
        let answer = self
            .approval_queue()
            .ask(&approval.id, &q.user_id, approval.clone(), timeout)
            .await;
        match answer {
            Answer::Allowed => {
                if let Some(key) = &q.grant_key {
                    self.approval_queue().grant(key, GRANT_TTL);
                }
                Ok(())
            }
            Answer::Denied => Err(DevicesError::coded(
                DeviceErrorCode::DeviceApprovalDenied,
                "the owner denied this action",
            )),
            Answer::TimedOut => {
                // Tell the UI to drop the card (a decided one was announced already).
                self.approval_event(&q.user_id, &approval, DeviceApprovalState::Resolved, Some(false));
                Err(DevicesError::coded(
                    DeviceErrorCode::DeviceApprovalTimeout,
                    "the owner did not answer in time",
                ))
            }
        }
    }

    /// `GET /api/devices/approvals`: the user's open questions, oldest first.
    pub fn approvals(&self, user_id: &str) -> Vec<DeviceApproval> {
        let mut rows = self.approval_queue().list(user_id);
        rows.sort_by_key(|r| r.created_at);
        rows
    }

    /// `POST /api/devices/approvals/{id}`. Unknown, answered, expired or
    /// another user's → `DEVICE_APPROVAL_EXPIRED`.
    pub fn decide_approval(&self, user_id: &str, id: &str, allow: bool) -> Result<(), DevicesError> {
        let approval = self.approval_queue().decide(user_id, id, allow).ok_or_else(|| {
            DevicesError::coded(
                DeviceErrorCode::DeviceApprovalExpired,
                "the approval is no longer pending",
            )
        })?;
        info!(user_id, approval_id = id, allow, "owner answered a device approval");
        self.approval_event(user_id, &approval, DeviceApprovalState::Resolved, Some(allow));
        Ok(())
    }

    /// `POST /api/devices/approvals/{id}` with "Always allow this app": the
    /// approval's app joins the device's trusted apps, then the question is
    /// answered. Only for an allow of a phone / Mac action that names an app.
    pub async fn decide_approval_always(&self, user_id: &str, id: &str) -> Result<(), DevicesError> {
        let expired = || {
            DevicesError::coded(
                DeviceErrorCode::DeviceApprovalExpired,
                "the approval is no longer pending",
            )
        };
        let approval = self
            .approvals(user_id)
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(expired)?;
        let app = approval
            .package
            .as_deref()
            .filter(|_| approval.task.is_none())
            .ok_or_else(|| {
                DevicesError::coded(
                    DeviceErrorCode::DeviceBadMessage,
                    "this approval names no app that can be always allowed",
                )
            })?;
        self.trust_app(user_id, &approval.device_id, app).await?;
        self.decide_approval(user_id, id, true)
    }

    /// Drop the open questions of a device (switched off, revoked): the
    /// waiting actions are refused.
    pub(crate) fn drop_device_approvals(&self, user_id: &str, device_id: &str) {
        for approval in self
            .approval_queue()
            .drop_where(|u, a| u == user_id && a.device_id == device_id)
        {
            self.approval_event(user_id, &approval, DeviceApprovalState::Resolved, Some(false));
        }
    }
}
