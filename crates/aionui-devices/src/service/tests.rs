use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aionui_api_types::devices::{DeviceAuditOutcome, DeviceKind, DeviceStatus, DeviceUpdate};
use aionui_db::{
    DevicePairingRow, IDeviceRepository, SqliteDeviceRepository, UpsertDeviceParams, init_database_memory,
};

use super::*;
use crate::ports::{BrowserFacts, MyBrowserFacts};

#[derive(Default)]
struct FakeBrowser {
    agents: AtomicBool,
    mine: AtomicBool,
}

#[async_trait::async_trait]
impl BrowserDevices for FakeBrowser {
    fn server_browser(&self, _: &str) -> BrowserFacts {
        BrowserFacts {
            installed: true,
            version: Some("157.0".into()),
            agents_enabled: self.agents.load(Ordering::SeqCst),
            running: false,
        }
    }
    async fn set_server_browser_enabled(&self, _: &str, enabled: bool) -> Result<(), String> {
        self.agents.store(enabled, Ordering::SeqCst);
        Ok(())
    }
    fn my_browsers(&self, user_id: &str) -> Vec<MyBrowserFacts> {
        if user_id != "u1" {
            return Vec::new();
        }
        vec![MyBrowserFacts {
            device_id: "dev_1".into(),
            name: "Mac".into(),
            browser: "Chrome 157".into(),
            online: true,
            last_seen_at: Some(5),
            extension_version: Some("0.1.2".into()),
        }]
    }
    fn my_browsers_enabled(&self, _: &str) -> bool {
        self.mine.load(Ordering::SeqCst)
    }
    async fn set_my_browsers_enabled(&self, _: &str, enabled: bool) -> Result<(), String> {
        self.mine.store(enabled, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<(String, String, serde_json::Value)>>);

impl DevicesEventSink for Events {
    fn emit(&self, user_id: &str, name: &str, data: serde_json::Value) {
        self.0.lock().unwrap().push((user_id.into(), name.into(), data));
    }
}

async fn setup() -> (Arc<DevicesService>, Arc<dyn IDeviceRepository>, Arc<Events>) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IDeviceRepository> = Arc::new(SqliteDeviceRepository::new(db.pool().clone()));
    let svc = DevicesService::new(repo.clone(), Arc::new(FakeBrowser::default()));
    let events = Arc::new(Events::default());
    svc.set_event_sink(events.clone());
    // Keep the pool alive for the test (the repo holds a clone).
    std::mem::forget(db);
    (svc, repo, events)
}

async fn paired_phone(repo: &Arc<dyn IDeviceRepository>, user: &str) -> String {
    repo.insert_pairing(&DevicePairingRow {
        id: format!("pair_{user}"),
        user_id: user.into(),
        kind: "mac_helper".into(),
        name: "MacBook".into(),
        token_sha256: format!("hash_{user}"),
        version: None,
        capabilities: "[]".into(),
        created_at: 1,
        last_seen_at: None,
        revoked_at: None,
    })
    .await
    .unwrap();
    let pairing = format!("pair_{user}");
    repo.upsert_device(UpsertDeviceParams {
        id: &format!("phone:{user}"),
        user_id: user,
        kind: "phone",
        pairing_id: Some(&pairing),
        source_id: "R5CX",
        name: "Galaxy",
        meta: "{}",
        now: 1,
    })
    .await
    .unwrap()
    .id
}

#[tokio::test]
async fn list_aggregates_browsers_phones_and_library_without_a_second_store() {
    let (svc, repo, _events) = setup().await;
    let phone = paired_phone(&repo, "u1").await;
    let list = svc.list("u1").await.unwrap();
    let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
    assert!(ids.contains(&SERVER_BROWSER_ID));
    assert!(ids.contains(&"my_browser:dev_1"));
    assert!(ids.contains(&phone.as_str()));
    assert!(ids.contains(&MAC_LATER_ID) && ids.contains(&CAMOUFOX_ID), "later cards");
    assert!(ids.contains(&"addon:artemis") && ids.contains(&"addon:mobilerun"));
    let mine = list.iter().find(|d| d.id == "my_browser:dev_1").unwrap();
    assert_eq!(mine.version.as_deref(), Some("0.1.2"));
    assert_eq!(mine.name, "Mac · Chrome");
    let p = list.iter().find(|d| d.id == phone).unwrap();
    assert_eq!(p.kind, DeviceKind::Phone);
    assert_eq!(p.status, DeviceStatus::Offline, "its Mac helper is not connected");
    let routes = p.routes.as_ref().unwrap();
    assert_eq!(routes[0].route, DeviceRouteKind::MacRelay);
    assert!(routes[1].experimental && routes[1].route == DeviceRouteKind::WifiDirect);
    // Connected but not in the client's inventory: still offline (relay
    // presence is covered end to end in tests/it/relay.rs).
    svc.set_pairing_online("pair_u1", true);
    assert_eq!(svc.get("u1", &phone).await.unwrap().status, DeviceStatus::Offline);
    // Another user sees none of u1's phones or browsers.
    let other = svc.list("u2").await.unwrap();
    assert!(other.iter().all(|d| d.id != phone && d.id != "my_browser:dev_1"));
    assert_eq!(
        svc.get("u2", &phone).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceNotFound)
    );
}

