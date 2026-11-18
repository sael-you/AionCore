//! The browser service (C30): settings, Chrome install/update, instance
//! lifecycle (start on demand, idle stop, clear), and the agents' bridges.
//! Sole place for business logic; `routes.rs` only translates.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use aionui_api_types::{
    BROWSER_ENGINES, BrowserEngine, BrowserEngineStatus, BrowserErrorCode, BrowserInstanceCreate, BrowserInstancePatch,
    BrowserInstanceStatus, BrowserSettings, BrowserSettingsUpdate, BrowserStatusResponse,
};
use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::approvals::{ApprovalBroker, BrowserEventSink};
use crate::bridge::{AgentBridge, BridgeBackend};
use crate::device_bridge::{DeviceBackend, DeviceListener, ENV_DEVICES, start_listener};
use crate::devices::DeviceStore;
use crate::error::BrowserError;
use crate::guard::NavigationGuard;
use crate::host::{
    HostPaths, ensure_browser_user, existing_browser_user, harden_and_verify, prepare_dirs, reap_strays,
};
use crate::install::{EngineInstaller, installed_version};
use crate::instance::{ChromeInstance, LaunchSpec};
use crate::layout::BrowserLayout;
use crate::panel::PanelBackend;
use crate::relay::{DeviceHub, instance_name};
use crate::settings::SettingsStore;

/// Default idle stop (no panel input and no agent traffic).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct BrowserServiceConfig {
    pub layout: BrowserLayout,
    /// `None` skips the host hardening (tests, non-standard roots).
    pub host: Option<HostPaths>,
    pub idle_timeout: Duration,
    pub guard: NavigationGuard,
    /// Use this browser binary instead of the managed installs, for every
    /// engine (dev/tests).
    pub chrome_override: Option<PathBuf>,
    /// Run Chrome as this (uid, gid) instead of the `aionui-browser` account (tests).
    pub run_as_override: Option<(u32, u32)>,
}

type Key = (String, u8);

#[derive(Debug, Clone, Default)]
struct InstallState {
    installing: bool,
    error: Option<String>,
}

pub struct BrowserService {
    /// This service's own `Arc` (trait methods only get `&self`).
    me: std::sync::Weak<BrowserService>,
    cfg: BrowserServiceConfig,
    settings: SettingsStore,
    installer: EngineInstaller,
    instances: DashMap<Key, Arc<ChromeInstance>>,
    start_locks: DashMap<Key, Arc<Mutex<()>>>,
    max_tabs: DashMap<String, Arc<AtomicU8>>,
    bridges: DashMap<String, AgentBridge>,
    bridge_lock: Mutex<()>,
    install_lock: Mutex<()>,
    /// Reap + launch run one at a time: a reap must never see a Chrome that
    /// is launching but not yet registered.
    launch_lock: Mutex<()>,
    /// Per engine: a download is running / why the last one failed.
    install_state: std::sync::Mutex<std::collections::HashMap<BrowserEngine, InstallState>>,
    guard: Arc<NavigationGuard>,
    /// C34: paired devices of the owner's own browser.
    devices: Arc<DeviceHub>,
    device_listeners: DashMap<String, DeviceListener>,
    device_lock: Mutex<()>,
}

impl BrowserService {
    pub fn new(cfg: BrowserServiceConfig) -> Result<Arc<Self>, BrowserError> {
        let installer = EngineInstaller::new(cfg.layout.clone())?;
        let settings = SettingsStore::new(cfg.layout.settings_dir());
        let guard = Arc::new(cfg.guard.clone());
        let devices = Arc::new(DeviceHub::new(
            DeviceStore::open(cfg.layout.devices_dir()),
            Arc::new(ApprovalBroker::default()),
            guard.clone(),
        ));
        Ok(Arc::new_cyclic(|me| Self {
            me: me.clone(),
            cfg,
            settings,
            installer,
            instances: DashMap::new(),
            start_locks: DashMap::new(),
            max_tabs: DashMap::new(),
            bridges: DashMap::new(),
            bridge_lock: Mutex::new(()),
            install_lock: Mutex::new(()),
            launch_lock: Mutex::new(()),
            install_state: std::sync::Mutex::new(std::collections::HashMap::new()),
            guard,
            devices,
            device_listeners: DashMap::new(),
            device_lock: Mutex::new(()),
        }))
    }

