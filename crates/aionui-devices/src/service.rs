//! The devices registry (C38 / TASK-136 K1). Sole place for business logic;
//! `routes.rs` only translates.
//!
//! - Server browser and own-browser cards are built from `aionui-browser`
//!   facts on every read (no second store); their switches write through.
//! - Phones, the Mac and add-ons are rows of migration 911, refreshed from
//!   relay inventories (K2). A phone / Mac switch needs a timeout (15, 30 or
//!   60 minutes): `enabled_until` is persisted, a ticker turns expired
//!   switches off, and startup does the same for those that expired while
//!   AionCore was down.
//! - Install / remove run as in-memory jobs through registered installers
//!   (K5 add-ons, K7 engines). A job does not survive a restart: its id then
//!   answers `DEVICE_JOB_NOT_FOUND`, which the UI shows as failed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::devices::{
    DEVICE_TIMEOUT_MINUTES, Device, DeviceAuditEntry, DeviceAuditOutcome, DeviceAuditPage, DeviceAvailability,
    DeviceErrorCode, DeviceJob, DeviceJobState, DeviceKind, DeviceRoute, DeviceRouteKind, DeviceStatus, DeviceUpdate,
    DevicesChangedEvent, EVENT_DEVICES_CHANGED,
};
use aionui_common::now_ms;
use aionui_db::{DeviceRow, IDeviceRepository, NewDeviceAuditEntry};
use dashmap::DashMap;
use tracing::{info, warn};

use crate::error::DevicesError;
use crate::ports::{BrowserDevices, DevicesEventSink, NoopDevicesEventSink};
use crate::relay::{RelayState, relay_error_code};

pub const SERVER_BROWSER_ID: &str = "server_browser:chrome";
pub const CAMOUFOX_ID: &str = "server_browser:camoufox";

/// The registry id of a server-browser engine (`server_browser:<engine id>`).
pub fn server_browser_id(engine_id: &str) -> String {
    format!("server_browser:{engine_id}")
}
pub const MAC_LATER_ID: &str = "mac:later";
/// Add-ons the Library offers (C38 §5).
pub const LIBRARY_ADDONS: &[(&str, &str)] = &[("artemis", "Artemis"), ("mobilerun", "Mobilerun")];
pub const DEFAULT_TIMEOUT_MINUTES: u32 = 30;
const EXPIRE_EVERY: Duration = Duration::from_secs(15);
const AUDIT_PAGE_DEFAULT: u32 = 50;
const AUDIT_PAGE_MAX: u32 = 200;

/// One audit row: what was done, never payload.
#[derive(Debug, Clone, Copy)]
pub struct AuditNote<'a> {
    pub verb: &'a str,
    pub package: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub outcome: DeviceAuditOutcome,
    pub conversation_id: Option<&'a str>,
    pub detail: Option<&'a str>,
}

/// Installs or removes one kind of device (K5 add-ons, K7 engines).
#[async_trait::async_trait]
pub trait DeviceInstaller: Send + Sync {
    /// Whether this installer handles `device_id`.
    fn handles(&self, device_id: &str) -> bool;
    async fn install(&self, user_id: &str, device_id: &str, progress: &ProgressFn) -> Result<(), DevicesError>;
    /// Asked before a remove job is created, so the request itself answers
    /// 409 `DEVICE_IN_USE` (K7 engines). Default: no objection.
    async fn check_remove(&self, _user_id: &str, _device_id: &str) -> Result<(), DevicesError> {
        Ok(())
    }
    /// `DEVICE_IN_USE` when something still needs it.
    async fn remove(&self, user_id: &str, device_id: &str, progress: &ProgressFn) -> Result<(), DevicesError>;
}

pub type ProgressFn = dyn Fn(u8) + Send + Sync;

struct JobEntry {
    user_id: String,
    job: DeviceJob,
    removing: bool,
}

pub struct DevicesService {
    repo: Arc<dyn IDeviceRepository>,
    browser: Arc<dyn BrowserDevices>,
    sink: std::sync::RwLock<Arc<dyn DevicesEventSink>>,
    installers: std::sync::RwLock<Vec<Arc<dyn DeviceInstaller>>>,
    jobs: Arc<DashMap<String, JobEntry>>,
    /// Relay clients connected right now.
    online_pairings: DashMap<String, ()>,
    relay: RelayState,
    companion: std::sync::RwLock<Option<Arc<crate::companion::CompanionDownloads>>>,
    approvals: aionui_common::approval_queue::ApprovalQueue<aionui_api_types::devices::DeviceApproval>,
    /// Agent calls per device in the last minute.
    rate: std::sync::Mutex<HashMap<String, std::collections::VecDeque<std::time::Instant>>>,
}

