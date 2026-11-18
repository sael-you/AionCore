//! The access mode of a phone / Mac (TASK-175, owner decision 2026-11-18):
//! how much agents may do on it without an approval card.
//!
//! - `ask` (default): ask before crucial apps, as described in `agent.rs`.
//! - `trusted`: apps on the owner's list never ask; everything else as `ask`.
//! - `full`: no approval cards at all for this device, for 1 h, 8 h or until
//!   the owner turns it off. Choosing it turns the device switch on for the
//!   same time; when the switch goes off (owner, timeout, the phone's own
//!   Stop) or is set again the normal way, `full` is over and the device is
//!   back on `ask`.
//!
//! Banking / payment apps and password managers keep asking (and password
//! managers are never read) in every mode unless the owner also sets
//! `include_sensitive`.
//!
//! Only the owner's session changes any of this (`PUT /api/devices/{id}/access`,
//! `…/access/trusted-apps`, "Always allow" on an approval card); there is no
//! runtime-token route and no MCP tool for it. The hard stops are not touched
//! by any mode: device switch, verb allow-list, rate limit, the private-URL
//! refusal and one audit row per call (which names the mode).
//!
//! Stored in the device row's JSON (`meta.access`); inventory upserts keep
//! that key (`SqliteDeviceRepository::upsert_device`).

use aionui_api_types::devices::{
    DEVICE_TRUSTED_APP_CHARS, DEVICE_TRUSTED_APPS_MAX, Device, DeviceAccess, DeviceAccessMode, DeviceAccessUpdate,
    DeviceErrorCode, DeviceFullDuration, DeviceKind, RelayOutbound,
};
use aionui_common::now_ms;
use aionui_db::DeviceRow;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::DevicesError;
use crate::service::{DEFAULT_TIMEOUT_MINUTES, DevicesService};

/// What the row keeps (`meta.access`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StoredAccess {
    #[serde(default)]
    mode: DeviceAccessMode,
    /// `full`: the `enabled_until` the switch got with it (null = no timeout).
    /// `full` is in effect only while the row still has exactly this value.
    #[serde(default)]
    full_until: Option<i64>,
    #[serde(default)]
    include_sensitive: bool,
    #[serde(default)]
    trusted_apps: Vec<String>,
}

fn stored(row: &DeviceRow) -> StoredAccess {
    serde_json::from_str::<serde_json::Value>(&row.meta)
        .ok()
        .and_then(|meta| meta.get("access").cloned())
        .and_then(|access| serde_json::from_value(access).ok())
        .unwrap_or_default()
}

/// The access settings in effect for `row` right now.
pub fn effective_access(row: &DeviceRow) -> DeviceAccess {
    let s = stored(row);
    let switch_on = row.enabled && row.enabled_until.is_none_or(|until| until > now_ms());
    let mode = match s.mode {
        // The switch must be on AND still the one `full` was chosen with.
        DeviceAccessMode::Full if switch_on && row.enabled_until == s.full_until => DeviceAccessMode::Full,
        DeviceAccessMode::Full => DeviceAccessMode::Ask,
        other => other,
    };
    DeviceAccess {
        mode,
        full_until: (mode == DeviceAccessMode::Full).then_some(s.full_until).flatten(),
        include_sensitive: mode != DeviceAccessMode::Ask && s.include_sensitive,
        trusted_apps: s.trusted_apps,
    }
}

pub(crate) fn mode_str(mode: DeviceAccessMode) -> &'static str {
    match mode {
        DeviceAccessMode::Ask => "ask",
        DeviceAccessMode::Trusted => "trusted",
        DeviceAccessMode::Full => "full",
    }
}

/// Whether `app` is on the list (ids compare without case).
pub(crate) fn is_trusted(access: &DeviceAccess, app: &str) -> bool {
    access.mode == DeviceAccessMode::Trusted && access.trusted_apps.iter().any(|t| t.eq_ignore_ascii_case(app))
}

fn bad(message: &'static str) -> DevicesError {
    DevicesError::coded(DeviceErrorCode::DeviceBadMessage, message)
}