    /// Orphan cleanup, idle reaper + daily update check (call once at startup).
    pub fn spawn_background(self: &Arc<Self>) {
        // A restart (every deploy) leaves Chrome and Xvfb running as the
        // browser account: root cannot signal them, so they would keep their
        // profile lock and their CDP port. Stop them first.
        let svc = self.clone();
        tokio::spawn(async move {
            let _launching = svc.launch_lock.lock().await;
            svc.reap_orphans().await;
        });
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let Some(svc) = weak.upgrade() else { return };
                svc.reap_idle().await;
            }
        });
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(UPDATE_EVERY).await;
                let Some(svc) = weak.upgrade() else { return };
                svc.update_check().await;
            }
        });
    }

    // -- settings ---------------------------------------------------------

    pub fn settings(&self, user_id: &str) -> Result<BrowserSettings, BrowserError> {
        let s = self.settings.load(user_id)?;
        self.max_tabs_cell(user_id).store(s.max_tabs, Ordering::Relaxed);
        Ok(s)
    }

    pub async fn update_settings(
        &self,
        user_id: &str,
        update: &BrowserSettingsUpdate,
    ) -> Result<BrowserSettings, BrowserError> {
        let s = self.settings.update(user_id, update)?;
        self.max_tabs_cell(user_id).store(s.max_tabs, Ordering::Relaxed);
        tracing::info!(
            user_id,
            agents_enabled = s.agents_enabled,
            max_instances = s.max_instances,
            max_tabs = s.max_tabs,
            "browser settings updated"
        );
        Ok(s)
    }

    fn max_tabs_cell(&self, user_id: &str) -> Arc<AtomicU8> {
        self.max_tabs
            .entry(user_id.to_owned())
            .or_insert_with(|| Arc::new(AtomicU8::new(BrowserSettings::default().max_tabs)))
            .clone()
    }

    fn agents_enabled_now(&self, user_id: &str) -> bool {
        self.settings.load(user_id).map(|s| s.agents_enabled).unwrap_or(false)
    }

    // -- status -----------------------------------------------------------

    /// The installed version of `engine` (`"override"` with a dev binary).
    fn engine_available(&self, engine: BrowserEngine) -> Option<String> {
        match &self.cfg.chrome_override {
            Some(p) => p.exists().then(|| "override".to_owned()),
            None => installed_version(&self.cfg.layout, engine),
        }
    }

    fn set_install_state(&self, engine: BrowserEngine, change: impl FnOnce(&mut InstallState)) {
        let mut states = self.install_state.lock().unwrap_or_else(|p| p.into_inner());
        change(states.entry(engine).or_default());
    }

    fn engine_statuses(&self) -> Vec<BrowserEngineStatus> {
        let states = self.install_state.lock().unwrap_or_else(|p| p.into_inner()).clone();
        BROWSER_ENGINES
            .into_iter()
            .map(|engine| {
                let spec = crate::engine::spec(engine);
                let state = states.get(&engine).cloned().unwrap_or_default();
                BrowserEngineStatus {
                    engine,
                    installed_version: self.engine_available(engine),
                    installing: state.installing,
                    error: state.error,
                    verification: spec.verification(),
                    agents_only: spec.agents_only(),
                }
            })
            .collect()
    }

    pub fn status(self: &Arc<Self>, user_id: &str) -> Result<BrowserStatusResponse, BrowserError> {
        let s = self.settings(user_id)?;
        // `available` is about the default engine (Chrome), as before C35.
        let version = self.engine_available(BrowserEngine::Chrome);
        if version.is_none() {
            // First use: install in the background; the UI polls status.
            let svc = self.clone();
            tokio::spawn(async move {
                let _ = svc.ensure_engine(BrowserEngine::Chrome).await;
            });
        }
        let instances = s
            .instances
            .iter()
            .map(|row| {
                let inst = self.running(user_id, row.instance);
                BrowserInstanceStatus {
                    instance: row.instance,
                    name: row.name.clone(),
                    color: row.color.clone(),
                    emoji: row.emoji.clone(),
                    running: inst.is_some(),
                    tabs: inst.as_ref().map(|i| i.tab_count() as u16).unwrap_or(0),
                    agent_active: inst.as_ref().is_some_and(|i| i.agent_active()),
                    hide_automation_active: inst.as_ref().is_some_and(|i| i.hide_automation),
                    engine: row.engine,
                    engine_active: inst.as_ref().map(|i| i.engine),
                }
            })
            .collect();
        Ok(BrowserStatusResponse {
            available: version.is_some(),
            unavailable_reason: version.is_none().then_some(BrowserErrorCode::BrowserUnavailable),
            chrome_version: version,
            instances,
            idle_timeout_secs: self.cfg.idle_timeout.as_secs(),
            engines: self.engine_statuses(),
        })
    }

    // -- install ------------------------------------------------------------

    /// The binary of `engine` to launch; installs on first use (single flight).
    async fn ensure_engine(&self, engine: BrowserEngine) -> Result<PathBuf, BrowserError> {
        if let Some(p) = &self.cfg.chrome_override {
            return Ok(p.clone());
        }
        let binary = self.cfg.layout.engine_binary(engine);
        if installed_version(&self.cfg.layout, engine).is_some() {
            return Ok(binary);
        }
        let _guard = self.install_lock.lock().await;
        if installed_version(&self.cfg.layout, engine).is_some() {
            return Ok(binary);
        }
        self.install_latest(engine).await.map_err(|_| {
            BrowserError::coded(
                BrowserErrorCode::BrowserUnavailable,
                format!("the {} engine is not installed", engine.id()),
            )
        })?;
        Ok(binary)
    }

    /// Download the newest release of `engine` and switch to it unless an
    /// instance of that engine runs (then the next check switches). The
    /// caller holds `install_lock`.
    async fn install_latest(&self, engine: BrowserEngine) -> Result<(), BrowserError> {
        self.set_install_state(engine, |s| s.installing = true);
        let result = async {
            let release = self.installer.latest(engine).await?;
            let version = self.installer.install(engine, &release).await?;
            let current = installed_version(&self.cfg.layout, engine);
            let in_use = self.instances.iter().any(|e| e.value().engine == engine);
            // Forward only: `install` already refused anything older than the
            // highest version ever installed; `activate` refuses the rest.
            let newer = current
                .as_deref()
                .is_none_or(|c| crate::install::version_newer(&version, c));
            if newer && (current.is_none() || !in_use) {
                self.installer.activate(engine, &version)?;
                tracing::info!(engine = engine.id(), %version, verification = ?crate::engine::spec(engine).verification(), "server browser engine installed");
            }
            Ok::<_, BrowserError>(())
        }
        .await;
        if let Err(e) = &result {
            tracing::error!(engine = engine.id(), error = %e, "server browser engine install failed");
        }
        let error = result.as_ref().err().map(|e| {
            let mut text = e.to_string();
            text.truncate(300);
            text
        });
        self.set_install_state(engine, |s| {
            s.installing = false;
            s.error = error;
        });
        result
    }

    /// Settings → Browser "Download" / "Update" (C35): fetch the newest
    /// release of `engine` in the background; the UI polls `status`.
    pub fn install_engine(
        self: &Arc<Self>,
        user_id: &str,
        engine: BrowserEngine,
    ) -> Result<BrowserStatusResponse, BrowserError> {
        if self.cfg.chrome_override.is_none() {
            // Shown at once; the task below owns the state from here.
            self.set_install_state(engine, |s| s.installing = true);
            let svc = self.clone();
            tokio::spawn(async move {
                let _guard = svc.install_lock.lock().await;
                let _ = svc.install_latest(engine).await;
            });
        }
        self.status(user_id)
    }

    /// Daily: install a newer release of every installed engine; switch to it
    /// only while no instance of that engine runs.
    async fn update_check(&self) {
        if self.cfg.chrome_override.is_some() {
            return;
        }
        for engine in BROWSER_ENGINES {
            if installed_version(&self.cfg.layout, engine).is_none() {
                continue;
            }
            let _guard = self.install_lock.lock().await;
            if let Err(e) = self.install_latest(engine).await {
                tracing::warn!(engine = engine.id(), error = %e, "server browser engine update check failed");
            }
        }
    }

    // -- instances --------------------------------------------------------

    /// Stop every process of the browser account that no tracked instance
    /// owns. Returns how many processes were found.
    pub async fn reap_orphans(&self) -> usize {
        let Some((uid, gid)) = self.cfg.run_as_override.or_else(existing_browser_user) else {
            return 0;
        };
        let keep: Vec<u32> = self.instances.iter().flat_map(|e| e.value().process_groups()).collect();
        let profiles: Vec<PathBuf> = self.instances.iter().map(|e| e.value().profile.clone()).collect();
        reap_strays(uid, gid, &keep, &profiles).await
    }

    pub fn running(&self, user_id: &str, n: u8) -> Option<Arc<ChromeInstance>> {
        let key = (user_id.to_owned(), n);
        let inst = self.instances.get(&key)?.clone();
        if inst.is_running() {
            Some(inst)
        } else {
            self.instances.remove(&key);
            None
        }
    }

    /// Instance `n` must exist for `user_id` (404 `BROWSER_INSTANCE_NOT_FOUND`).
    fn check_instance(&self, user_id: &str, n: u8) -> Result<BrowserSettings, BrowserError> {
        let s = self.settings(user_id)?;
        if !s.instances.iter().any(|i| i.instance == n) {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserInstanceNotFound,
                format!("no instance {n}"),
            ));
        }
        Ok(s)
    }

    /// Whether instance `n` exists for `user_id`.
    pub fn instance_exists(&self, user_id: &str, n: u8) -> bool {
        self.settings.exists(user_id, n).unwrap_or(false)
    }

    // -- instances (TASK-124 named profiles) ----------------------------------

    pub fn create_instance(&self, user_id: &str, req: &BrowserInstanceCreate) -> Result<BrowserSettings, BrowserError> {
        let (s, n) = self.settings.create(user_id, req)?;
        tracing::info!(user_id, instance = n, "browser instance created");
        Ok(s)
    }

    pub fn patch_instance(
        &self,
        user_id: &str,
        n: u8,
        req: &BrowserInstancePatch,
    ) -> Result<BrowserSettings, BrowserError> {
        self.settings.patch(user_id, n, req)
    }

    /// Stop instance `n`, wipe its profile (as the account) and remove it.
    pub async fn delete_instance(self: &Arc<Self>, user_id: &str, n: u8) -> Result<BrowserSettings, BrowserError> {
        if n == 1 {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserInstanceProtected,
                "the main instance cannot be deleted",
            ));
        }
        self.check_instance(user_id, n)?;
        let key = (user_id.to_owned(), n);
        let lock = self.start_locks.entry(key.clone()).or_default().clone();
        let Ok(_held) = lock.try_lock() else {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserBusy,
                "the profile is busy",
            ));
        };
        if let Some(inst) = self.running(user_id, n) {
            inst.stop().await;
            self.instances.remove(&key);
        }
        let profile = self.cfg.layout.profile_dir(user_id, n)?;
        if let Some((uid, gid)) = self.cfg.run_as_override.or_else(existing_browser_user) {
            crate::host::wipe_profile(&profile, uid, gid).await?;
        }
        let s = self.settings.delete(user_id, n)?;
        tracing::info!(user_id, instance = n, "browser instance deleted and its profile wiped");
        Ok(s)
    }

    /// Start instance `n` for `user_id` (or return the running one). Every
    /// failure is logged and carries a code the panel can show.
    pub async fn start(self: &Arc<Self>, user_id: &str, n: u8) -> Result<Arc<ChromeInstance>, BrowserError> {
        self.start_inner(user_id, n)
            .await
            .map_err(|e| start_failure(user_id, n, e))
    }

    async fn start_inner(self: &Arc<Self>, user_id: &str, n: u8) -> Result<Arc<ChromeInstance>, BrowserError> {
        self.check_instance(user_id, n)?;
        if let Some(i) = self.running(user_id, n) {
            return Ok(i);
        }
        let key = (user_id.to_owned(), n);
        let lock = self.start_locks.entry(key.clone()).or_default().clone();
        let _held = lock.lock().await;
        if let Some(i) = self.running(user_id, n) {
            return Ok(i);
        }
        let engine = self.settings.engine(user_id, n)?;
        let chrome = self.ensure_engine(engine).await?;
        if let Some(host) = &self.cfg.host {
            harden_and_verify(host)?;
        }
        let (uid, gid) = match self.cfg.run_as_override {
            Some(ids) => ids,
            None => ensure_browser_user().await?,
        };
        let profile = prepare_dirs(&self.cfg.layout, user_id, n, uid, gid).await?;
        let launching = self.launch_lock.lock().await;
        // Leftovers of a crashed or restarted AionCore hold profile locks.
        self.reap_orphans().await;
        let inst = ChromeInstance::launch(LaunchSpec {
            user_id: user_id.to_owned(),
            instance: n,
            engine,
            chrome,
            home: crate::host::instance_home(&profile),
            profile,
            uid,
            gid,
            hide_automation: self.settings.hide_automation(user_id, n)?,
            max_tabs: self.max_tabs_cell(user_id),
            guard: self.guard.clone(),
        })
        .await?;
        self.instances.insert(key.clone(), inst.clone());
        drop(launching);
        let weak = Arc::downgrade(self);
        let watched = inst.clone();
        tokio::spawn(async move {
            watched.exited().await;
            if let Some(svc) = weak.upgrade() {
                svc.instances.remove_if(&key, |_, v| Arc::ptr_eq(v, &watched));
            }
        });
        Ok(inst)
    }

    pub async fn stop(self: &Arc<Self>, user_id: &str, n: u8) -> Result<BrowserStatusResponse, BrowserError> {
        if let Some(inst) = self.running(user_id, n) {
            inst.stop().await;
            self.instances.remove(&(user_id.to_owned(), n));
        }
        self.status(user_id)
    }

    /// Wipe instance `n`'s profile (all logins); restart if it was running.
    pub async fn clear(self: &Arc<Self>, user_id: &str, n: u8) -> Result<BrowserStatusResponse, BrowserError> {
        self.check_instance(user_id, n)?;
        let key = (user_id.to_owned(), n);
        let lock = self.start_locks.entry(key.clone()).or_default().clone();
        let Ok(held) = lock.try_lock() else {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserBusy,
                "the profile is busy",
            ));
        };
        let was_running = match self.running(user_id, n) {
            Some(inst) => {
                inst.stop().await;
                self.instances.remove(&key);
                true
            }
            None => false,
        };
        let profile = self.cfg.layout.profile_dir(user_id, n)?;
        // The profile lives in an account-owned directory: the wipe runs as
        // the account, never as root (a planted symlink must not redirect it).
        if let Some((uid, gid)) = self.cfg.run_as_override.or_else(existing_browser_user) {
            crate::host::wipe_profile(&profile, uid, gid).await?;
        }
        tracing::info!(user_id, instance = n, "server browser profile cleared");
        drop(held);
        if was_running {
            self.start(user_id, n).await?;
        }
        self.status(user_id)
    }

    async fn reap_idle(&self) {
        let idle: Vec<Arc<ChromeInstance>> = self
            .instances
            .iter()
            .filter(|e| e.value().idle_ms() > self.cfg.idle_timeout.as_millis() as i64)
            .map(|e| e.value().clone())
            .collect();
        for inst in idle {
            tracing::info!(user_id = %inst.user_id, instance = inst.instance, "server browser idle; stopping");
            inst.stop().await;
            self.instances
                .remove_if(&(inst.user_id.clone(), inst.instance), |_, v| Arc::ptr_eq(v, &inst));
        }
    }

    // -- agents -------------------------------------------------------------

    /// Env for an agent of `user_id`: the bridge ports + token, or nothing
    /// while "Agents can use the browser" is off or Chrome is unavailable.
    /// With "Agents may use my Mac browser" on, also `AIONUI_CDP_DEVICES`.
    pub async fn agent_env(self: &Arc<Self>, user_id: &str) -> Vec<(String, String)> {
        if !self.agents_enabled_now(user_id) {
            return Vec::new();
        }
        let Some(bridge) = self.ensure_bridge(user_id).await else {
            return Vec::new();
        };
        let mut env = bridge.env();
        // Name → port of the existing instances, as at agent spawn.
        let mut instances: Vec<serde_json::Value> = self
            .settings(user_id)
            .map(|s| s.instances)
            .unwrap_or_default()
            .iter()
            .filter_map(|row| {
                bridge
                    .port_of(row.instance)
                    .map(|port| serde_json::json!({"instance": row.instance, "name": row.name, "port": port}))
            })
            .collect();
        // C34: the owner's own browser is one more entry next to the named
        // server profiles ("Mac · Chrome", `kind: "device"`, no profile number),
        // so picking a profile by name finds it too.
        let devices = if self.devices_allowed_now(user_id) {
            self.device_env(user_id, bridge.token()).await
        } else {
            Vec::new()
        };
        instances.extend(devices.iter().map(|d| {
            serde_json::json!({"kind": "device", "device_id": d["device_id"], "name": d["name"], "port": d["port"]})
        }));
        env.push((
            crate::bridge::ENV_INSTANCES.to_owned(),
            serde_json::Value::Array(instances).to_string(),
        ));
        if !devices.is_empty() {
            env.push((ENV_DEVICES.to_owned(), serde_json::Value::Array(devices).to_string()));
        }
        env
    }

    async fn ensure_bridge(self: &Arc<Self>, user_id: &str) -> Option<AgentBridge> {
        if let Some(b) = self.bridges.get(user_id) {
            return Some(b.clone());
        }
        let _held = self.bridge_lock.lock().await;
        if let Some(b) = self.bridges.get(user_id) {
            return Some(b.clone());
        }
        let backend: Arc<dyn BridgeBackend> = self.clone();
        match AgentBridge::start(user_id, backend).await {
            Ok(bridge) => {
                self.bridges.insert(user_id.to_owned(), bridge.clone());
                Some(bridge)
            }
            Err(e) => {
                tracing::warn!(user_id, error = %e, "browser agent bridge did not start");
                None
            }
        }
    }

    /// One loopback listener per paired device of the user (started once).
    async fn device_env(self: &Arc<Self>, user_id: &str, token: Arc<String>) -> Vec<serde_json::Value> {
        let _held = self.device_lock.lock().await;
        let mut rows = Vec::new();
        for d in self.devices.store().devices(user_id) {
            let port = match self.device_listeners.get(&d.device_id) {
                Some(l) => l.port,
                None => {
                    let backend: Arc<dyn DeviceBackend> = self.clone();
                    match start_listener(user_id, &d.device_id, token.clone(), self.devices.clone(), backend).await {
                        Ok(l) => {
                            let port = l.port;
                            self.device_listeners.insert(d.device_id.clone(), l);
                            port
                        }
                        Err(e) => {
                            tracing::warn!(user_id, device_id = %d.device_id, error = %e, "browser device bridge did not start");
                            continue;
                        }
                    }
                }
            };
            rows.push(serde_json::json!({
                "device_id": d.device_id,
                "name": instance_name(&d.name, &d.browser),
                "port": port,
            }));
        }
        rows
    }

    fn devices_allowed_now(&self, user_id: &str) -> bool {
        self.settings
            .load(user_id)
            .map(|s| s.agents_enabled && s.devices_agents_enabled)
            .unwrap_or(false)
    }

    // -- devices (C34) ------------------------------------------------------

    pub fn device_hub(&self) -> &Arc<DeviceHub> {
        &self.devices
    }

    /// Where approval and device events go (the app's per-user sockets).
    pub fn set_event_sink(&self, sink: Arc<dyn BrowserEventSink>) {
        self.devices.approvals().set_sink(sink);
    }

    /// Revoke a device: its token, socket, approvals and agent listener.
    pub fn revoke_device(&self, user_id: &str, device_id: &str) -> Result<(), BrowserError> {
        self.devices.revoke(user_id, device_id)?;
        self.device_listeners.remove_if(device_id, |_, l| l.user_id == user_id);
        Ok(())
    }

    /// Whether the `browser-safety` skill should be delivered.
    pub fn agents_enabled(&self, user_id: &str) -> bool {
        self.agents_enabled_now(user_id)
    }

    pub fn guard_ref(&self) -> Arc<NavigationGuard> {
        self.guard.clone()
    }
}