impl std::fmt::Debug for DevicesService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicesService").finish_non_exhaustive()
    }
}

fn route_of_pairing_kind(kind: &str) -> Option<DeviceRouteKind> {
    match kind {
        "mac_helper" => Some(DeviceRouteKind::MacRelay),
        "companion" => Some(DeviceRouteKind::Companion),
        "wifi_direct" => Some(DeviceRouteKind::WifiDirect),
        _ => None,
    }
}

/// The companion card a Mac-relay phone row belongs to: its inventory named
/// a companion app (`companion_client_id`) that is one of this user's
/// companion pairings. Such a phone is ONE card (the companion's) with two
/// routes (director, 2026-11-17).
fn companion_card_of(row: &DeviceRow, pairings: &HashMap<String, String>) -> Option<String> {
    if row.kind != "phone" || row.source_id == aionui_api_types::devices::RELAY_DEVICE_SELF {
        return None;
    }
    let meta = meta_of(row);
    let cid = meta.get("companion_client_id")?.as_str()?;
    (pairings.get(cid).map(String::as_str) == Some("companion"))
        .then(|| crate::relay::relay_device_id("phone", cid, aionui_api_types::devices::RELAY_DEVICE_SELF))
}

fn meta_of(row: &DeviceRow) -> serde_json::Value {
    serde_json::from_str(&row.meta).unwrap_or(serde_json::Value::Null)
}

impl DevicesService {
    pub fn new(repo: Arc<dyn IDeviceRepository>, browser: Arc<dyn BrowserDevices>) -> Arc<Self> {
        Arc::new(Self {
            repo,
            browser,
            sink: std::sync::RwLock::new(Arc::new(NoopDevicesEventSink)),
            installers: std::sync::RwLock::new(Vec::new()),
            jobs: Arc::new(DashMap::new()),
            online_pairings: DashMap::new(),
            relay: RelayState::default(),
            companion: std::sync::RwLock::new(None),
            approvals: Default::default(),
            rate: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn rate_windows(
        &self,
    ) -> &std::sync::Mutex<HashMap<String, std::collections::VecDeque<std::time::Instant>>> {
        &self.rate
    }

    pub(crate) fn approval_queue(
        &self,
    ) -> &aionui_common::approval_queue::ApprovalQueue<aionui_api_types::devices::DeviceApproval> {
        &self.approvals
    }

    /// One event to the owning user only.
    pub fn emit(&self, user_id: &str, name: &str, data: serde_json::Value) {
        let sink = self.sink.read().expect("sink lock").clone();
        sink.emit(user_id, name, data);
    }

    /// Where the signed companion APK lives (`<data dir>/devices/companion`).
    pub fn set_companion_downloads(&self, downloads: Arc<crate::companion::CompanionDownloads>) {
        *self.companion.write().expect("companion lock") = Some(downloads);
    }

    pub fn companion_downloads(&self) -> Option<Arc<crate::companion::CompanionDownloads>> {
        self.companion.read().expect("companion lock").clone()
    }

    pub(crate) fn relay(&self) -> &RelayState {
        &self.relay
    }

    pub fn repo(&self) -> &Arc<dyn IDeviceRepository> {
        &self.repo
    }

    pub fn set_event_sink(&self, sink: Arc<dyn DevicesEventSink>) {
        *self.sink.write().expect("sink lock") = sink;
    }

    pub fn register_installer(&self, installer: Arc<dyn DeviceInstaller>) {
        self.installers.write().expect("installers lock").push(installer);
    }

    /// `devices.changed {device_id}` to the owning user only.
    pub fn changed(&self, user_id: &str, device_id: Option<&str>) {
        let sink = self.sink.read().expect("sink lock").clone();
        sink.emit(
            user_id,
            EVENT_DEVICES_CHANGED,
            serde_json::to_value(DevicesChangedEvent {
                device_id: device_id.map(str::to_owned),
            })
            .unwrap_or(serde_json::Value::Null),
        );
    }

    /// K2: a relay client connected / disconnected.
    pub fn set_pairing_online(&self, pairing_id: &str, online: bool) {
        if online {
            self.online_pairings.insert(pairing_id.to_owned(), ());
        } else {
            self.online_pairings.remove(pairing_id);
        }
    }

    fn pairing_online(&self, pairing_id: Option<&str>) -> bool {
        pairing_id.is_some_and(|p| self.online_pairings.contains_key(p))
    }

    // -- reconcile ------------------------------------------------------------

    /// Startup: switches whose timeout passed while AionCore was down go off.
    /// Then every [`EXPIRE_EVERY`] the same for timeouts that pass while it runs.
    pub fn spawn_background(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(svc) = weak.upgrade() else { return };
                svc.expire_switches().await;
                drop(svc);
                tokio::time::sleep(EXPIRE_EVERY).await;
            }
        });
    }