/// A package / bundle id as the owner may list it.
fn valid_app(app: &str) -> bool {
    !app.is_empty()
        && app.len() <= DEVICE_TRUSTED_APP_CHARS
        && app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn clean_apps(apps: &[String]) -> Result<Vec<String>, DevicesError> {
    let mut out: Vec<String> = Vec::new();
    for app in apps {
        let app = app.trim();
        if !valid_app(app) {
            return Err(bad(
                "an app id has 1 to 200 letters, digits, dots, underscores or hyphens",
            ));
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(app)) {
            out.push(app.to_owned());
        }
    }
    if out.len() > DEVICE_TRUSTED_APPS_MAX {
        return Err(bad("at most 100 trusted apps"));
    }
    Ok(out)
}

impl DevicesService {
    /// The phone / Mac row behind card `id`, or a coded error.
    async fn access_row(&self, user_id: &str, id: &str) -> Result<DeviceRow, DevicesError> {
        let row = self
            .repo()
            .get_device(user_id, id)
            .await?
            .ok_or_else(DevicesError::not_found)?;
        if !matches!(row.kind.as_str(), "phone" | "mac") {
            return Err(bad("only phones and Macs have an access mode"));
        }
        Ok(row)
    }

    async fn save_access(&self, user_id: &str, id: &str, access: &StoredAccess) -> Result<(), DevicesError> {
        let json = serde_json::to_string(access).map_err(|e| DevicesError::Internal(e.to_string()))?;
        if !self.repo().set_device_access(user_id, id, &json, now_ms()).await? {
            return Err(DevicesError::not_found());
        }
        Ok(())
    }

    /// Set the switch of card `id` and of the Mac-relay rows of the same phone.
    async fn set_switch(&self, user_id: &str, id: &str, until: Option<i64>) -> Result<(), DevicesError> {
        self.repo()
            .set_device_enabled(user_id, id, true, until, now_ms())
            .await?;
        for linked in self.linked_rows(user_id, id).await? {
            self.repo()
                .set_device_enabled(user_id, &linked.id, true, until, now_ms())
                .await?;
        }
        Ok(())
    }

    /// `PUT /api/devices/{id}/access` (owner session only).
    pub async fn set_access(
        &self,
        user_id: &str,
        id: &str,
        update: &DeviceAccessUpdate,
    ) -> Result<Device, DevicesError> {
        let card = self.get(user_id, id).await?;
        if !matches!(card.kind, DeviceKind::Phone | DeviceKind::Mac) {
            return Err(bad("only phones and Macs have an access mode"));
        }
        let row = self.access_row(user_id, id).await?;
        let was_full = effective_access(&row).mode == DeviceAccessMode::Full;
        let mut access = stored(&row);
        match (update.mode, update.full_duration) {
            (DeviceAccessMode::Full, None) => {
                return Err(bad("full access needs full_duration: 1h, 8h or until_off"));
            }
            (DeviceAccessMode::Full, Some(duration)) => {
                let until = match duration {
                    DeviceFullDuration::OneHour => Some(now_ms() + 3_600_000),
                    DeviceFullDuration::EightHours => Some(now_ms() + 8 * 3_600_000),
                    DeviceFullDuration::UntilOff => None,
                };
                // Full access = the switch is on for exactly that long.
                self.set_switch(user_id, id, until).await?;
                access.full_until = until;
            }
            (_, Some(_)) => return Err(bad("full_duration only goes with mode full")),
            (_, None) => {
                access.full_until = None;
                // Leaving full: the switch gets its normal timeout back.
                if was_full {
                    let until = now_ms() + i64::from(DEFAULT_TIMEOUT_MINUTES) * 60_000;
                    self.set_switch(user_id, id, Some(until)).await?;
                }
            }
        }
        access.mode = update.mode;
        access.include_sensitive = update.mode != DeviceAccessMode::Ask && update.include_sensitive;
        self.save_access(user_id, id, &access).await?;
        info!(
            user_id,
            device_id = id,
            mode = mode_str(update.mode),
            include_sensitive = access.include_sensitive,
            "device access mode changed by the owner"
        );
        self.push_access_mode(user_id, id).await;
        self.changed(user_id, Some(id));
        self.get(user_id, id).await
    }

    /// `PUT /api/devices/{id}/access/trusted-apps` (owner session only).
    pub async fn set_trusted_apps(&self, user_id: &str, id: &str, apps: &[String]) -> Result<Device, DevicesError> {
        let row = self.access_row(user_id, id).await?;
        let mut access = stored(&row);
        access.trusted_apps = clean_apps(apps)?;
        self.save_access(user_id, id, &access).await?;
        info!(
            user_id,
            device_id = id,
            apps = access.trusted_apps.len(),
            "device trusted apps changed by the owner"
        );
        self.changed(user_id, Some(id));
        self.get(user_id, id).await
    }

    /// "Always allow this app" on an approval card: the app joins the list
    /// and `ask` becomes `trusted`. `full` stays `full`.
    pub(crate) async fn trust_app(&self, user_id: &str, id: &str, app: &str) -> Result<(), DevicesError> {
        if !valid_app(app) || app == "unknown" {
            return Err(bad("this approval names no app that can be always allowed"));
        }
        let row = self.access_row(user_id, id).await?;
        let mut access = stored(&row);
        if !access.trusted_apps.iter().any(|t| t.eq_ignore_ascii_case(app)) {
            access.trusted_apps.push(app.to_owned());
        }
        if access.trusted_apps.len() > DEVICE_TRUSTED_APPS_MAX {
            return Err(bad("at most 100 trusted apps"));
        }
        if effective_access(&row).mode == DeviceAccessMode::Ask {
            access.mode = DeviceAccessMode::Trusted;
            access.full_until = None;
        }
        self.save_access(user_id, id, &access).await?;
        info!(
            user_id,
            device_id = id,
            "an app was added to the device's trusted apps by the owner"
        );
        self.push_access_mode(user_id, id).await;
        self.changed(user_id, Some(id));
        Ok(())
    }

    /// Tell the companion app which mode is in effect (its chip shows it).
    /// Older apps ignore the frame.
    pub(crate) async fn push_access_mode(&self, user_id: &str, id: &str) {
        let Ok(Some(row)) = self.repo().get_device(user_id, id).await else {
            return;
        };
        let Some(pairing_id) = row.pairing_id.as_deref() else {
            return;
        };
        self.relay_send_to_companion(
            pairing_id,
            RelayOutbound::Access {
                device: row.source_id.clone(),
                mode: effective_access(&row).mode,
            },
        );
    }
}
