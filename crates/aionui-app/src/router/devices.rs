//! Settings → Devices composition (C38, TASK-136): the registry built over
//! the server browser and the C34 own-browser devices.

use std::sync::Arc;

use aionui_api_types::devices::DeviceErrorCode;
use aionui_api_types::{BrowserEngine, BrowserErrorCode, BrowserSettingsUpdate};
use aionui_browser::{BrowserError, BrowserService};
use aionui_devices::service::{DeviceInstaller, ProgressFn};
use aionui_devices::{BrowserDevices, BrowserFacts, DevicesError, DevicesEventSink, EngineFacts, MyBrowserFacts};

/// Brand names of the engines (not translated).
fn engine_name(engine: BrowserEngine) -> &'static str {
    match engine {
        BrowserEngine::Chrome => "Chrome",
        BrowserEngine::ChromeForTesting => "Chrome for Testing",
        BrowserEngine::Brave => "Brave",
        BrowserEngine::Edge => "Microsoft Edge",
        BrowserEngine::HeadlessShell => "Chrome Headless Shell",
        BrowserEngine::Camoufox => "Camoufox",
    }
}

/// `server_browser:<engine id>` → the engine, for the ids this lane owns.
fn engine_of_device(device_id: &str) -> Option<BrowserEngine> {
    BrowserEngine::from_id(device_id.strip_prefix("server_browser:")?)
}

fn engine_error(e: BrowserError) -> DevicesError {
    match e.code() {
        Some(BrowserErrorCode::BrowserEngineInUse) => DevicesError::coded(DeviceErrorCode::DeviceInUse, e.to_string()),
        _ => DevicesError::Internal(e.to_string()),
    }
}

/// K7: install / remove of the server-browser engines as registry jobs.
pub(crate) struct EngineDeviceInstaller {
    pub service: Arc<BrowserService>,
}

#[async_trait::async_trait]
impl DeviceInstaller for EngineDeviceInstaller {
    fn handles(&self, device_id: &str) -> bool {
        engine_of_device(device_id).is_some()
    }

    async fn install(&self, _user_id: &str, device_id: &str, progress: &ProgressFn) -> Result<(), DevicesError> {
        let engine = engine_of_device(device_id).ok_or_else(DevicesError::not_found)?;
        // The download reports no byte counts: one step before, one after.
        progress(5);
        self.service.install_engine_now(engine).await.map_err(engine_error)?;
        progress(100);
        Ok(())
    }

    async fn check_remove(&self, _user_id: &str, device_id: &str) -> Result<(), DevicesError> {
        let engine = engine_of_device(device_id).ok_or_else(DevicesError::not_found)?;
        self.service.check_engine_removable(engine).map_err(engine_error)
    }

    async fn remove(&self, _user_id: &str, device_id: &str, progress: &ProgressFn) -> Result<(), DevicesError> {
        let engine = engine_of_device(device_id).ok_or_else(DevicesError::not_found)?;
        progress(10);
        // Checked again under the browser's locks (an instance may have
        // started since the request was accepted).
        self.service.remove_engine(engine).await.map_err(engine_error)?;
        progress(100);
        Ok(())
    }
}

pub(crate) struct BrowserDevicesAdapter {
    pub service: Arc<BrowserService>,
}

#[async_trait::async_trait]
impl BrowserDevices for BrowserDevicesAdapter {
    fn server_browser(&self, user_id: &str) -> BrowserFacts {
        let status = self.service.status(user_id).ok();
        BrowserFacts {
            installed: status.as_ref().is_some_and(|s| s.available),
            version: status.as_ref().and_then(|s| s.chrome_version.clone()),
            agents_enabled: self.service.agents_enabled(user_id),
            running: status.as_ref().is_some_and(|s| s.instances.iter().any(|i| i.running)),
        }
    }