#[tokio::test]
async fn phone_switch_needs_a_valid_timeout_and_expires_with_an_event() {
    let (svc, repo, events) = setup().await;
    let phone = paired_phone(&repo, "u1").await;
    svc.set_pairing_online("pair_u1", true);
    let bad = svc
        .update(
            "u1",
            &phone,
            &DeviceUpdate {
                enabled: true,
                timeout_minutes: Some(45),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Some(DeviceErrorCode::DeviceBadMessage));
    let on = svc
        .update(
            "u1",
            &phone,
            &DeviceUpdate {
                enabled: true,
                timeout_minutes: Some(15),
            },
        )
        .await
        .unwrap();
    assert!(on.enabled);
    let until = on.enabled_until.expect("timeout persisted");
    assert!(until > now_ms() + 14 * 60_000 && until <= now_ms() + 15 * 60_000);
    assert!(svc.is_enabled("u1", &phone).await.unwrap());
    // Default timeout = 30 minutes.
    let default = svc
        .update(
            "u1",
            &phone,
            &DeviceUpdate {
                enabled: true,
                timeout_minutes: None,
            },
        )
        .await
        .unwrap();
    assert!(default.enabled_until.unwrap() > now_ms() + 29 * 60_000);
    // Another user cannot switch it.
    assert_eq!(
        svc.update(
            "u2",
            &phone,
            &DeviceUpdate {
                enabled: true,
                timeout_minutes: Some(15)
            }
        )
        .await
        .unwrap_err()
        .code(),
        Some(DeviceErrorCode::DeviceNotFound)
    );
    // Expired (as after a restart): off, with an event.
    repo.set_device_enabled("u1", &phone, true, Some(now_ms() - 1), now_ms())
        .await
        .unwrap();
    assert!(
        !svc.is_enabled("u1", &phone).await.unwrap(),
        "past timeout = off even before the ticker"
    );
    events.0.lock().unwrap().clear();
    assert_eq!(svc.expire_switches().await, 1);
    let d = svc.get("u1", &phone).await.unwrap();
    assert!(!d.enabled && d.enabled_until.is_none());
    let ev = events.0.lock().unwrap().clone();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, "u1");
    assert_eq!(ev[0].1, "devices.changed");
    assert_eq!(ev[0].2["device_id"], phone.as_str());
}

#[tokio::test]
async fn browser_switches_write_through_and_later_cards_refuse() {
    let (svc, _repo, _events) = setup().await;
    let chrome = svc
        .update(
            "u1",
            SERVER_BROWSER_ID,
            &DeviceUpdate {
                enabled: true,
                timeout_minutes: None,
            },
        )
        .await
        .unwrap();
    assert!(chrome.enabled && chrome.status == DeviceStatus::On);
    let mine = svc
        .update(
            "u1",
            "my_browser:dev_1",
            &DeviceUpdate {
                enabled: true,
                timeout_minutes: None,
            },
        )
        .await
        .unwrap();
    assert!(mine.enabled);
    for id in [CAMOUFOX_ID, MAC_LATER_ID] {
        assert_eq!(
            svc.update(
                "u1",
                id,
                &DeviceUpdate {
                    enabled: true,
                    timeout_minutes: None
                }
            )
            .await
            .unwrap_err()
            .code(),
            Some(DeviceErrorCode::DeviceVerbRefused)
        );
    }
    assert_eq!(
        svc.update(
            "u1",
            "addon:artemis",
            &DeviceUpdate {
                enabled: true,
                timeout_minutes: None
            }
        )
        .await
        .unwrap_err()
        .code(),
        Some(DeviceErrorCode::DeviceVerbRefused),
        "an add-on must be installed first"
    );
}