    pub async fn expire_switches(&self) -> usize {
        match self.repo.expire_device_switches(now_ms()).await {
            Ok(rows) => {
                for row in &rows {
                    info!(user_id = %row.user_id, device_id = %row.id, "device switch timed out; agents may no longer use it");
                    if let Some(p) = &row.pairing_id {
                        self.relay_stop(p, &row.source_id);
                    }
                    self.drop_device_approvals(&row.user_id, &row.id);
                    self.changed(&row.user_id, Some(&row.id));
                }
                rows.len()
            }
            Err(error) => {
                warn!(error = %error, "device switch expiry failed");
                0
            }
        }
    }

    // -- list -----------------------------------------------------------------

    pub async fn list(&self, user_id: &str) -> Result<Vec<Device>, DevicesError> {
        let mut out = Vec::new();
        let engines = self.server_browser_cards(user_id);
        // Camoufox is a real engine row (K6); the placeholder only shows when
        // the browser port does not list it.
        let camoufox_listed = engines.iter().any(|d| d.id == CAMOUFOX_ID);
        out.extend(engines);
        if !camoufox_listed {
            out.push(Device {
                id: CAMOUFOX_ID.to_owned(),
                kind: DeviceKind::ServerBrowser,
                source_id: Some("camoufox".to_owned()),
                name: "Camoufox".to_owned(),
                status: DeviceStatus::Off,
                available: DeviceAvailability::Later,
                enabled: false,
                enabled_until: None,
                installed: false,
                disk_bytes: None,
                version: None,
                job_id: None,
                last_seen_at: None,
                routes: None,
                addon_host: None,
            });
        }
        out.extend(self.my_browser_cards(user_id));
        let rows = self.repo.list_devices(user_id).await?;
        let pairings: HashMap<String, String> = self
            .repo
            .list_pairings(user_id)
            .await?
            .into_iter()
            .map(|p| (p.id, p.kind))
            .collect();
        let mut has_mac = false;
        let mut installed_addons: Vec<String> = Vec::new();
        // Mac-relay rows that are the same phone as a companion card.
        let merged: HashMap<String, &DeviceRow> = rows
            .iter()
            .filter_map(|r| companion_card_of(r, &pairings).map(|card| (card, r)))
            .filter(|(card, _)| rows.iter().any(|r| &r.id == card))
            .collect();
        for row in &rows {
            // A device whose pairing was revoked is gone from the page.
            if let Some(p) = &row.pairing_id
                && !pairings.contains_key(p)
            {
                continue;
            }
            if merged.values().any(|m| m.id == row.id) {
                continue; // shown inside its companion card
            }
            let mut card = self.row_card(row, &pairings);
            if let Some(mac_row) = merged.get(&row.id) {
                let via_mac = self.row_card(mac_row, &pairings);
                if let (Some(routes), Some(mac_routes)) = (card.routes.as_mut(), via_mac.routes) {
                    let extra: Vec<DeviceRoute> = mac_routes
                        .into_iter()
                        .filter(|r| r.route == DeviceRouteKind::MacRelay)
                        .map(|r| DeviceRoute {
                            enabled: card.enabled,
                            ..r
                        })
                        .collect();
                    routes.splice(0..0, extra);
                }
                // Reachable through either route.
                if card.status == DeviceStatus::Offline && via_mac.status != DeviceStatus::Offline {
                    card.status = if card.enabled {
                        DeviceStatus::On
                    } else {
                        DeviceStatus::Off
                    };
                }
            }
            match card.kind {
                DeviceKind::Mac => has_mac = true,
                DeviceKind::Addon => installed_addons.push(row.source_id.clone()),
                _ => {}
            }
            out.push(card);
        }
        if !has_mac {
            out.push(Device {
                id: MAC_LATER_ID.to_owned(),
                kind: DeviceKind::Mac,
                source_id: None,
                name: "Mac".to_owned(),
                status: DeviceStatus::NeedsPairing,
                available: DeviceAvailability::Later,
                enabled: false,
                enabled_until: None,
                installed: false,
                disk_bytes: None,
                version: None,
                job_id: None,
                last_seen_at: None,
                routes: None,
                addon_host: None,
            });
        }
        for (name, label) in LIBRARY_ADDONS {
            if installed_addons.iter().any(|a| a == name) {
                continue;
            }
            let id = format!("addon:{name}");
            out.push(Device {
                job_id: self.job_for(&id),
                status: self.job_status(&id).unwrap_or(DeviceStatus::Off),
                id,
                kind: DeviceKind::Addon,
                source_id: Some((*name).to_owned()),
                name: (*label).to_owned(),
                available: DeviceAvailability::Now,
                enabled: false,
                enabled_until: None,
                installed: false,
                disk_bytes: None,
                version: None,
                last_seen_at: None,
                routes: None,
                addon_host: None,
            });
        }
        Ok(out)
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<Device, DevicesError> {
        self.list(user_id)
            .await?
            .into_iter()
            .find(|d| d.id == id)
            .ok_or_else(DevicesError::not_found)
    }

    /// One card per engine of the server browser (K7): `server_browser:<id>`.
    /// They share the one "agents can use the browser" switch.
    fn server_browser_cards(&self, user_id: &str) -> Vec<Device> {
        let agents_enabled = self.browser.server_browser(user_id).agents_enabled;
        self.browser
            .server_engines(user_id)
            .into_iter()
            .map(|e| {
                let id = server_browser_id(&e.engine_id);
                let status = if let Some(status) = self.job_status(&id) {
                    status
                } else if e.installing {
                    DeviceStatus::Installing
                } else if !e.installed && e.error.is_some() {
                    DeviceStatus::Error
                } else if e.installed && agents_enabled {
                    DeviceStatus::On
                } else {
                    DeviceStatus::Off
                };
                Device {
                    job_id: self.job_for(&id),
                    id,
                    kind: DeviceKind::ServerBrowser,
                    source_id: Some(e.engine_id),
                    name: e.name,
                    status,
                    available: DeviceAvailability::Now,
                    enabled: agents_enabled,
                    enabled_until: None,
                    installed: e.installed,
                    disk_bytes: e.disk_bytes,
                    version: e.version,
                    last_seen_at: None,
                    routes: None,
                    addon_host: None,
                }
            })
            .collect()
    }

    fn my_browser_cards(&self, user_id: &str) -> Vec<Device> {
        let enabled = self.browser.my_browsers_enabled(user_id);
        self.browser
            .my_browsers(user_id)
            .into_iter()
            .map(|b| Device {
                id: format!("my_browser:{}", b.device_id),
                kind: DeviceKind::MyBrowser,
                source_id: Some(b.device_id),
                name: format!(
                    "{} · {}",
                    b.name,
                    b.browser.split_whitespace().next().unwrap_or("Chrome")
                ),
                status: if !b.online {
                    DeviceStatus::Offline
                } else if enabled {
                    DeviceStatus::On
                } else {
                    DeviceStatus::Off
                },
                available: DeviceAvailability::Now,
                enabled,
                enabled_until: None,
                installed: true,
                disk_bytes: None,
                version: b.extension_version,
                job_id: None,
                last_seen_at: b.last_seen_at,
                routes: None,
                addon_host: None,
            })
            .collect()
    }

    fn row_card(&self, row: &DeviceRow, pairings: &HashMap<String, String>) -> Device {
        let kind = match row.kind.as_str() {
            "mac" => DeviceKind::Mac,
            "addon" => DeviceKind::Addon,
            _ => DeviceKind::Phone,
        };
        let meta = meta_of(row);
        let presence_key = if kind == DeviceKind::Addon {
            format!("addon:{}", row.source_id)
        } else {
            row.source_id.clone()
        };
        let online = match row.pairing_id.as_deref() {
            None => true,
            Some(p) => self.pairing_online(Some(p)) && self.relay_present(p, &presence_key),
        };
        // The Mac itself is "later" until the helper reports it (M3).
        let available = if kind == DeviceKind::Mac && meta.get("mac_reported").and_then(|v| v.as_bool()) != Some(true) {
            DeviceAvailability::Later
        } else {
            DeviceAvailability::Now
        };
        let status = if let Some(status) = self.job_status(&row.id) {
            status
        } else if !online {
            DeviceStatus::Offline
        } else if row.enabled {
            DeviceStatus::On
        } else {
            DeviceStatus::Off
        };
        let routes = (kind == DeviceKind::Phone).then(|| {
            let mut routes = Vec::new();
            if let Some(route) = row
                .pairing_id
                .as_ref()
                .and_then(|p| pairings.get(p))
                .and_then(|k| route_of_pairing_kind(k))
            {
                routes.push(DeviceRoute {
                    route,
                    enabled: row.enabled,
                    status,
                    experimental: false,
                });
            }
            if !routes.iter().any(|r| r.route == DeviceRouteKind::WifiDirect) {
                routes.push(DeviceRoute {
                    route: DeviceRouteKind::WifiDirect,
                    enabled: false,
                    status: DeviceStatus::Off,
                    experimental: true,
                });
            }
            routes
        });
        Device {
            id: row.id.clone(),
            kind,
            source_id: Some(row.source_id.clone()),
            name: row.name.clone(),
            status,
            available,
            enabled: row.enabled,
            enabled_until: row.enabled_until,
            installed: true,
            disk_bytes: meta.get("disk_bytes").and_then(|v| v.as_u64()),
            version: meta.get("version").and_then(|v| v.as_str()).map(str::to_owned),
            job_id: self.job_for(&row.id),
            last_seen_at: row.last_seen_at,
            routes,
            addon_host: (kind == DeviceKind::Addon).then(|| row.pairing_id.clone().unwrap_or_else(|| "server".into())),
        }
    }

    // -- switches -------------------------------------------------------------

    /// `PUT /api/devices/{id}`: the agent switch. Phones and the Mac need a
    /// timeout (default 30 minutes); it is ignored for other kinds.
    pub async fn update(&self, user_id: &str, id: &str, update: &DeviceUpdate) -> Result<Device, DevicesError> {
        let current = self.get(user_id, id).await?;
        if current.available == DeviceAvailability::Later {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceVerbRefused,
                "this device is not available yet",
            ));
        }
        match current.kind {
            DeviceKind::ServerBrowser => self
                .browser
                .set_server_browser_enabled(user_id, update.enabled)
                .await
                .map_err(DevicesError::Internal)?,
            DeviceKind::MyBrowser => self
                .browser
                .set_my_browsers_enabled(user_id, update.enabled)
                .await
                .map_err(DevicesError::Internal)?,
            DeviceKind::Phone | DeviceKind::Mac | DeviceKind::Addon => {
                if !current.installed {
                    return Err(DevicesError::coded(
                        DeviceErrorCode::DeviceVerbRefused,
                        "install it first",
                    ));
                }
                let until = if update.enabled && matches!(current.kind, DeviceKind::Phone | DeviceKind::Mac) {
                    let minutes = update.timeout_minutes.unwrap_or(DEFAULT_TIMEOUT_MINUTES);
                    if !DEVICE_TIMEOUT_MINUTES.contains(&minutes) {
                        return Err(DevicesError::coded(
                            DeviceErrorCode::DeviceBadMessage,
                            "timeout_minutes must be 15, 30 or 60",
                        ));
                    }
                    Some(now_ms() + i64::from(minutes) * 60_000)
                } else {
                    None
                };
                if !self
                    .repo
                    .set_device_enabled(user_id, id, update.enabled, until, now_ms())
                    .await?
                {
                    return Err(DevicesError::not_found());
                }
                // One phone, two routes: the Mac-relay row follows the card.
                for linked in self.linked_rows(user_id, id).await? {
                    self.repo
                        .set_device_enabled(user_id, &linked.id, update.enabled, until, now_ms())
                        .await?;
                    if !update.enabled
                        && let Some(p) = &linked.pairing_id
                    {
                        self.relay_stop(p, &linked.source_id);
                    }
                }
                if !update.enabled {
                    // Off: stop the device and refuse what waits for an OK.
                    if let Some(row) = self.repo.get_device(user_id, id).await?
                        && let Some(p) = &row.pairing_id
                    {
                        self.relay_stop(p, &row.source_id);
                    }
                    self.drop_device_approvals(user_id, id);
                }
            }
        }
        info!(
            user_id,
            device_id = id,
            enabled = update.enabled,
            "device switch changed"
        );
        self.changed(user_id, Some(id));
        self.get(user_id, id).await
    }

    /// `PUT /api/devices/{id}/routes/{route}`. v1: a phone has the route of
    /// the relay that reaches it (its switch) and Wi-Fi direct, which stays
    /// refused until the T0 test passes (K4).
    pub async fn update_route(
        &self,
        user_id: &str,
        id: &str,
        route: DeviceRouteKind,
        enabled: bool,
    ) -> Result<Device, DevicesError> {
        let current = self.get(user_id, id).await?;
        let Some(routes) = &current.routes else {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceBadMessage,
                "this device has no routes",
            ));
        };
        let Some(found) = routes.iter().find(|r| r.route == route) else {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceBadMessage,
                "this phone has no such route",
            ));
        };
        if found.experimental && enabled {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceVerbRefused,
                "Wi-Fi direct needs a test first",
            ));
        }
        self.update(
            user_id,
            id,
            &DeviceUpdate {
                enabled,
                timeout_minutes: None,
            },
        )
        .await
    }

    /// The Mac-relay rows that are the same phone as card `id`.
    pub(crate) async fn linked_rows(&self, user_id: &str, id: &str) -> Result<Vec<DeviceRow>, DevicesError> {
        let pairings: HashMap<String, String> = self
            .repo
            .list_pairings(user_id)
            .await?
            .into_iter()
            .map(|p| (p.id, p.kind))
            .collect();
        Ok(self
            .repo
            .list_devices(user_id)
            .await?
            .into_iter()
            .filter(|r| r.pairing_id.as_ref().is_some_and(|p| pairings.contains_key(p)))
            .filter(|r| companion_card_of(r, &pairings).as_deref() == Some(id))
            .collect())
    }

    /// Run `verb` on the phone behind card `id` over whichever route serves
    /// it: the Mac relay first (it also serves `logcat`, `install_apk`,
    /// Chrome), else the companion app.
    pub async fn routed_call(
        &self,
        user_id: &str,
        id: &str,
        verb: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, DevicesError> {
        for linked in self.linked_rows(user_id, id).await? {
            let usable = linked.pairing_id.as_deref().is_some_and(|p| {
                self.relay_client(user_id, p)
                    .is_some_and(|c| c.is_present(&linked.source_id) && c.serves(verb))
            });
            if usable {
                return self.relay_call(user_id, &linked.id, verb, args).await;
            }
        }
        self.relay_call(user_id, id, verb, args).await
    }

    /// Whether the user has a phone or Mac at all (paired relay client): the
    /// `aionui-devices` MCP is only given to conversations of such users.
    pub async fn has_agent_devices(&self, user_id: &str) -> bool {
        let Ok(pairings) = self.repo.list_pairings(user_id).await else {
            return false;
        };
        !pairings.is_empty()
    }

    /// Whether agents may use the device right now (switch on, not expired).
    pub async fn is_enabled(&self, user_id: &str, id: &str) -> Result<bool, DevicesError> {
        let row = self
            .repo
            .get_device(user_id, id)
            .await?
            .ok_or_else(DevicesError::not_found)?;
        Ok(row.enabled && row.enabled_until.is_none_or(|until| until > now_ms()))
    }

    // -- jobs -----------------------------------------------------------------

    fn job_for(&self, device_id: &str) -> Option<String> {
        self.jobs
            .iter()
            .find(|e| {
                e.job.device_id == device_id && matches!(e.job.state, DeviceJobState::Queued | DeviceJobState::Running)
            })
            .map(|e| e.job.job_id.clone())
    }

    fn job_status(&self, device_id: &str) -> Option<DeviceStatus> {
        self.jobs
            .iter()
            .find(|e| {
                e.job.device_id == device_id && matches!(e.job.state, DeviceJobState::Queued | DeviceJobState::Running)
            })
            .map(|e| {
                if e.removing {
                    DeviceStatus::Removing
                } else {
                    DeviceStatus::Installing
                }
            })
    }

    /// `POST /api/devices/{id}/install|remove`.
    pub async fn start_job(self: &Arc<Self>, user_id: &str, id: &str, remove: bool) -> Result<String, DevicesError> {
        let device = self.get(user_id, id).await?;
        // An installed add-on is removed by its installer (the Mac frees the
        // disk, K5); phones and the helper's own card are forgotten / revoked.
        if remove
            && !crate::addons::removes_through_installer(device.kind)
            && let Some(job_id) = self.remove_relay_device(user_id, id).await?
        {
            return Ok(job_id);
        }
        if device.available == DeviceAvailability::Later {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceVerbRefused,
                "this device is not available yet",
            ));
        }
        if self.job_for(id).is_some() {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceBusy,
                "an install or remove is already running",
            ));
        }
        let installer = self
            .installers
            .read()
            .expect("installers lock")
            .iter()
            .find(|i| i.handles(id))
            .cloned()
            .ok_or_else(|| {
                DevicesError::coded(
                    DeviceErrorCode::DeviceVerbRefused,
                    "this device cannot be installed here",
                )
            })?;
        if remove {
            installer.check_remove(user_id, id).await?;
        }
        let job_id = format!("job_{}", uuid::Uuid::new_v4().simple());
        self.jobs.insert(
            job_id.clone(),
            JobEntry {
                user_id: user_id.to_owned(),
                removing: remove,
                job: DeviceJob {
                    job_id: job_id.clone(),
                    device_id: id.to_owned(),
                    state: DeviceJobState::Queued,
                    progress: 0,
                    error: None,
                },
            },
        );
        self.changed(user_id, Some(id));
        let svc = self.clone();
        let (user, device_id, jid) = (user_id.to_owned(), id.to_owned(), job_id.clone());
        tokio::spawn(async move {
            svc.set_job(&jid, DeviceJobState::Running, 0, None);
            let jobs = svc.jobs.clone();
            let progress_id = jid.clone();
            let progress = move |p: u8| {
                if let Some(mut e) = jobs.get_mut(&progress_id) {
                    e.job.progress = p.min(100);
                }
            };
            let result = if remove {
                installer.remove(&user, &device_id, &progress).await
            } else {
                installer.install(&user, &device_id, &progress).await
            };
            match result {
                Ok(()) => {
                    info!(user_id = %user, device_id = %device_id, remove, "device job done");
                    svc.set_job(&jid, DeviceJobState::Done, 100, None);
                }
                Err(error) => {
                    warn!(user_id = %user, device_id = %device_id, remove, error = %error, "device job failed");
                    svc.set_job(&jid, DeviceJobState::Failed, 100, Some(error.to_string()));
                }
            }
            svc.changed(&user, Some(&device_id));
        });
        Ok(job_id)
    }

    /// Remove of a device a relay client reaches: the client's own card
    /// (`mac` / companion `self`) revokes its pairing (token dead, socket
    /// closed, its devices off); a phone behind a Mac helper is forgotten
    /// (it comes back with the next inventory). Done at once; the job is
    /// recorded as `done` for the UI's polling.
    async fn remove_relay_device(&self, user_id: &str, id: &str) -> Result<Option<String>, DevicesError> {
        let Some(row) = self.repo.get_device(user_id, id).await? else {
            return Ok(None);
        };
        let Some(pairing_id) = row.pairing_id.clone() else {
            return Ok(None);
        };
        let own_card = matches!(
            row.source_id.as_str(),
            aionui_api_types::devices::RELAY_DEVICE_MAC | aionui_api_types::devices::RELAY_DEVICE_SELF
        );
        if own_card {
            self.repo.revoke_pairing(user_id, &pairing_id, now_ms()).await?;
            self.relay_disconnect(&pairing_id);
            self.set_pairing_online(&pairing_id, false);
            info!(user_id, pairing_id = %pairing_id, "device relay pairing revoked");
        } else {
            self.relay_stop(&pairing_id, &row.source_id);
            self.repo.delete_device(user_id, id).await?;
        }
        self.drop_device_approvals(user_id, id);
        let job_id = format!("job_{}", uuid::Uuid::new_v4().simple());
        self.jobs.insert(
            job_id.clone(),
            JobEntry {
                user_id: user_id.to_owned(),
                removing: true,
                job: DeviceJob {
                    job_id: job_id.clone(),
                    device_id: id.to_owned(),
                    state: DeviceJobState::Done,
                    progress: 100,
                    error: None,
                },
            },
        );
        self.changed(user_id, None);
        Ok(Some(job_id))
    }

    /// Run one verb on a relay device (no switch / approval checks: the agent
    /// surface does those, K3).
    pub async fn relay_call(
        &self,
        user_id: &str,
        device_id: &str,
        verb: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, DevicesError> {
        let row = self
            .repo
            .get_device(user_id, device_id)
            .await?
            .ok_or_else(DevicesError::not_found)?;
        let Some(pairing_id) = row.pairing_id.as_deref() else {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceVerbRefused,
                "this device has no relay",
            ));
        };
        let offline = || DevicesError::coded(DeviceErrorCode::DeviceOffline, "the device is offline");
        let client = self.relay_client(user_id, pairing_id).ok_or_else(offline)?;
        if !client.is_present(&row.source_id) {
            return Err(offline());
        }
        // Never route a verb the client did not announce (the companion app
        // serves no logcat / install_apk / cdp_*).
        if !client.serves(verb) {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceVerbRefused,
                format!("{verb} is not available on this device"),
            ));
        }
        client
            .call(&row.source_id, verb, args)
            .await
            .map_err(|e| DevicesError::coded(relay_error_code(&e), e.message))
    }

    fn set_job(&self, job_id: &str, state: DeviceJobState, progress: u8, error: Option<String>) {
        if let Some(mut e) = self.jobs.get_mut(job_id) {
            e.job.state = state;
            if progress > e.job.progress || matches!(state, DeviceJobState::Done | DeviceJobState::Failed) {
                e.job.progress = progress;
            }
            e.job.error = error;
        }
    }

    /// `GET /api/devices/jobs/{job_id}`: only the job's owner sees it.
    pub fn job(&self, user_id: &str, job_id: &str) -> Result<DeviceJob, DevicesError> {
        self.jobs
            .get(job_id)
            .filter(|e| e.user_id == user_id)
            .map(|e| e.job.clone())
            .ok_or_else(|| DevicesError::coded(DeviceErrorCode::DeviceJobNotFound, "no such job"))
    }

    // -- audit ----------------------------------------------------------------

    pub async fn record(&self, user_id: &str, device_id: &str, note: AuditNote<'_>) {
        let AuditNote {
            verb,
            package,
            origin,
            outcome,
            conversation_id,
            detail,
        } = note;
        let outcome = serde_json::to_value(outcome)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        if let Err(error) = self
            .repo
            .append_audit(NewDeviceAuditEntry {
                user_id,
                device_id,
                at: now_ms(),
                verb,
                package,
                origin,
                outcome: &outcome,
                conversation_id,
                detail,
            })
            .await
        {
            warn!(user_id, device_id, verb, error = %error, "device audit row not written");
        }
    }

    /// `GET /api/devices/{id}/audit?cursor=&limit=`, newest first.
    pub async fn audit(
        &self,
        user_id: &str,
        device_id: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<DeviceAuditPage, DevicesError> {
        self.get(user_id, device_id).await?;
        let before = match cursor {
            None | Some("") => None,
            Some(c) => Some(
                c.parse::<i64>()
                    .map_err(|_| DevicesError::coded(DeviceErrorCode::DeviceBadMessage, "bad cursor"))?,
            ),
        };
        let limit = limit.unwrap_or(AUDIT_PAGE_DEFAULT).clamp(1, AUDIT_PAGE_MAX);
        let rows = self.repo.list_audit(user_id, device_id, before, limit + 1).await?;
        let more = rows.len() > limit as usize;
        let rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
        let next_cursor = if more {
            rows.last().map(|r| r.id.to_string())
        } else {
            None
        };
        let rows = rows
            .into_iter()
            .map(|r| DeviceAuditEntry {
                at: r.at,
                verb: r.verb,
                package: r.package,
                origin: r.origin,
                outcome: serde_json::from_value(serde_json::Value::String(r.outcome))
                    .unwrap_or(DeviceAuditOutcome::Error),
                conversation_id: r.conversation_id,
                detail: r.detail,
            })
            .collect();
        Ok(DeviceAuditPage { rows, next_cursor })
    }
}

#[cfg(test)]
mod tests;
