//! The browser service (C30): settings, Chrome install/update, instance
//! lifecycle (start on demand, idle stop, clear), and the agents' bridges.
//! Sole place for business logic; `routes.rs` only translates.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use aionui_api_types::{
    BROWSER_MAX_INSTANCES_LIMIT, BrowserErrorCode, BrowserInstanceStatus, BrowserSettings, BrowserSettingsUpdate,
    BrowserStatusResponse,
};
use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::bridge::{AgentBridge, BridgeBackend};
use crate::error::BrowserError;
use crate::guard::NavigationGuard;
use crate::host::{HostPaths, ensure_browser_user, harden_and_verify, prepare_dirs};
use crate::install::{ChromeInstaller, installed_version};
use crate::instance::{ChromeInstance, LaunchSpec};
use crate::layout::BrowserLayout;
use crate::panel::PanelBackend;
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
    /// Use this Chrome binary instead of the managed install (dev/tests).
    pub chrome_override: Option<PathBuf>,
    /// Run Chrome as this (uid, gid) instead of the `aionui-browser` account (tests).
    pub run_as_override: Option<(u32, u32)>,
}

type Key = (String, u8);

pub struct BrowserService {
    /// This service's own `Arc` (trait methods only get `&self`).
    me: std::sync::Weak<BrowserService>,
    cfg: BrowserServiceConfig,
    settings: SettingsStore,
    installer: ChromeInstaller,
    instances: DashMap<Key, Arc<ChromeInstance>>,
    start_locks: DashMap<Key, Arc<Mutex<()>>>,
    max_tabs: DashMap<String, Arc<AtomicU8>>,
    bridges: DashMap<String, AgentBridge>,
    bridge_lock: Mutex<()>,
    install_lock: Mutex<()>,
    install_error: std::sync::Mutex<Option<String>>,
    guard: Arc<NavigationGuard>,
}

