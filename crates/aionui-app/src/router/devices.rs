//! Settings → Devices composition (C38, TASK-136): the registry built over
//! the server browser and the C34 own-browser devices.

use std::sync::Arc;

use aionui_api_types::BrowserSettingsUpdate;
use aionui_browser::BrowserService;
use aionui_devices::{BrowserDevices, BrowserFacts, DevicesEventSink, MyBrowserFacts};

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