#[tokio::test]
async fn wifi_direct_route_is_refused_until_tested() {
    let (svc, repo, _events) = setup().await;
    let phone = paired_phone(&repo, "u1").await;
    svc.set_pairing_online("pair_u1", true);
    assert_eq!(
        svc.update_route("u1", &phone, DeviceRouteKind::WifiDirect, true)
            .await
            .unwrap_err()
            .code(),
        Some(DeviceErrorCode::DeviceVerbRefused)
    );
    let on = svc
        .update_route("u1", &phone, DeviceRouteKind::MacRelay, true)
        .await
        .unwrap();
    assert!(on.enabled);
    assert_eq!(
        svc.update_route("u1", &phone, DeviceRouteKind::Companion, true)
            .await
            .unwrap_err()
            .code(),
        Some(DeviceErrorCode::DeviceBadMessage)
    );
}

struct SlowInstaller {
    in_use: bool,
}

#[async_trait::async_trait]
impl DeviceInstaller for SlowInstaller {
    fn handles(&self, device_id: &str) -> bool {
        device_id == "addon:artemis"
    }
    async fn install(&self, _: &str, _: &str, progress: &ProgressFn) -> Result<(), DevicesError> {
        progress(50);
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }
    async fn remove(&self, _: &str, _: &str, _: &ProgressFn) -> Result<(), DevicesError> {
        if self.in_use {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceInUse,
                "switch the instance first",
            ));
        }
        Ok(())
    }
}

#[tokio::test]
async fn install_jobs_run_once_belong_to_their_user_and_report_failures() {
    let (svc, _repo, _events) = setup().await;
    assert_eq!(
        svc.start_job("u1", "addon:mobilerun", false).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceVerbRefused),
        "no installer registered"
    );
    svc.register_installer(Arc::new(SlowInstaller { in_use: true }));
    let job = svc.start_job("u1", "addon:artemis", false).await.unwrap();
    assert_eq!(
        svc.start_job("u1", "addon:artemis", false).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceBusy)
    );
    let card = svc.get("u1", "addon:artemis").await.unwrap();
    assert_eq!(card.status, DeviceStatus::Installing);
    assert_eq!(card.job_id.as_deref(), Some(job.as_str()));
    assert_eq!(
        svc.job("u2", &job).unwrap_err().code(),
        Some(DeviceErrorCode::DeviceJobNotFound),
        "another user's job is not visible"
    );
    for _ in 0..100 {
        if svc.job("u1", &job).unwrap().state == DeviceJobState::Done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(svc.job("u1", &job).unwrap().state, DeviceJobState::Done);
    assert_eq!(svc.job("u1", &job).unwrap().progress, 100);
    let remove = svc.start_job("u1", "addon:artemis", true).await.unwrap();
    for _ in 0..100 {
        if svc.job("u1", &remove).unwrap().state == DeviceJobState::Failed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let failed = svc.job("u1", &remove).unwrap();
    assert_eq!(failed.state, DeviceJobState::Failed);
    assert!(failed.error.unwrap().contains("switch the instance first"));
    assert_eq!(
        svc.job("u1", "job_unknown").unwrap_err().code(),
        Some(DeviceErrorCode::DeviceJobNotFound),
        "a job from before a restart is unknown"
    );
}

#[tokio::test]
async fn audit_pages_and_never_leaks_across_users() {
    let (svc, repo, _events) = setup().await;
    let phone = paired_phone(&repo, "u1").await;
    for outcome in [
        DeviceAuditOutcome::Ok,
        DeviceAuditOutcome::Refused,
        DeviceAuditOutcome::Approved,
    ] {
        svc.record(
            "u1",
            &phone,
            AuditNote {
                verb: "tap",
                package: Some("com.app"),
                origin: None,
                outcome,
                conversation_id: Some("c1"),
                detail: None,
            },
        )
        .await;
    }
    let first = svc.audit("u1", &phone, None, Some(2)).await.unwrap();
    assert_eq!(first.rows.len(), 2);
    assert_eq!(first.rows[0].outcome, DeviceAuditOutcome::Approved);
    let next = svc
        .audit("u1", &phone, first.next_cursor.as_deref(), Some(2))
        .await
        .unwrap();
    assert_eq!(next.rows.len(), 1);
    assert!(next.next_cursor.is_none());
    assert_eq!(
        svc.audit("u1", &phone, Some("x"), None).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceBadMessage)
    );
    assert_eq!(
        svc.audit("u2", &phone, None, None).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceNotFound)
    );
}