impl BrowserService {
    pub fn new(cfg: BrowserServiceConfig) -> Result<Arc<Self>, BrowserError> {
        let installer = ChromeInstaller::new(cfg.layout.clone())?;
        let settings = SettingsStore::new(cfg.layout.settings_dir());
        let guard = Arc::new(cfg.guard.clone());
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
            install_error: std::sync::Mutex::new(None),
            guard,
        }))
    }

    /// Idle reaper + daily update check (call once at startup).
    pub fn spawn_background(self: &Arc<Self>) {
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
        // A lowered limit stops the instances above it.
        let above: Vec<Arc<ChromeInstance>> = self
            .instances
            .iter()
            .filter(|e| e.key().0 == user_id && e.key().1 > s.max_instances)
            .map(|e| e.value().clone())
            .collect();
        for inst in above {
            inst.stop().await;
        }
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

    fn chrome_available(&self) -> Option<String> {
        match &self.cfg.chrome_override {
            Some(p) => p.exists().then(|| "override".to_owned()),
            None => installed_version(&self.cfg.layout),
        }
    }

    pub fn status(self: &Arc<Self>, user_id: &str) -> Result<BrowserStatusResponse, BrowserError> {
        let s = self.settings(user_id)?;
        let version = self.chrome_available();
        if version.is_none() {
            // First use: install in the background; the UI polls status.
            let svc = self.clone();
            tokio::spawn(async move {
                let _ = svc.ensure_chrome().await;
            });
        }
        let instances = (1..=s.max_instances)
            .map(|n| {
                let inst = self.running(user_id, n);
                BrowserInstanceStatus {
                    instance: n,
                    running: inst.is_some(),
                    tabs: inst.as_ref().map(|i| i.tab_count() as u16).unwrap_or(0),
                    agent_active: inst.as_ref().is_some_and(|i| i.agent_active()),
                    hide_automation_active: inst.as_ref().is_some_and(|i| i.hide_automation),
                }
            })
            .collect();
        Ok(BrowserStatusResponse {
            available: version.is_some(),
            unavailable_reason: version.is_none().then_some(BrowserErrorCode::BrowserUnavailable),
            chrome_version: version,
            instances,
            idle_timeout_secs: self.cfg.idle_timeout.as_secs(),
        })
    }

    // -- install ------------------------------------------------------------

    /// The Chrome binary to launch; installs on first use (single flight).
    async fn ensure_chrome(&self) -> Result<PathBuf, BrowserError> {
        if let Some(p) = &self.cfg.chrome_override {
            return Ok(p.clone());
        }
        if installed_version(&self.cfg.layout).is_some() {
            return Ok(self.cfg.layout.chrome_binary());
        }
        let _guard = self.install_lock.lock().await;
        if installed_version(&self.cfg.layout).is_some() {
            return Ok(self.cfg.layout.chrome_binary());
        }
        tracing::info!("installing Chrome for the server browser");
        let result = async {
            let entry = self.installer.latest().await?;
            let version = self.installer.install(&entry).await?;
            self.installer.activate(&version)?;
            tracing::info!(chrome = %version, "Chrome installed for the server browser");
            Ok::<_, BrowserError>(self.cfg.layout.chrome_binary())
        }
        .await;
        if let Err(e) = &result {
            tracing::error!(error = %e, "Chrome install failed");
            *self.install_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
        }
        result.map_err(|_| BrowserError::coded(BrowserErrorCode::BrowserUnavailable, "Chrome is not installed"))
    }

    /// Daily: install a newer stable; switch to it only while nothing runs.
    async fn update_check(&self) {
        if self.cfg.chrome_override.is_some() || installed_version(&self.cfg.layout).is_none() {
            return;
        }
        let _guard = self.install_lock.lock().await;
        let result = async {
            let entry = self.installer.latest().await?;
            let version = self.installer.install(&entry).await?;
            if installed_version(&self.cfg.layout).as_deref() != Some(version.as_str()) && self.instances.is_empty() {
                self.installer.activate(&version)?;
                tracing::info!(chrome = %version, "server browser Chrome updated");
            }
            Ok::<_, BrowserError>(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "Chrome update check failed");
        }
    }

    // -- instances --------------------------------------------------------

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

    fn check_instance(&self, user_id: &str, n: u8) -> Result<BrowserSettings, BrowserError> {
        let s = self.settings(user_id)?;
        if n == 0 || n > s.max_instances || n > BROWSER_MAX_INSTANCES_LIMIT {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserInstanceLimit,
                format!("instance {n} is above the limit"),
            ));
        }
        Ok(s)
    }

    /// Start instance `n` for `user_id` (or return the running one).
    pub async fn start(self: &Arc<Self>, user_id: &str, n: u8) -> Result<Arc<ChromeInstance>, BrowserError> {
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
        let chrome = self.ensure_chrome().await?;
        if let Some(host) = &self.cfg.host {
            harden_and_verify(host)?;
        }
        let (uid, gid) = match self.cfg.run_as_override {
            Some(ids) => ids,
            None => ensure_browser_user().await?,
        };
        let profile = prepare_dirs(&self.cfg.layout, user_id, n, uid, gid)?;
        let inst = ChromeInstance::launch(LaunchSpec {
            user_id: user_id.to_owned(),
            instance: n,
            chrome,
            profile,
            home: self.cfg.layout.home_dir(),
            uid,
            gid,
            hide_automation: self.settings.hide_automation(user_id, n)?,
            max_tabs: self.max_tabs_cell(user_id),
            guard: self.guard.clone(),
        })
        .await?;
        self.instances.insert(key.clone(), inst.clone());
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
        if n == 0 || n > BROWSER_MAX_INSTANCES_LIMIT {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserInstanceLimit,
                "no such instance",
            ));
        }
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
        match std::fs::remove_dir_all(&profile) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
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
    pub async fn agent_env(self: &Arc<Self>, user_id: &str) -> Vec<(String, String)> {
        if !self.agents_enabled_now(user_id) {
            return Vec::new();
        }
        if let Some(b) = self.bridges.get(user_id) {
            return b.env();
        }
        let _held = self.bridge_lock.lock().await;
        if let Some(b) = self.bridges.get(user_id) {
            return b.env();
        }
        let backend: Arc<dyn BridgeBackend> = self.clone();
        match AgentBridge::start(user_id, backend).await {
            Ok(bridge) => {
                let env = bridge.env();
                self.bridges.insert(user_id.to_owned(), bridge);
                env
            }
            Err(e) => {
                tracing::warn!(user_id, error = %e, "browser agent bridge did not start");
                Vec::new()
            }
        }
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

    #[tokio::test]
    async fn instance_numbers_above_the_limit_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let svc = service(t.path());
        for n in [0, 4, 6] {
            let err = svc.start("u1", n).await.unwrap_err();
            assert_eq!(err.code(), Some(BrowserErrorCode::BrowserInstanceLimit), "{n}");
        }
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
    }
}