#[async_trait::async_trait]
impl BridgeBackend for BrowserService {
    async fn agent_instance(&self, user_id: &str, instance: u8) -> Result<Arc<ChromeInstance>, BrowserError> {
        if !self.agents_enabled_now(user_id) {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserAgentsDisabled,
                "agents may not use the browser",
            ));
        }
        // `start` needs an Arc; the bridge only exists for an Arc'd service.
        let me = self
            .self_arc()
            .ok_or_else(|| BrowserError::Internal("browser service gone".into()))?;
        me.start(user_id, instance).await
    }

    fn agents_enabled(&self, user_id: &str) -> bool {
        self.agents_enabled_now(user_id)
    }
}

impl DeviceBackend for BrowserService {
    fn devices_allowed(&self, user_id: &str) -> bool {
        self.devices_allowed_now(user_id)
    }

    fn max_tabs(&self, user_id: &str) -> u8 {
        self.max_tabs_cell(user_id).load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl PanelBackend for BrowserService {
    fn running(&self, user_id: &str, instance: u8) -> Option<Arc<ChromeInstance>> {
        BrowserService::running(self, user_id, instance)
    }

    async fn start(&self, user_id: &str, instance: u8) -> Result<Arc<ChromeInstance>, BrowserError> {
        let me = self
            .self_arc()
            .ok_or_else(|| BrowserError::Internal("browser service gone".into()))?;
        BrowserService::start(&me, user_id, instance).await
    }

    fn guard(&self) -> Arc<NavigationGuard> {
        self.guard.clone()
    }

    fn max_tabs(&self, user_id: &str) -> u8 {
        self.max_tabs_cell(user_id).load(Ordering::Relaxed)
    }
}

/// Log a start failure and make sure it has a code (an uncoded error would
/// reach the panel as a bare `BROWSER_NOT_RUNNING`).
fn start_failure(user_id: &str, instance: u8, e: BrowserError) -> BrowserError {
    match e.code() {
        Some(code) => {
            tracing::warn!(user_id, instance, ?code, error = %e, "server browser did not start");
            e
        }
        None => {
            tracing::error!(user_id, instance, error = %e, "server browser did not start");
            BrowserError::coded(BrowserErrorCode::BrowserStartFailed, "the browser did not start")
        }
    }
}

impl BrowserService {
    fn self_arc(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(dir: &std::path::Path) -> Arc<BrowserService> {
        BrowserService::new(BrowserServiceConfig {
            layout: BrowserLayout::new(dir.join("b"), dir.join("d")),
            host: None,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            guard: NavigationGuard::default(),
            chrome_override: Some(dir.join("no-chrome")),
            run_as_override: None,
        })
        .unwrap()
    }

    #[test]
    fn start_failures_always_carry_a_code() {
        let io = start_failure("u1", 1, BrowserError::Io(std::io::Error::other("boom")));
        assert_eq!(io.code(), Some(BrowserErrorCode::BrowserStartFailed));
        let coded = start_failure(
            "u1",
            1,
            BrowserError::coded(BrowserErrorCode::BrowserUnavailable, "host self-check failed"),
        );
        assert_eq!(coded.code(), Some(BrowserErrorCode::BrowserUnavailable));
    }

    #[tokio::test]
    async fn instances_that_do_not_exist_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let svc = service(t.path());
        for n in [0, 4, 6] {
            let err = svc.start("u1", n).await.unwrap_err();
            assert_eq!(err.code(), Some(BrowserErrorCode::BrowserInstanceNotFound), "{n}");
        }
        assert!(svc.instance_exists("u1", 3) && !svc.instance_exists("u1", 4));
    }

    /// TASK-124: delete wipes only that profile (as the account) and removes
    /// the row; instance 1 is protected; status rows carry the names.
    #[tokio::test]
    async fn delete_wipes_only_that_profile_and_status_lists_names() {
        let t = tempfile::tempdir().unwrap();
        let ids = unsafe { (libc::getuid(), libc::getgid()) };
        let svc = BrowserService::new(BrowserServiceConfig {
            layout: BrowserLayout::new(t.path().join("b"), t.path().join("d")),
            host: None,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            guard: NavigationGuard::default(),
            chrome_override: Some(t.path().join("no-chrome")),
            run_as_override: Some(ids),
        })
        .unwrap();
        svc.patch_instance(
            "u1",
            2,
            &BrowserInstancePatch {
                name: Some("TB Discord".into()),
                ..Default::default()
            },
        )
        .unwrap();
        for n in [1u8, 2] {
            let dir = t.path().join("b/u1").join(n.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Cookies"), "x").unwrap();
        }
        assert_eq!(
            svc.delete_instance("u1", 1).await.unwrap_err().code(),
            Some(BrowserErrorCode::BrowserInstanceProtected)
        );
        let s = svc.delete_instance("u1", 2).await.unwrap();
        assert!(!s.instances.iter().any(|i| i.instance == 2));
        assert!(!t.path().join("b/u1/2").exists(), "profile 2 wiped");
        assert!(t.path().join("b/u1/1/Cookies").exists(), "profile 1 untouched");
        assert_eq!(
            svc.delete_instance("u1", 2).await.unwrap_err().code(),
            Some(BrowserErrorCode::BrowserInstanceNotFound)
        );
        let st = svc.status("u1").unwrap();
        let names: Vec<&str> = st.instances.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["Main", "Instance 3"]);
    }

    #[tokio::test]
    async fn agents_get_no_env_and_no_instance_when_disabled() {
        let t = tempfile::tempdir().unwrap();
        let svc = service(t.path());
        svc.update_settings(
            "u1",
            &BrowserSettingsUpdate {
                agents_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(svc.agent_env("u1").await.is_empty());
        let err = BridgeBackend::agent_instance(svc.as_ref(), "u1", 1).await.unwrap_err();
        assert_eq!(err.code(), Some(BrowserErrorCode::BrowserAgentsDisabled));
    }

    #[tokio::test]
    async fn status_reports_unavailable_without_chrome_and_never_a_port() {
        let t = tempfile::tempdir().unwrap();
        let svc = service(t.path());
        let st = svc.status("u1").unwrap();
        assert!(!st.available);
        assert_eq!(st.unavailable_reason, Some(BrowserErrorCode::BrowserUnavailable));
        assert_eq!(st.instances.len(), 3);
        assert!(st.instances.iter().all(|i| !i.running));
        let json = serde_json::to_string(&st).unwrap();
        assert!(!json.contains("port") && !json.contains("token"));
    }

    #[tokio::test]
    async fn agent_env_lists_the_bridge_once_per_user() {
        let t = tempfile::tempdir().unwrap();
        let svc = service(t.path());
        let env: std::collections::HashMap<_, _> = svc.agent_env("u1").await.into_iter().collect();
        assert!(env.contains_key(crate::bridge::ENV_ACTIVE_PORT));
        assert_eq!(env[crate::bridge::ENV_PORTS].split(',').count(), 5);
        let again: std::collections::HashMap<_, _> = svc.agent_env("u1").await.into_iter().collect();
        assert_eq!(env, again, "same bridge, same token");
        let other: std::collections::HashMap<_, _> = svc.agent_env("u2").await.into_iter().collect();
        assert_ne!(
            other[crate::bridge::ENV_ACTIVE_TOKEN],
            env[crate::bridge::ENV_ACTIVE_TOKEN],
            "per-user token"
        );
        // TASK-124: name → port of the existing instances.
        svc.patch_instance(
            "u1",
            2,
            &BrowserInstancePatch {
                name: Some("Work, \"x\"".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let env: std::collections::HashMap<_, _> = svc.agent_env("u1").await.into_iter().collect();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&env[crate::bridge::ENV_INSTANCES]).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["name"], "Work, \"x\"");
        let ports: std::collections::HashMap<String, String> = env[crate::bridge::ENV_PORTS]
            .split(',')
            .filter_map(|p| p.split_once(':').map(|(n, port)| (n.to_owned(), port.to_owned())))
            .collect();
        assert_eq!(rows[1]["port"].to_string(), ports["2"]);
    }
}