    fn server_engines(&self, _user_id: &str) -> Vec<EngineFacts> {
        self.service
            .engines()
            .into_iter()
            .map(|e| EngineFacts {
                engine_id: e.engine.id().to_owned(),
                name: engine_name(e.engine).to_owned(),
                installed: e.installed_version.is_some(),
                version: e.installed_version,
                disk_bytes: e.disk_bytes,
                installing: e.installing,
                error: e.error,
            })
            .collect()
    }

    async fn set_server_browser_enabled(&self, user_id: &str, enabled: bool) -> Result<(), String> {
        self.service
            .update_settings(
                user_id,
                &BrowserSettingsUpdate {
                    agents_enabled: Some(enabled),
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn my_browsers(&self, user_id: &str) -> Vec<MyBrowserFacts> {
        self.service
            .device_hub()
            .devices(user_id)
            .into_iter()
            .map(|d| MyBrowserFacts {
                device_id: d.device_id,
                name: d.name,
                browser: d.browser,
                online: d.online,
                last_seen_at: d.last_seen_at,
                extension_version: d.extension_version,
            })
            .collect()
    }

    fn my_browsers_enabled(&self, user_id: &str) -> bool {
        self.service
            .settings(user_id)
            .map(|s| s.agents_enabled && s.devices_agents_enabled)
            .unwrap_or(false)
    }

    async fn set_my_browsers_enabled(&self, user_id: &str, enabled: bool) -> Result<(), String> {
        self.service
            .update_settings(
                user_id,
                &BrowserSettingsUpdate {
                    devices_agents_enabled: Some(enabled),
                    // The own-browser switch needs the general one on too (C34).
                    agents_enabled: enabled.then_some(true),
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

impl DevicesEventSink for super::browser::BrowserEventsToUser {
    fn emit(&self, user_id: &str, name: &str, data: serde_json::Value) {
        self.0
            .broadcast_to_user(user_id, aionui_api_types::WebSocketMessage::new(name, data));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::BrowserInstancePatch;
    use aionui_browser::{BrowserLayout, BrowserServiceConfig, DEFAULT_IDLE_TIMEOUT, NavigationGuard};

    /// A browser service on a temp root with `engines` "installed" (never the
    /// real root: these tests remove engines).
    fn service(dir: &std::path::Path, engines: &[(BrowserEngine, &str, &str)]) -> Arc<BrowserService> {
        let layout = BrowserLayout::new(dir.join("b"), dir.join("d"));
        for (engine, version, binary) in engines {
            let d = layout.engine_dir(*engine).join(version);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(binary), vec![0u8; 2048]).unwrap();
            std::os::unix::fs::symlink(version, layout.engine_current(*engine)).unwrap();
        }
        BrowserService::new(BrowserServiceConfig {
            layout,
            host: None,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            guard: NavigationGuard::default(),
            chrome_override: None,
            run_as_override: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn engines_are_registry_rows_and_remove_maps_in_use_to_device_in_use() {
        let t = tempfile::tempdir().unwrap();
        let svc = service(
            t.path(),
            &[
                (BrowserEngine::Chrome, "157.0.1.1", "chrome"),
                (BrowserEngine::Brave, "1.99.57", "brave"),
            ],
        );
        let adapter = BrowserDevicesAdapter { service: svc.clone() };
        let installer = EngineDeviceInstaller { service: svc.clone() };
        let facts = adapter.server_engines("u1");
        let ids: Vec<&str> = facts.iter().map(|f| f.engine_id.as_str()).collect();
        // Camoufox (K6) is a real engine row, not the registry's placeholder.
        assert_eq!(
            ids,
            [
                "chrome",
                "chrome-for-testing",
                "brave",
                "edge",
                "headless-shell",
                "camoufox"
            ]
        );
        let camoufox = facts.iter().find(|f| f.engine_id == "camoufox").unwrap();
        assert_eq!((camoufox.name.as_str(), camoufox.installed), ("Camoufox", false));
        let brave = facts.iter().find(|f| f.engine_id == "brave").unwrap();
        assert_eq!(
            (
                brave.name.as_str(),
                brave.installed,
                brave.version.as_deref(),
                brave.disk_bytes
            ),
            ("Brave", true, Some("1.99.57"), Some(2048))
        );
        assert!(!facts.iter().find(|f| f.engine_id == "edge").unwrap().installed);

        assert!(installer.handles("server_browser:brave"));
        assert!(installer.handles("server_browser:headless-shell"));
        assert!(installer.handles("server_browser:camoufox"));
        for other in ["server_browser:", "server_browser:firefox", "addon:artemis", "brave"] {
            assert!(!installer.handles(other), "{other}");
        }

        // Chrome: the default rows select it → DEVICE_IN_USE before and in the job.
        let noop = |_: u8| {};
        for refused in [
            installer.check_remove("u1", "server_browser:chrome").await.unwrap_err(),
            installer
                .remove("u1", "server_browser:chrome", &noop)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(refused.code(), Some(DeviceErrorCode::DeviceInUse));
        }
        // Brave: selected by a row → refused; freed → removed.
        let pick = |engine| BrowserInstancePatch {
            engine: Some(engine),
            ..Default::default()
        };
        svc.patch_instance("u1", 3, &pick(BrowserEngine::Brave)).unwrap();
        assert_eq!(
            installer
                .check_remove("u1", "server_browser:brave")
                .await
                .unwrap_err()
                .code(),
            Some(DeviceErrorCode::DeviceInUse)
        );
        svc.patch_instance("u1", 3, &pick(BrowserEngine::Chrome)).unwrap();
        installer.check_remove("u1", "server_browser:brave").await.unwrap();
        installer.remove("u1", "server_browser:brave", &noop).await.unwrap();
        let brave = adapter
            .server_engines("u1")
            .into_iter()
            .find(|f| f.engine_id == "brave")
            .unwrap();
        assert_eq!((brave.installed, brave.version, brave.disk_bytes), (false, None, None));
        assert!(adapter.server_engines("u1")[0].installed, "Chrome untouched");
    }

    #[tokio::test]
    async fn the_camoufox_row_counts_its_cached_archive_and_remove_clears_it() {
        let t = tempfile::tempdir().unwrap();
        let svc = service(
            t.path(),
            &[
                (BrowserEngine::Chrome, "157.0.1.1", "chrome"),
                (BrowserEngine::Camoufox, "159.0-beta.42", "camoufox-bin"),
            ],
        );
        let cache = t.path().join("b").join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let archive = cache.join(aionui_browser::camoufox::ASSET);
        std::fs::write(&archive, vec![0u8; 1000]).unwrap();
        let adapter = BrowserDevicesAdapter { service: svc.clone() };
        let installer = EngineDeviceInstaller { service: svc.clone() };
        let row = |a: &BrowserDevicesAdapter| {
            a.server_engines("u1")
                .into_iter()
                .find(|f| f.engine_id == "camoufox")
                .unwrap()
        };
        let before = row(&adapter);
        assert_eq!(
            (before.installed, before.version.as_deref(), before.disk_bytes),
            (true, Some("159.0-beta.42"), Some(2048 + 1000)),
            "engine tree + cached archive"
        );
        // Selected by an instance row: 409 and the cache stays.
        let pick = |engine| BrowserInstancePatch {
            engine: Some(engine),
            ..Default::default()
        };
        svc.patch_instance("u1", 2, &pick(BrowserEngine::Camoufox)).unwrap();
        let noop = |_: u8| {};
        let refused = installer
            .remove("u1", "server_browser:camoufox", &noop)
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Some(DeviceErrorCode::DeviceInUse));
        assert!(archive.exists(), "a refused remove clears nothing");
        svc.patch_instance("u1", 2, &pick(BrowserEngine::Chrome)).unwrap();
        installer.remove("u1", "server_browser:camoufox", &noop).await.unwrap();
        assert!(!archive.exists(), "remove clears the cached archive");
        let after = row(&adapter);
        assert_eq!((after.installed, after.disk_bytes), (false, None));
    }
}
