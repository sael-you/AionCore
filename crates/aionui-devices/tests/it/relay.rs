//! C38 / TASK-136 K2: the device relay end to end with a FAKE Mac helper.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aionui_api_types::devices::{
    DeviceAvailability, DeviceErrorCode, DeviceKind, DevicePairingKind, DeviceStatus, DeviceUpdate,
};
use aionui_db::{IDeviceRepository, SqliteDeviceRepository, init_database_memory};
use aionui_devices::{
    BrowserDevices, BrowserFacts, DevicesRouterState, DevicesService, MyBrowserFacts, devices_relay_routes,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub(crate) const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct NoBrowser(AtomicBool);

#[async_trait::async_trait]
impl BrowserDevices for NoBrowser {
    fn server_browser(&self, _: &str) -> BrowserFacts {
        BrowserFacts {
            installed: false,
            version: None,
            agents_enabled: self.0.load(Ordering::SeqCst),
            running: false,
        }
    }
    async fn set_server_browser_enabled(&self, _: &str, e: bool) -> Result<(), String> {
        self.0.store(e, Ordering::SeqCst);
        Ok(())
    }
    fn my_browsers(&self, _: &str) -> Vec<MyBrowserFacts> {
        Vec::new()
    }
    fn my_browsers_enabled(&self, _: &str) -> bool {
        false
    }
    async fn set_my_browsers_enabled(&self, _: &str, _: bool) -> Result<(), String> {
        Ok(())
    }
    /// Stands in for the browser's navigation guard.
    fn url_blocked(&self, url: &str) -> bool {
        url.contains("//192.168.") || url.contains("//localhost") || url.contains("//aionui.example")
    }
}

pub(crate) async fn harness() -> (Arc<DevicesService>, u16) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IDeviceRepository> = Arc::new(SqliteDeviceRepository::new(db.pool().clone()));
    std::mem::forget(db);
    let svc = DevicesService::new(repo, Arc::new(NoBrowser::default()));
    let app = devices_relay_routes(DevicesRouterState { service: svc.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (svc, port)
}

pub(crate) async fn open(port: u16, origin: Option<&str>) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://127.0.0.1:{port}/api/devices/relay")
        .into_client_request()
        .unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
}

pub(crate) async fn next(ws: &mut Ws) -> Option<Value> {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return serde_json::from_str(&t).ok(),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) | Err(_) => return None,
            Ok(Some(Ok(_))) => {}
        }
    }
}

pub(crate) async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// Pair a fake Mac helper serving the phone verbs the tests use.
async fn pair_mac(svc: &Arc<DevicesService>, port: u16) -> (Ws, String, String) {
    pair_mac_with(
        svc,
        port,
        &["tap", "screenshot", "foreground", "text", "ui_tree", "launch", "stop"],
    )
    .await
}

async fn pair_mac_with(svc: &Arc<DevicesService>, port: u16, capabilities: &[&str]) -> (Ws, String, String) {
    let code = svc.pairing_code("u1", DevicePairingKind::MacHelper, None).unwrap().code;
    let mut ws = open(port, None).await.unwrap();
    send(
        &mut ws,
        json!({"type": "pair", "code": code, "kind": "mac_helper", "name": "MacBook", "version": "0.1.0",
               "capabilities": capabilities}),
    )
    .await;
    let welcome = next(&mut ws).await.unwrap();
    assert_eq!(welcome["type"], "welcome");
    let token = welcome["token"].as_str().unwrap().to_owned();
    let client_id = welcome["client_id"].as_str().unwrap().to_owned();
    send(
        &mut ws,
        json!({"type": "inventory",
               "phones": [{"serial": "R5CX", "name": "Galaxy", "transport": "usb", "chrome": true, "enabled": true}],
               "mac": {"name": "MacBook", "permissions_ok": true, "enabled": true}}),
    )
    .await;
    (ws, token, client_id)
}

async fn wait_status(svc: &Arc<DevicesService>, id: &str, want: DeviceStatus) {
    for _ in 0..200 {
        if matches!(svc.get("u1", id).await, Ok(d) if d.status == want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{id} never became {want:?}: {:?}",
        svc.get("u1", id).await.map(|d| d.status)
    );
}

#[tokio::test]
async fn first_frame_and_origin_rules() {
    let (svc, port) = harness().await;
    assert!(
        open(port, Some("https://evil.example")).await.is_err(),
        "browsers refused"
    );
    for (first, code) in [
        (
            json!({"type": "hello", "token": "nope", "kind": "mac_helper", "version": "1"}),
            "DEVICE_REVOKED",
        ),
        (
            json!({"type": "pair", "code": "WRONG-CODE", "kind": "mac_helper", "name": "x", "version": "1"}),
            "DEVICE_PAIRING_INVALID",
        ),
        (json!({"type": "ping"}), "DEVICE_BAD_MESSAGE"),
    ] {
        let mut ws = open(port, None).await.unwrap();
        send(&mut ws, first).await;
        assert_eq!(next(&mut ws).await.unwrap(), json!({"type": "error", "code": code}));
        assert!(next(&mut ws).await.is_none());
    }
    // A companion code is not a Mac helper code.
    let code = svc
        .pairing_code("u1", DevicePairingKind::Companion, Some("y.talkseeds.com"))
        .unwrap();
    assert_eq!(
        code.qr_payload.as_deref(),
        Some(format!("aionui-pair:v1?host=y.talkseeds.com&code={}", code.code).as_str())
    );
    let mut ws = open(port, None).await.unwrap();
    send(
        &mut ws,
        json!({"type": "pair", "code": code.code, "kind": "mac_helper", "name": "x", "version": "1"}),
    )
    .await;
    assert_eq!(next(&mut ws).await.unwrap()["code"], "DEVICE_PAIRING_INVALID");
    assert_eq!(
        svc.pairing_code("u1", DevicePairingKind::WifiDirect, None)
            .unwrap_err()
            .code(),
        Some(DeviceErrorCode::DeviceVerbRefused)
    );
}

#[tokio::test]
async fn pair_inventory_requests_stop_and_revoke() {
    let (svc, port) = harness().await;
    let (mut ws, token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    let mac = format!("mac:{client_id}:mac");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    let list = svc.list("u1").await.unwrap();
    let m = list.iter().find(|d| d.id == mac).expect("the Mac card");
    assert_eq!((m.kind, m.available), (DeviceKind::Mac, DeviceAvailability::Now));
    assert!(list.iter().all(|d| d.id != "mac:later"));
    assert!(svc.list("u2").await.unwrap().iter().all(|d| d.id != phone), "per user");

    // A request goes to the phone serial and its answer comes back.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let call = tokio::spawn(async move { svc2.relay_call("u1", &phone2, "tap", json!({"x": 1, "y": 2})).await });
    let req = next(&mut ws).await.unwrap();
    assert_eq!(
        (req["type"].as_str(), req["device"].as_str(), req["verb"].as_str()),
        (Some("request"), Some("R5CX"), Some("tap"))
    );
    send(
        &mut ws,
        json!({"type": "response", "id": req["id"], "ok": true, "result": {"done": true}}),
    )
    .await;
    assert_eq!(call.await.unwrap().unwrap(), json!({"done": true}));

    // Peer errors map to codes; unannounced verbs never reach the peer.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let call = tokio::spawn(async move { svc2.relay_call("u1", &phone2, "screenshot", json!({})).await });
    let req = next(&mut ws).await.unwrap();
    send(
        &mut ws,
        json!({"type": "response", "id": req["id"], "ok": false, "error": {"code": "switch_off", "message": "off"}}),
    )
    .await;
    assert_eq!(
        call.await.unwrap().unwrap_err().code(),
        Some(DeviceErrorCode::DeviceDisabled)
    );
    assert_eq!(
        svc.relay_call("u1", &phone, "logcat", json!({}))
            .await
            .unwrap_err()
            .code(),
        Some(DeviceErrorCode::DeviceVerbRefused)
    );
    assert_eq!(
        svc.relay_call("u2", &phone, "tap", json!({})).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceNotFound)
    );

    // Switch on (15 min), then off: the helper is told to stop.
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
    assert_eq!(on.status, DeviceStatus::On);
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: false,
            timeout_minutes: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(next(&mut ws).await.unwrap(), json!({"type": "stop", "device": "R5CX"}));

    // Stop pressed on the phone: the core switch goes off too.
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(30),
        },
    )
    .await
    .unwrap();
    send(
        &mut ws,
        json!({"type": "event", "device": "R5CX", "name": "local_switch_off", "data": {}}),
    )
    .await;
    for _ in 0..200 {
        if !svc.get("u1", &phone).await.unwrap().enabled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!svc.get("u1", &phone).await.unwrap().enabled);

    // Reconnect with the token: same client, no new token; the old socket ends.
    let mut again = open(port, None).await.unwrap();
    send(
        &mut again,
        json!({"type": "hello", "token": token, "kind": "mac_helper", "version": "0.1.1", "capabilities": ["tap"]}),
    )
    .await;
    let welcome = next(&mut again).await.unwrap();
    assert_eq!(welcome["client_id"], client_id.as_str());
    assert!(welcome.get("token").is_none());
    assert!(next(&mut ws).await.is_none(), "replaced");
    send(&mut again, json!({"type": "ping"})).await;
    assert_eq!(next(&mut again).await.unwrap(), json!({"type": "pong"}));
    // Not re-inventoried yet on this socket: the phone is offline.
    assert_eq!(svc.get("u1", &phone).await.unwrap().status, DeviceStatus::Offline);
    assert_eq!(
        svc.relay_call("u1", &phone, "tap", json!({})).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceOffline)
    );

    // Remove the Mac card = revoke: socket gets DEVICE_REVOKED, token is dead.
    svc.start_job("u1", &mac, true).await.unwrap();
    assert_eq!(
        next(&mut again).await.unwrap(),
        json!({"type": "error", "code": "DEVICE_REVOKED"})
    );
    let mut third = open(port, None).await.unwrap();
    send(
        &mut third,
        json!({"type": "hello", "token": token, "kind": "mac_helper", "version": "1"}),
    )
    .await;
    assert_eq!(next(&mut third).await.unwrap()["code"], "DEVICE_REVOKED");
    let list = svc.list("u1").await.unwrap();
    assert!(
        list.iter().all(|d| d.id != phone && d.id != mac),
        "revoked devices leave the page"
    );
}

/// Answer the helper side of agent calls: `foreground` / `mac_foreground`
/// say which app is in front (changeable through the returned handle); every
/// other verb succeeds. Returns the verbs that reached the device.
fn serve_device(
    mut ws: Ws,
    front: &str,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<String>,
    Arc<std::sync::Mutex<String>>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let front = Arc::new(std::sync::Mutex::new(front.to_owned()));
    let shared = front.clone();
    tokio::spawn(async move {
        while let Some(frame) = next(&mut ws).await {
            if frame["type"] != "request" {
                continue;
            }
            let verb = frame["verb"].as_str().unwrap_or_default().to_owned();
            let app = shared.lock().unwrap().clone();
            let result = match verb.as_str() {
                "foreground" => json!({"package": app}),
                "mac_foreground" => json!({"bundle_id": app}),
                _ => json!({"ok": true}),
            };
            send(
                &mut ws,
                json!({"type": "response", "id": frame["id"], "ok": true, "result": result}),
            )
            .await;
            if !verb.ends_with("foreground") {
                let _ = tx.send(verb);
            }
        }
    });
    (rx, front)
}

fn serve_phone(ws: Ws, front: &'static str) -> tokio::sync::mpsc::UnboundedReceiver<String> {
    serve_device(ws, front).0
}

fn agent(device: &str, verb: &str) -> aionui_devices::AgentCall {
    aionui_devices::AgentCall {
        user_id: "u1".into(),
        conversation_id: Some("c1".into()),
        device_id: device.into(),
        verb: verb.into(),
        args: json!({}),
    }
}

async fn pending(svc: &Arc<DevicesService>) -> aionui_api_types::devices::DeviceApproval {
    for _ in 0..300 {
        if let Some(a) = svc.approvals("u1").into_iter().next() {
            return a;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no approval was requested");
}

/// K3: switch, verb allowlist, and no prompt on an ordinary app.
#[tokio::test]
async fn agent_calls_need_the_switch_and_an_allowed_verb() {
    let (svc, port) = harness().await;
    let (ws, _token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    let mut reached = serve_phone(ws, "com.android.chrome");

    assert_eq!(
        svc.agent_call(agent(&phone, "tap")).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceDisabled),
        "switch off"
    );
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    for verb in ["stop", "cdp_frame", "addon_run", "shell", "mac_click"] {
        assert_eq!(
            svc.agent_call(agent(&phone, verb)).await.unwrap_err().code(),
            Some(DeviceErrorCode::DeviceVerbRefused),
            "{verb}"
        );
    }
    let mut other = agent(&phone, "tap");
    other.user_id = "u2".into();
    assert_eq!(
        svc.agent_call(other).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceNotFound)
    );

    // An ordinary app: actions and reads pass without a prompt.
    svc.agent_call(agent(&phone, "tap")).await.unwrap();
    svc.agent_call(agent(&phone, "screenshot")).await.unwrap();
    assert!(svc.approvals("u1").is_empty());
    assert_eq!(reached.recv().await.as_deref(), Some("tap"));
    assert_eq!(reached.recv().await.as_deref(), Some("screenshot"));

    let audit = svc.audit("u1", &phone, None, Some(50)).await.unwrap().rows;
    assert!(audit.iter().any(|r| r.verb == "tap"
        && r.outcome == aionui_api_types::devices::DeviceAuditOutcome::Ok
        && r.package.as_deref() == Some("com.android.chrome")
        && r.conversation_id.as_deref() == Some("c1")));
    assert!(
        audit
            .iter()
            .any(|r| r.verb == "shell" && r.outcome == aionui_api_types::devices::DeviceAuditOutcome::Refused)
    );
}

/// Owner decision 2026-11-17: on a crucial app reads are gated like actions,
/// and one OK covers reads and actions on that device + app for 5 minutes.
#[tokio::test]
async fn a_crucial_app_gates_reads_and_actions_and_one_ok_covers_both() {
    let (svc, port) = harness().await;
    let (ws, _token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    let mut reached = serve_phone(ws, "com.whatsapp");
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();

    // A READ waits for the owner; a deny refuses it and nothing reaches the phone.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let read = tokio::spawn(async move { svc2.agent_call(agent(&phone2, "screenshot")).await });
    let ask = pending(&svc).await;
    assert_eq!(
        (ask.verb.as_str(), ask.package.as_deref()),
        ("screenshot", Some("com.whatsapp"))
    );
    assert_eq!(ask.grant_minutes, Some(5));
    assert!(svc.approvals("u2").is_empty());
    svc.decide_approval("u1", &ask.id, false).unwrap();
    assert_eq!(
        read.await.unwrap().unwrap_err().code(),
        Some(DeviceErrorCode::DeviceApprovalDenied)
    );
    assert!(reached.try_recv().is_err(), "a denied read never reaches the phone");

    // Allow an ACTION: it runs, and the following read needs no new prompt.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let act = tokio::spawn(async move { svc2.agent_call(agent(&phone2, "text")).await });
    let ask = pending(&svc).await;
    assert_eq!(ask.verb, "text");
    svc.decide_approval("u1", &ask.id, true).unwrap();
    act.await.unwrap().unwrap();
    assert_eq!(reached.recv().await.as_deref(), Some("text"));
    svc.agent_call(agent(&phone, "ui_tree")).await.unwrap();
    assert!(svc.approvals("u1").is_empty(), "covered by the 5-minute OK");
    assert_eq!(reached.recv().await.as_deref(), Some("ui_tree"));
    assert_eq!(
        svc.decide_approval("u1", &ask.id, true).unwrap_err().code(),
        Some(DeviceErrorCode::DeviceApprovalExpired)
    );

    let outcomes: Vec<_> = svc
        .audit("u1", &phone, None, Some(50))
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.outcome)
        .collect();
    use aionui_api_types::devices::DeviceAuditOutcome as O;
    // The denied read, then the allowed action and the read its OK covered.
    assert_eq!(outcomes.iter().filter(|o| **o == O::Denied).count(), 1);
    assert_eq!(outcomes.iter().filter(|o| **o == O::Approved).count(), 2);
}

/// Password managers: their screen is never read (no prompt); actions ask.
#[tokio::test]
async fn a_password_manager_is_never_read() {
    let (svc, port) = harness().await;
    let (ws, _token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    let mut reached = serve_phone(ws, "com.x8bit.bitwarden");
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    for verb in ["screenshot", "ui_tree"] {
        assert_eq!(
            svc.agent_call(agent(&phone, verb)).await.unwrap_err().code(),
            Some(DeviceErrorCode::DeviceVerbRefused),
            "{verb}"
        );
    }
    assert!(svc.approvals("u1").is_empty(), "no OK is possible for a read");
    assert!(reached.try_recv().is_err());

    // An action asks; allowed, it runs, but a read stays refused afterwards.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let act = tokio::spawn(async move { svc2.agent_call(agent(&phone2, "tap")).await });
    let ask = pending(&svc).await;
    svc.decide_approval("u1", &ask.id, true).unwrap();
    act.await.unwrap().unwrap();
    assert_eq!(reached.recv().await.as_deref(), Some("tap"));
    assert_eq!(
        svc.agent_call(agent(&phone, "screenshot")).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceVerbRefused),
        "the OK for actions never unlocks reading a password manager"
    );
}

/// A pending question is refused when the device is switched off.
#[tokio::test]
async fn switching_off_refuses_what_waits_for_an_ok() {
    let (svc, port) = harness().await;
    let (ws, _token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    let _reached = serve_phone(ws, "com.paypal.android.p2pmobile");
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let act = tokio::spawn(async move { svc2.agent_call(agent(&phone2, "tap")).await });
    pending(&svc).await;
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: false,
            timeout_minutes: None,
        },
    )
    .await
    .unwrap();
    assert!(svc.approvals("u1").is_empty());
    let err = tokio::time::timeout(Duration::from_secs(5), act)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code(), Some(DeviceErrorCode::DeviceApprovalTimeout));
}

/// Director 2026-11-17: a phone reached via the Mac relay AND the companion
/// app is ONE card with both routes; calls go over the route that serves them.
#[tokio::test]
async fn a_phone_reached_both_ways_is_one_card() {
    let (svc, port) = harness().await;
    // The companion app pairs first.
    let code = svc
        .pairing_code("u1", DevicePairingKind::Companion, Some("h"))
        .unwrap()
        .code;
    let mut app = open(port, None).await.unwrap();
    send(
        &mut app,
        json!({"type": "pair", "code": code, "kind": "companion", "name": "Pixel", "version": "0.1.0",
               "capabilities": ["tap", "key", "foreground"]}),
    )
    .await;
    let companion_id = next(&mut app).await.unwrap()["client_id"].as_str().unwrap().to_owned();
    send(
        &mut app,
        json!({"type": "inventory", "phones": [{"serial": "self", "name": "Pixel", "transport": "self", "enabled": true}]}),
    )
    .await;
    let card = format!("phone:{companion_id}:self");
    wait_status(&svc, &card, DeviceStatus::Off).await;

    // The Mac helper sees the same phone over adb and names its companion.
    let code = svc.pairing_code("u1", DevicePairingKind::MacHelper, None).unwrap().code;
    let mut mac = open(port, None).await.unwrap();
    send(
        &mut mac,
        json!({"type": "pair", "code": code, "kind": "mac_helper", "name": "MacBook", "version": "0.1.0",
               "capabilities": ["tap", "foreground", "logcat"]}),
    )
    .await;
    let mac_id = next(&mut mac).await.unwrap()["client_id"].as_str().unwrap().to_owned();
    send(
        &mut mac,
        json!({"type": "inventory", "phones": [
            {"serial": "R5CX", "name": "Pixel 9", "transport": "usb", "enabled": true, "companion_client_id": companion_id},
            {"serial": "OTHER", "name": "Tablet", "transport": "usb", "enabled": true, "companion_client_id": "pair_someone_else"}
        ]}),
    )
    .await;
    let hidden = format!("phone:{mac_id}:R5CX");
    let tablet = format!("phone:{mac_id}:OTHER");
    wait_status(&svc, &tablet, DeviceStatus::Off).await;

    let list = svc.list("u1").await.unwrap();
    let phones: Vec<&str> = list
        .iter()
        .filter(|d| d.kind == DeviceKind::Phone)
        .map(|d| d.id.as_str())
        .collect();
    assert_eq!(phones.len(), 2, "{phones:?}");
    assert!(phones.contains(&card.as_str()) && phones.contains(&tablet.as_str()));
    assert!(
        !phones.contains(&hidden.as_str()),
        "the Mac-relay row is inside the companion card"
    );
    let merged = list.iter().find(|d| d.id == card).unwrap();
    let routes: Vec<_> = merged.routes.as_ref().unwrap().iter().map(|r| r.route).collect();
    assert_eq!(
        routes,
        [
            aionui_api_types::devices::DeviceRouteKind::MacRelay,
            aionui_api_types::devices::DeviceRouteKind::Companion,
            aionui_api_types::devices::DeviceRouteKind::WifiDirect
        ]
    );
    // An id that is not one of this user's companion pairings merges nothing.
    assert_eq!(
        list.iter()
            .find(|d| d.id == tablet)
            .unwrap()
            .routes
            .as_ref()
            .unwrap()
            .len(),
        2
    );

    // One switch for the card: the hidden row follows it.
    svc.update(
        "u1",
        &card,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    assert!(svc.is_enabled("u1", &hidden).await.unwrap());

    // `tap` is served by both: it goes over the Mac relay. `key` only by the app.
    let svc2 = svc.clone();
    let card2 = card.clone();
    let tap = tokio::spawn(async move { svc2.routed_call("u1", &card2, "tap", json!({})).await });
    let req = next(&mut mac).await.unwrap();
    assert_eq!(
        (req["verb"].as_str(), req["device"].as_str()),
        (Some("tap"), Some("R5CX"))
    );
    send(
        &mut mac,
        json!({"type": "response", "id": req["id"], "ok": true, "result": {}}),
    )
    .await;
    tap.await.unwrap().unwrap();

    let svc2 = svc.clone();
    let card2 = card.clone();
    let key = tokio::spawn(async move { svc2.routed_call("u1", &card2, "key", json!({"code": 4})).await });
    // TASK-175: after its inventory the companion is told the access mode
    // in effect (its chip shows it); like the real app, skip to the request.
    let mut req = next(&mut app).await.unwrap();
    let mut told_mode = false;
    while req["type"] == "access" {
        assert_eq!(
            (req["device"].as_str(), req["mode"].as_str()),
            (Some("self"), Some("ask"))
        );
        told_mode = true;
        req = next(&mut app).await.unwrap();
    }
    assert!(told_mode, "the companion was told the access mode");
    assert_eq!(
        (req["verb"].as_str(), req["device"].as_str()),
        (Some("key"), Some("self"))
    );
    send(
        &mut app,
        json!({"type": "response", "id": req["id"], "ok": true, "result": {}}),
    )
    .await;
    key.await.unwrap().unwrap();

    // Off: both routes are told to stop.
    svc.update(
        "u1",
        &card,
        &DeviceUpdate {
            enabled: false,
            timeout_minutes: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(next(&mut mac).await.unwrap(), json!({"type": "stop", "device": "R5CX"}));
    assert_eq!(next(&mut app).await.unwrap(), json!({"type": "stop", "device": "self"}));
    assert!(!svc.is_enabled("u1", &hidden).await.unwrap());
}

fn agent_with(device: &str, verb: &str, args: Value, conversation: &str) -> aionui_devices::AgentCall {
    aionui_devices::AgentCall {
        user_id: "u1".into(),
        conversation_id: Some(conversation.into()),
        device_id: device.into(),
        verb: verb.into(),
        args,
    }
}

async fn enabled_phone(
    svc: &Arc<DevicesService>,
    port: u16,
    front: &str,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    Arc<std::sync::Mutex<String>>,
) {
    let (ws, _token, client_id) = pair_mac(svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(svc, &phone, DeviceStatus::Off).await;
    let (reached, handle) = serve_device(ws, front);
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    (phone, reached, handle)
}

/// Review part 2, blocker 1: a package in the agent's arguments never decides
/// the gate; the device's real front app does.
#[tokio::test]
async fn a_spoofed_package_argument_does_not_skip_the_gate() {
    let (svc, port) = harness().await;
    let (phone, mut reached, front) = enabled_phone(&svc, port, "com.x8bit.bitwarden").await;
    // A password manager is in front: naming a harmless package changes nothing.
    let spoofed = svc
        .agent_call(agent_with(
            &phone,
            "screenshot",
            json!({"package": "com.example.harmless"}),
            "c1",
        ))
        .await
        .unwrap_err();
    assert_eq!(spoofed.code(), Some(DeviceErrorCode::DeviceVerbRefused));
    assert!(reached.try_recv().is_err(), "the screen was not read");

    // A bank in front: an action with a spoofed package still asks.
    *front.lock().unwrap() = "de.mybank.app".into();
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let tap = tokio::spawn(async move {
        svc2.agent_call(agent_with(
            &phone2,
            "tap",
            json!({"x": 1, "y": 1, "package": "com.example.harmless"}),
            "c1",
        ))
        .await
    });
    let ask = pending(&svc).await;
    assert_eq!(
        ask.package.as_deref(),
        Some("de.mybank.app"),
        "the prompt names the REAL app"
    );
    svc.decide_approval("u1", &ask.id, false).unwrap();
    assert_eq!(
        tap.await.unwrap().unwrap_err().code(),
        Some(DeviceErrorCode::DeviceApprovalDenied)
    );
    assert!(reached.try_recv().is_err());
}

/// `launch` is judged by the app it opens AND by the app in front.
#[tokio::test]
async fn launch_checks_the_launched_app_and_the_front_app() {
    let (svc, port) = harness().await;
    let (phone, mut reached, front) = enabled_phone(&svc, port, "com.android.chrome").await;
    // Harmless front app, harmless target: no prompt.
    svc.agent_call(agent_with(&phone, "launch", json!({"package": "org.wikipedia"}), "c1"))
        .await
        .unwrap();
    assert_eq!(reached.recv().await.as_deref(), Some("launch"));
    assert!(svc.approvals("u1").is_empty());

    for (front_app, target) in [
        ("com.android.chrome", "com.whatsapp"),
        ("com.paypal.android.p2pmobile", "org.wikipedia"),
    ] {
        *front.lock().unwrap() = front_app.into();
        let svc2 = svc.clone();
        let phone2 = phone.clone();
        let launch = tokio::spawn(async move {
            svc2.agent_call(agent_with(&phone2, "launch", json!({"package": target}), "c1"))
                .await
        });
        let ask = pending(&svc).await;
        assert_eq!(ask.verb, "launch", "{front_app} -> {target}");
        svc.decide_approval("u1", &ask.id, false).unwrap();
        assert_eq!(
            launch.await.unwrap().unwrap_err().code(),
            Some(DeviceErrorCode::DeviceApprovalDenied)
        );
    }
    assert!(reached.try_recv().is_err(), "no denied launch ran");
}

/// The OK is per conversation, and the front app is checked again right
/// before the call runs.
#[tokio::test]
async fn an_ok_is_per_conversation_and_the_front_app_is_rechecked() {
    let (svc, port) = harness().await;
    let (phone, mut reached, front) = enabled_phone(&svc, port, "com.whatsapp").await;
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let first = tokio::spawn(async move { svc2.agent_call(agent_with(&phone2, "tap", json!({}), "c1")).await });
    let ask = pending(&svc).await;
    svc.decide_approval("u1", &ask.id, true).unwrap();
    first.await.unwrap().unwrap();
    assert_eq!(reached.recv().await.as_deref(), Some("tap"));
    // Same conversation: covered. Another conversation: asks again.
    svc.agent_call(agent_with(&phone, "tap", json!({}), "c1"))
        .await
        .unwrap();
    assert_eq!(reached.recv().await.as_deref(), Some("tap"));
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let other = tokio::spawn(async move { svc2.agent_call(agent_with(&phone2, "tap", json!({}), "c2")).await });
    let ask = pending(&svc).await;
    assert_eq!(ask.conversation_id.as_deref(), Some("c2"));

    // While the owner decides, another app comes to the front: allowed, but
    // nothing runs because the OK was for the app that was in front.
    *front.lock().unwrap() = "de.mybank.app".into();
    svc.decide_approval("u1", &ask.id, true).unwrap();
    let err = other.await.unwrap().unwrap_err();
    assert_eq!(err.code(), Some(DeviceErrorCode::DeviceVerbRefused));
    assert!(err.to_string().contains("changed"), "{err}");
    assert!(reached.try_recv().is_err(), "the tap did not land in the bank app");
    let audit = svc.audit("u1", &phone, None, Some(50)).await.unwrap().rows;
    assert!(
        audit
            .iter()
            .any(|r| r.detail.as_deref() == Some("front_app_changed") && r.package.as_deref() == Some("de.mybank.app"))
    );
}

/// Review part 2, blocker 2: the Mac is gated like the phone, by its real
/// frontmost app; an unknown front app asks every time (single-use OK).
#[tokio::test]
async fn the_mac_is_gated_by_its_front_app() {
    let (svc, port) = harness().await;
    let (ws, _token, client_id) = pair_mac_with(
        &svc,
        port,
        &["mac_foreground", "mac_screenshot", "mac_click", "mac_type", "stop"],
    )
    .await;
    let mac = format!("mac:{client_id}:mac");
    wait_status(&svc, &mac, DeviceStatus::Off).await;
    let (mut reached, front) = serve_device(ws, "com.apple.Safari");
    svc.update(
        "u1",
        &mac,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    // An ordinary app: no prompt.
    svc.agent_call(agent_with(&mac, "mac_click", json!({"x": 1, "y": 1}), "c1"))
        .await
        .unwrap();
    assert_eq!(reached.recv().await.as_deref(), Some("mac_click"));

    // Mail in front: reads and actions ask.
    *front.lock().unwrap() = "com.apple.mail".into();
    for verb in ["mac_screenshot", "mac_type"] {
        let svc2 = svc.clone();
        let mac2 = mac.clone();
        let call = tokio::spawn(async move { svc2.agent_call(agent_with(&mac2, verb, json!({}), verb)).await });
        let ask = pending(&svc).await;
        assert_eq!(
            (ask.verb.as_str(), ask.package.as_deref()),
            (verb, Some("com.apple.mail"))
        );
        svc.decide_approval("u1", &ask.id, false).unwrap();
        assert_eq!(
            call.await.unwrap().unwrap_err().code(),
            Some(DeviceErrorCode::DeviceApprovalDenied)
        );
    }
    // Keychain Access in front: its screen is never read.
    *front.lock().unwrap() = "com.apple.keychainaccess".into();
    assert_eq!(
        svc.agent_call(agent_with(&mac, "mac_screenshot", json!({}), "c1"))
            .await
            .unwrap_err()
            .code(),
        Some(DeviceErrorCode::DeviceVerbRefused)
    );
    assert!(svc.approvals("u1").is_empty());
    assert!(reached.try_recv().is_err());

    // The helper cannot say which app is in front: asks, and that OK is single-use.
    *front.lock().unwrap() = String::new();
    for round in 0..2 {
        let svc2 = svc.clone();
        let mac2 = mac.clone();
        let call = tokio::spawn(async move { svc2.agent_call(agent_with(&mac2, "mac_click", json!({}), "c1")).await });
        let ask = pending(&svc).await;
        assert_eq!(
            ask.grant_minutes, None,
            "round {round}: no 5-minute grant for an unknown app"
        );
        svc.decide_approval("u1", &ask.id, true).unwrap();
        call.await.unwrap().unwrap();
        assert_eq!(reached.recv().await.as_deref(), Some("mac_click"));
    }
}

// -- TASK-172: a call never hangs on a device that cannot answer ---------------

async fn audit_rows(svc: &Arc<DevicesService>, device: &str) -> Vec<aionui_api_types::devices::DeviceAuditEntry> {
    svc.audit("u1", device, None, Some(50)).await.unwrap().rows
}

/// The live bug: the Android companion echoes the request id as a JSON string
/// (`"id": "1"`). That answer resolves the call instead of being dropped.
#[tokio::test]
async fn a_response_with_a_string_id_answers_the_call() {
    let (svc, port) = harness().await;
    let (mut ws, _token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    tokio::spawn(async move {
        while let Some(frame) = next(&mut ws).await {
            if frame["type"] == "request" {
                let id = frame["id"].as_u64().unwrap().to_string();
                send(
                    &mut ws,
                    json!({"type": "response", "id": id, "ok": true, "result": {"package": "org.wikipedia"}}),
                )
                .await;
            }
        }
    });
    let started = std::time::Instant::now();
    let front = tokio::time::timeout(Duration::from_secs(5), svc.agent_call(agent(&phone, "foreground")))
        .await
        .expect("the answer was read")
        .unwrap();
    assert_eq!(front["package"], "org.wikipedia");
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// A connected device that never answers: the gate's front-app lookup gives up
/// after 10 s with DEVICE_OFFLINE, the owner is not asked, and the call is in
/// the audit log.
#[tokio::test]
async fn a_silent_device_fails_fast_with_a_coded_error_and_an_audit_row() {
    let (svc, port) = harness().await;
    let (mut ws, _token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    // Keeps the socket open (pings) but answers no request.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = ws.next() => if frame.is_none() { break },
                () = tokio::time::sleep(Duration::from_secs(2)) => send(&mut ws, json!({"type": "ping"})).await,
            }
        }
    });
    for verb in ["foreground", "launch"] {
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(Duration::from_secs(13), svc.agent_call(agent(&phone, verb)))
            .await
            .unwrap_or_else(|_| panic!("{verb} still waiting after 13 s"))
            .unwrap_err();
        assert_eq!(err.code(), Some(DeviceErrorCode::DeviceOffline), "{verb}");
        assert!(
            started.elapsed() >= Duration::from_secs(9),
            "{verb}: the device had its 10 s"
        );
        assert!(
            svc.approvals("u1").is_empty(),
            "{verb}: nobody is asked about a silent device"
        );
        let audit = audit_rows(&svc, &phone).await;
        assert!(
            audit.iter().any(|r| r.verb == verb
                && r.outcome == aionui_api_types::devices::DeviceAuditOutcome::Error
                && r.detail.as_deref() == Some("DEVICE_OFFLINE")),
            "{verb}: {audit:?}"
        );
    }
}

/// No connected relay client: DEVICE_OFFLINE at once (no owner prompt, no
/// wait), an audit row, and the card says offline with a fresh "last seen".
#[tokio::test]
async fn a_disconnected_device_is_offline_at_once() {
    let (svc, port) = harness().await;
    let (mut ws, _token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    wait_status(&svc, &phone, DeviceStatus::On).await;

    // "Last seen" follows the client's frames (pings), not only the inventory.
    let before = svc.get("u1", &phone).await.unwrap().last_seen_at.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    send(&mut ws, json!({"type": "ping"})).await;
    assert_eq!(next(&mut ws).await.unwrap()["type"], "pong");
    let seen = svc.get("u1", &phone).await.unwrap().last_seen_at.unwrap();
    assert!(seen > before, "{seen} > {before}");

    ws.close(None).await.unwrap();
    wait_status(&svc, &phone, DeviceStatus::Offline).await;
    let card = svc.get("u1", &phone).await.unwrap();
    assert!(card.enabled, "the switch stays as the owner left it");
    assert!(card.last_seen_at.unwrap() >= seen);

    for verb in ["foreground", "tap", "launch", "screenshot"] {
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(Duration::from_secs(3), svc.agent_call(agent(&phone, verb)))
            .await
            .unwrap_or_else(|_| panic!("{verb} hangs on an offline device"))
            .unwrap_err();
        assert_eq!(err.code(), Some(DeviceErrorCode::DeviceOffline), "{verb}");
        assert!(started.elapsed() < Duration::from_secs(1), "{verb}");
        assert!(svc.approvals("u1").is_empty(), "{verb}");
    }
    let audit = audit_rows(&svc, &phone).await;
    for verb in ["foreground", "tap", "launch", "screenshot"] {
        assert!(
            audit.iter().any(|r| r.verb == verb
                && r.outcome == aionui_api_types::devices::DeviceAuditOutcome::Error
                && r.detail.as_deref() == Some("DEVICE_OFFLINE")),
            "{verb}: {audit:?}"
        );
    }
}

/// An answer core cannot parse but that names its request fails THAT request
/// at once with DEVICE_BAD_MESSAGE (it used to stay pending until the timeout).
#[tokio::test]
async fn an_unreadable_answer_fails_its_request_at_once() {
    let (svc, port) = harness().await;
    let (mut ws, _token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    tokio::spawn(async move {
        while let Some(frame) = next(&mut ws).await {
            if frame["type"] == "request" {
                // `ok` must be a boolean: this frame is not a valid response.
                send(&mut ws, json!({"type": "response", "id": frame["id"], "ok": "yes"})).await;
            }
        }
    });
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(5), svc.agent_call(agent(&phone, "foreground")))
        .await
        .expect("the request did not stay pending")
        .unwrap_err();
    assert_eq!(err.code(), Some(DeviceErrorCode::DeviceBadMessage));
    assert!(started.elapsed() < Duration::from_secs(2));
    let audit = audit_rows(&svc, &phone).await;
    assert!(
        audit.iter().any(|r| r.verb == "foreground"
            && r.outcome == aionui_api_types::devices::DeviceAuditOutcome::Error
            && r.detail.as_deref() == Some("DEVICE_BAD_MESSAGE")),
        "{audit:?}"
    );
}

// -- TASK-173: the companion 0.2 verbs and the phone's health ------------------

const COMPANION_02_VERBS: &[&str] = &[
    "screenshot",
    "ui_tree",
    "tap",
    "swipe",
    "text",
    "key",
    "global",
    "launch",
    "open_url",
    "foreground",
    "click_text",
    "set_text",
    "stop",
];

async fn approve_next(svc: &Arc<DevicesService>, allow: bool) -> aionui_api_types::devices::DeviceApproval {
    let ask = pending(svc).await;
    svc.decide_approval("u1", &ask.id, allow).unwrap();
    ask
}

#[tokio::test]
async fn the_new_verbs_go_through_the_gate() {
    let (svc, port) = harness().await;
    let (ws, _token, client_id) = pair_mac_with(&svc, port, COMPANION_02_VERBS).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    let (mut reached, front) = serve_device(ws, "org.wikipedia");
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();

    // An ordinary app in front: no prompt.
    for (verb, args) in [
        ("click_text", json!({"text": "Search", "index": 1})),
        ("set_text", json!({"value": ""})),
        ("text", json!({"value": "abc", "replace": true})),
        ("screenshot", json!({"format": "jpeg", "scale": 0.5})),
        ("global", json!({"action": "lock"})),
    ] {
        svc.agent_call(agent_with(&phone, verb, args, "c1")).await.unwrap();
        assert_eq!(reached.recv().await.as_deref(), Some(verb));
    }
    assert!(svc.approvals("u1").is_empty());

    // Arguments are checked before anything is asked or sent.
    for (verb, args) in [
        ("click_text", json!({"text": "   "})),
        ("click_text", json!({"text": "OK", "index": 100})),
        ("set_text", json!({})),
        ("text", json!({"value": "a", "replace": "yes"})),
        ("screenshot", json!({"format": "gif"})),
        ("global", json!({"action": "reboot"})),
        ("open_url", json!({"url": "file:///sdcard/x"})),
        ("open_url", json!({"url": "intent://scan/#Intent;end"})),
        ("open_url", json!({"url": "https://example.com/a b"})),
    ] {
        let err = svc
            .agent_call(agent_with(&phone, verb, args.clone(), "c1"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some(DeviceErrorCode::DeviceBadMessage), "{verb} {args}");
    }
    // Private / local addresses and AionUi itself: refused, never asked.
    for url in [
        "http://192.168.1.1/admin",
        "http://localhost:8080/",
        "https://aionui.example/api/devices",
    ] {
        let err = svc
            .agent_call(agent_with(&phone, "open_url", json!({"url": url}), "c1"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some(DeviceErrorCode::DeviceVerbRefused), "{url}");
    }
    assert!(svc.approvals("u1").is_empty());
    assert!(reached.try_recv().is_err(), "nothing refused reached the phone");

    // A public link asks EVERY time (no 5-minute grant), and shows the link.
    for round in 0..2 {
        let svc2 = svc.clone();
        let phone2 = phone.clone();
        let call = tokio::spawn(async move {
            svc2.agent_call(agent_with(
                &phone2,
                "open_url",
                json!({"url": "https://example.com/x"}),
                "c1",
            ))
            .await
        });
        let ask = approve_next(&svc, true).await;
        assert!(
            ask.summary.contains("https://example.com/x"),
            "round {round}: {}",
            ask.summary
        );
        assert_eq!(ask.grant_minutes, None, "round {round}");
        call.await.unwrap().unwrap();
        assert_eq!(reached.recv().await.as_deref(), Some("open_url"));
    }
    // The store link is allowed as a scheme and asks too.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let market = tokio::spawn(async move {
        svc2.agent_call(agent_with(
            &phone2,
            "open_url",
            json!({"url": "market://details?id=org.wikipedia"}),
            "c1",
        ))
        .await
    });
    approve_next(&svc, false).await;
    assert_eq!(
        market.await.unwrap().unwrap_err().code(),
        Some(DeviceErrorCode::DeviceApprovalDenied)
    );

    // The notification shade and quick settings ask whatever is in front;
    // one OK covers both for this conversation, not ordinary crucial apps.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let shade = tokio::spawn(async move {
        svc2.agent_call(agent_with(&phone2, "global", json!({"action": "notifications"}), "c1"))
            .await
    });
    let ask = approve_next(&svc, true).await;
    assert_eq!(ask.verb, "global");
    assert!(ask.grant_minutes.is_some());
    shade.await.unwrap().unwrap();
    assert_eq!(reached.recv().await.as_deref(), Some("global"));
    svc.agent_call(agent_with(&phone, "global", json!({"action": "quick_settings"}), "c1"))
        .await
        .unwrap();
    assert_eq!(reached.recv().await.as_deref(), Some("global"));

    // A bank in front: click_text and set_text ask like a tap.
    *front.lock().unwrap() = "de.mybank.app".into();
    for verb in ["click_text", "set_text"] {
        let svc2 = svc.clone();
        let phone2 = phone.clone();
        let call = tokio::spawn(async move {
            svc2.agent_call(agent_with(&phone2, verb, json!({"text": "Pay", "value": "1"}), verb))
                .await
        });
        let ask = approve_next(&svc, false).await;
        assert_eq!(
            (ask.verb.as_str(), ask.package.as_deref()),
            (verb, Some("de.mybank.app"))
        );
        assert_eq!(
            call.await.unwrap().unwrap_err().code(),
            Some(DeviceErrorCode::DeviceApprovalDenied)
        );
    }
    assert!(reached.try_recv().is_err());
    let audit = audit_rows(&svc, &phone).await;
    assert!(
        audit
            .iter()
            .any(|r| r.verb == "open_url" && r.detail.as_deref() == Some("url_blocked"))
    );
    assert!(
        audit
            .iter()
            .all(|r| !r.detail.as_deref().unwrap_or_default().contains("example.com")),
        "no link in the audit detail"
    );
}

/// The companion's `health` (pair / hello and the event) is kept on the
/// device row and returned with the card.
#[tokio::test]
async fn a_phone_reports_its_health() {
    let (svc, port) = harness().await;
    let code = svc.pairing_code("u1", DevicePairingKind::Companion, None).unwrap().code;
    let mut ws = open(port, None).await.unwrap();
    send(
        &mut ws,
        json!({"type": "pair", "code": code, "kind": "companion", "name": "OnePlus Nord 4", "version": "0.2.0",
               "capabilities": COMPANION_02_VERBS,
               "health": {"battery_percent": 80, "charging": true, "locked": false, "accessibility_on": true,
                          "android_version": "15", "model": "CPH2663"}}),
    )
    .await;
    let welcome = next(&mut ws).await.unwrap();
    let phone = format!("phone:{}:self", welcome["client_id"].as_str().unwrap());
    send(
        &mut ws,
        json!({"type": "inventory", "phones": [{"serial": "self", "name": "OnePlus Nord 4", "transport": "self",
               "chrome": false, "enabled": true}], "addons": []}),
    )
    .await;
    wait_status(&svc, &phone, DeviceStatus::Off).await;
    assert_eq!(
        svc.get("u1", &phone).await.unwrap().version.as_deref(),
        Some("0.2.0"),
        "the app version"
    );
    let health = svc.get("u1", &phone).await.unwrap().health.expect("health from pair");
    assert_eq!(
        (health.battery_percent, health.charging, health.locked),
        (Some(80), Some(true), false)
    );
    assert!(health.accessibility_on);
    assert_eq!(health.model.as_deref(), Some("CPH2663"));

    // A change arrives as an event; a value out of range is dropped, not the frame.
    send(
        &mut ws,
        json!({"type": "event", "device": "self", "name": "health",
               "data": {"battery_percent": 300, "charging": false, "locked": true, "accessibility_on": false}}),
    )
    .await;
    for _ in 0..200 {
        if svc.get("u1", &phone).await.unwrap().health.is_some_and(|h| h.locked) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let health = svc.get("u1", &phone).await.unwrap().health.unwrap();
    assert_eq!(
        (health.battery_percent, health.charging, health.locked),
        (None, Some(false), true)
    );
    assert!(!health.accessibility_on);
    let listed = svc.list("u1").await.unwrap();
    assert!(
        listed
            .iter()
            .any(|d| d.id == phone && d.health.as_ref().is_some_and(|h| h.locked))
    );
    // A Mac-relay phone has no health.
    assert!(listed.iter().filter(|d| d.id != phone).all(|d| d.health.is_none()));
}

/// Live finding 2026-11-18: with a game on screen a OnePlus reports its game
/// toolbar (`com.oplus.games`) as the front app. An overlay says nothing about
/// the app underneath: the call asks, and that OK is single-use.
#[tokio::test]
async fn an_overlay_in_front_counts_as_an_unknown_app() {
    let (svc, port) = harness().await;
    let (phone, mut reached, front) = enabled_phone(&svc, port, "com.oplus.games").await;
    for overlay in ["com.oplus.games", "com.android.systemui"] {
        *front.lock().unwrap() = overlay.into();
        for round in 0..2 {
            let svc2 = svc.clone();
            let phone2 = phone.clone();
            let call = tokio::spawn(async move { svc2.agent_call(agent_with(&phone2, "tap", json!({}), "c1")).await });
            let ask = approve_next(&svc, true).await;
            assert_eq!(ask.package.as_deref(), Some("unknown"), "{overlay}");
            assert_eq!(ask.grant_minutes, None, "{overlay} round {round}");
            call.await.unwrap().unwrap();
            assert_eq!(reached.recv().await.as_deref(), Some("tap"));
        }
    }
}

/// Reconnect with the stored token and report the same phone again.
async fn reconnect_mac(port: u16, token: &str) -> Ws {
    let mut ws = open(port, None).await.unwrap();
    send(
        &mut ws,
        json!({"type": "hello", "token": token, "kind": "mac_helper", "version": "0.1.0",
               "capabilities": ["tap", "screenshot", "foreground", "text", "ui_tree", "launch", "stop"]}),
    )
    .await;
    assert_eq!(next(&mut ws).await.unwrap()["type"], "welcome");
    send(
        &mut ws,
        json!({"type": "inventory",
               "phones": [{"serial": "R5CX", "name": "Galaxy", "transport": "usb", "chrome": true, "enabled": true}]}),
    )
    .await;
    ws
}

/// Live finding 2026-11-18: the phone app's socket drops and reconnects within
/// seconds when the app goes to the background. A pending READ is sent once
/// more on the new socket; an ACTION never is (it may already have run).
#[tokio::test]
async fn a_pending_read_is_sent_again_after_a_reconnect_an_action_is_not() {
    let (svc, port) = harness().await;
    let (mut ws, token, client_id) = pair_mac(&svc, port).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(&svc, &phone, DeviceStatus::Off).await;

    // A read: the first socket closes with the request pending.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let read = tokio::spawn(async move { svc2.relay_call("u1", &phone2, "foreground", json!({})).await });
    let request = next(&mut ws).await.unwrap();
    assert_eq!(request["verb"], "foreground");
    ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut ws = reconnect_mac(port, &token).await;
    let again = next(&mut ws).await.expect("the read was sent again");
    assert_eq!(
        (again["type"].as_str(), again["verb"].as_str()),
        (Some("request"), Some("foreground"))
    );
    send(
        &mut ws,
        json!({"type": "response", "id": again["id"], "ok": true, "result": {"package": "org.wikipedia"}}),
    )
    .await;
    assert_eq!(read.await.unwrap().unwrap()["package"], "org.wikipedia");

    // An action: DEVICE_OFFLINE, and nothing arrives on the new socket.
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let tap = tokio::spawn(async move { svc2.relay_call("u1", &phone2, "tap", json!({"x": 1, "y": 1})).await });
    assert_eq!(next(&mut ws).await.unwrap()["verb"], "tap");
    ws.close(None).await.unwrap();
    assert_eq!(
        tap.await.unwrap().unwrap_err().code(),
        Some(DeviceErrorCode::DeviceOffline)
    );
    let mut ws = reconnect_mac(port, &token).await;
    let quiet = tokio::time::timeout(Duration::from_millis(700), ws.next()).await;
    assert!(quiet.is_err(), "the tap was not sent again: {quiet:?}");
}

// -- TASK-175: the access mode of a device -------------------------------------

use aionui_api_types::devices::{DeviceAccessMode, DeviceAccessUpdate, DeviceFullDuration};

fn access(mode: DeviceAccessMode, full: Option<DeviceFullDuration>, sensitive: bool) -> DeviceAccessUpdate {
    DeviceAccessUpdate {
        mode,
        full_duration: full,
        include_sensitive: sensitive,
    }
}

/// A call that must NOT ask: it returns and reaches the phone.
async fn runs_unasked(svc: &Arc<DevicesService>, phone: &str, verb: &str, args: Value) {
    tokio::time::timeout(
        Duration::from_secs(3),
        svc.agent_call(agent_with(phone, verb, args, "c1")),
    )
    .await
    .unwrap_or_else(|_| panic!("{verb} waits for the owner"))
    .unwrap_or_else(|e| panic!("{verb}: {e}"));
    assert!(svc.approvals("u1").is_empty(), "{verb}");
}

/// A call that must ask; it is denied here.
async fn asks(
    svc: &Arc<DevicesService>,
    phone: &str,
    verb: &str,
    args: Value,
) -> aionui_api_types::devices::DeviceApproval {
    let svc2 = svc.clone();
    let phone2 = phone.to_owned();
    let verb2 = verb.to_owned();
    let call = tokio::spawn(async move { svc2.agent_call(agent_with(&phone2, &verb2, args, "c9")).await });
    let ask = approve_next(svc, false).await;
    assert_eq!(
        call.await.unwrap().unwrap_err().code(),
        Some(DeviceErrorCode::DeviceApprovalDenied),
        "{verb}"
    );
    ask
}

async fn phone_with_new_verbs(
    svc: &Arc<DevicesService>,
    port: u16,
    front: &str,
) -> (String, Ws, Arc<std::sync::Mutex<String>>) {
    let (ws, _token, client_id) = pair_mac_with(svc, port, COMPANION_02_VERBS).await;
    let phone = format!("phone:{client_id}:R5CX");
    wait_status(svc, &phone, DeviceStatus::Off).await;
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: true,
            timeout_minutes: Some(15),
        },
    )
    .await
    .unwrap();
    // A second socket handle is not possible: serve on a clone-free task and
    // keep nothing; the caller gets the front-app handle.
    let (reached, handle) = serve_device(ws, front);
    std::mem::forget(reached);
    let dummy = open(port, None).await.unwrap();
    (phone, dummy, handle)
}

#[tokio::test]
async fn trusted_apps_never_ask_everything_else_does() {
    let (svc, port) = harness().await;
    let (phone, _ws, front) = phone_with_new_verbs(&svc, port, "com.whatsapp").await;
    let card = svc.get("u1", &phone).await.unwrap();
    let start = card.access.expect("phones have an access mode");
    assert_eq!(
        (start.mode, start.trusted_apps.len(), start.include_sensitive),
        (DeviceAccessMode::Ask, 0, false)
    );

    // Bad lists are refused; a good one is stored without duplicates.
    for bad in [
        vec!["".to_owned()],
        vec!["com.x/../y".to_owned()],
        vec!["a b".to_owned()],
    ] {
        assert_eq!(
            svc.set_trusted_apps("u1", &phone, &bad).await.unwrap_err().code(),
            Some(DeviceErrorCode::DeviceBadMessage)
        );
    }
    let listed = svc
        .set_trusted_apps(
            "u1",
            &phone,
            &["com.whatsapp".into(), "COM.WHATSAPP".into(), "de.mybank.app".into()],
        )
        .await
        .unwrap();
    assert_eq!(
        listed.access.unwrap().trusted_apps,
        vec!["com.whatsapp", "de.mybank.app"]
    );
    // The list alone changes nothing while the mode is ask.
    asks(&svc, &phone, "tap", json!({})).await;

    let card = svc
        .set_access("u1", &phone, &access(DeviceAccessMode::Trusted, None, false))
        .await
        .unwrap();
    assert_eq!(card.access.unwrap().mode, DeviceAccessMode::Trusted);
    runs_unasked(&svc, &phone, "tap", json!({})).await;
    runs_unasked(&svc, &phone, "screenshot", json!({})).await;
    // Not on the list, unknown, an overlay, a link: as before.
    for other in ["com.google.android.gm", "", "com.oplus.games"] {
        *front.lock().unwrap() = other.into();
        asks(&svc, &phone, "tap", json!({})).await;
    }
    *front.lock().unwrap() = "com.whatsapp".into();
    asks(&svc, &phone, "open_url", json!({"url": "https://example.com/"})).await;
    // A listed banking app still asks until include_sensitive is on.
    *front.lock().unwrap() = "de.mybank.app".into();
    let ask = asks(&svc, &phone, "tap", json!({})).await;
    assert!(ask.sensitive);
    svc.set_access("u1", &phone, &access(DeviceAccessMode::Trusted, None, true))
        .await
        .unwrap();
    runs_unasked(&svc, &phone, "tap", json!({})).await;

    // An inventory refresh keeps the owner's settings.
    let stored = svc.get("u1", &phone).await.unwrap().access.unwrap();
    assert_eq!(
        (stored.mode, stored.include_sensitive, stored.trusted_apps.len()),
        (DeviceAccessMode::Trusted, true, 2)
    );
    let audit = audit_rows(&svc, &phone).await;
    assert!(
        audit.iter().any(|r| r.verb == "tap"
            && r.outcome == aionui_api_types::devices::DeviceAuditOutcome::Ok
            && r.detail.as_deref() == Some("mode=trusted")),
        "{audit:?}"
    );
    // Only the owner of the device; only phones and Macs.
    assert_eq!(
        svc.set_access("u2", &phone, &access(DeviceAccessMode::Trusted, None, false))
            .await
            .unwrap_err()
            .code(),
        Some(DeviceErrorCode::DeviceNotFound)
    );
    assert_eq!(
        svc.set_access(
            "u1",
            "server_browser:chrome",
            &access(DeviceAccessMode::Trusted, None, false)
        )
        .await
        .unwrap_err()
        .code(),
        Some(DeviceErrorCode::DeviceBadMessage)
    );
}

#[tokio::test]
async fn full_access_shows_no_cards_keeps_the_hard_stops_and_ends_with_the_switch() {
    let (svc, port) = harness().await;
    let (phone, _ws, front) = phone_with_new_verbs(&svc, port, "com.whatsapp").await;
    // A duration is required, and only goes with full.
    for bad in [
        access(DeviceAccessMode::Full, None, false),
        access(DeviceAccessMode::Ask, Some(DeviceFullDuration::OneHour), false),
    ] {
        assert_eq!(
            svc.set_access("u1", &phone, &bad).await.unwrap_err().code(),
            Some(DeviceErrorCode::DeviceBadMessage)
        );
    }
    let before = aionui_common::now_ms();
    let card = svc
        .set_access(
            "u1",
            &phone,
            &access(DeviceAccessMode::Full, Some(DeviceFullDuration::OneHour), false),
        )
        .await
        .unwrap();
    let a = card.access.clone().unwrap();
    assert_eq!(a.mode, DeviceAccessMode::Full);
    let until = a.full_until.expect("1 h");
    assert!((before + 3_590_000..=before + 3_610_000).contains(&until), "{until}");
    assert_eq!(
        (card.enabled, card.enabled_until),
        (true, Some(until)),
        "the switch lasts as long"
    );

    // No cards: crucial app, unknown app, overlay, link, shade.
    for other in ["com.whatsapp", "", "com.oplus.games", "com.android.settings"] {
        *front.lock().unwrap() = other.into();
        runs_unasked(&svc, &phone, "tap", json!({})).await;
        runs_unasked(&svc, &phone, "screenshot", json!({})).await;
    }
    runs_unasked(&svc, &phone, "open_url", json!({"url": "https://example.com/"})).await;
    runs_unasked(&svc, &phone, "global", json!({"action": "notifications"})).await;
    // The hard stops hold: verb list, private addresses, bad arguments.
    for (verb, args, code) in [
        ("shell", json!({}), DeviceErrorCode::DeviceVerbRefused),
        ("cdp_frame", json!({}), DeviceErrorCode::DeviceVerbRefused),
        (
            "open_url",
            json!({"url": "http://192.168.1.1/"}),
            DeviceErrorCode::DeviceVerbRefused,
        ),
        ("global", json!({"action": "reboot"}), DeviceErrorCode::DeviceBadMessage),
    ] {
        let err = svc.agent_call(agent_with(&phone, verb, args, "c1")).await.unwrap_err();
        assert_eq!(err.code(), Some(code), "{verb}");
    }
    // Sensitive apps keep asking / stay unread without include_sensitive.
    *front.lock().unwrap() = "de.mybank.app".into();
    assert!(asks(&svc, &phone, "tap", json!({})).await.sensitive);
    *front.lock().unwrap() = "com.x8bit.bitwarden".into();
    assert_eq!(
        svc.agent_call(agent_with(&phone, "screenshot", json!({}), "c1"))
            .await
            .unwrap_err()
            .code(),
        Some(DeviceErrorCode::DeviceVerbRefused)
    );
    svc.set_access(
        "u1",
        &phone,
        &access(DeviceAccessMode::Full, Some(DeviceFullDuration::UntilOff), true),
    )
    .await
    .unwrap();
    let card = svc.get("u1", &phone).await.unwrap();
    assert_eq!((card.enabled, card.enabled_until), (true, None), "until I turn it off");
    assert_eq!(card.access.as_ref().unwrap().full_until, None);
    assert!(card.access.as_ref().unwrap().include_sensitive);
    runs_unasked(&svc, &phone, "screenshot", json!({})).await;
    *front.lock().unwrap() = "de.mybank.app".into();
    runs_unasked(&svc, &phone, "tap", json!({})).await;
    let audit = audit_rows(&svc, &phone).await;
    assert!(
        audit
            .iter()
            .any(|r| r.verb == "tap" && r.detail.as_deref() == Some("mode=full"))
    );
    assert!(
        audit.iter().any(|r| r.verb == "shell"
            && r.outcome == aionui_api_types::devices::DeviceAuditOutcome::Refused
            && r.detail.as_deref() == Some("DEVICE_VERB_REFUSED; mode=full")),
        "{audit:?}"
    );

    // The switch goes off: full is over, and switching on again is plain ask.
    svc.update(
        "u1",
        &phone,
        &DeviceUpdate {
            enabled: false,
            timeout_minutes: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        svc.get("u1", &phone).await.unwrap().access.unwrap().mode,
        DeviceAccessMode::Ask
    );
    assert_eq!(
        svc.agent_call(agent_with(&phone, "tap", json!({}), "c1"))
            .await
            .unwrap_err()
            .code(),
        Some(DeviceErrorCode::DeviceDisabled)
    );
    let card = svc
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
    let a = card.access.unwrap();
    assert_eq!((a.mode, a.include_sensitive), (DeviceAccessMode::Ask, false));
    *front.lock().unwrap() = "com.whatsapp".into();
    asks(&svc, &phone, "tap", json!({})).await;

    // Leaving full by choice gives the switch its normal timeout back.
    svc.set_access(
        "u1",
        &phone,
        &access(DeviceAccessMode::Full, Some(DeviceFullDuration::EightHours), false),
    )
    .await
    .unwrap();
    let now = aionui_common::now_ms();
    let card = svc
        .set_access("u1", &phone, &access(DeviceAccessMode::Ask, None, false))
        .await
        .unwrap();
    let until = card.enabled_until.expect("a timeout again");
    assert!(until <= now + 31 * 60_000 && until >= now + 29 * 60_000, "{until}");
    assert_eq!(card.access.unwrap().mode, DeviceAccessMode::Ask);
}

#[tokio::test]
async fn always_allow_adds_the_app_and_an_agent_cannot_change_the_mode() {
    let (svc, port) = harness().await;
    let (phone, _ws, front) = phone_with_new_verbs(&svc, port, "com.whatsapp").await;
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let tap = tokio::spawn(async move { svc2.agent_call(agent_with(&phone2, "tap", json!({}), "c1")).await });
    let ask = pending(&svc).await;
    assert!(!ask.sensitive);
    svc.decide_approval_always("u1", &ask.id).await.unwrap();
    tap.await.unwrap().unwrap();
    let a = svc.get("u1", &phone).await.unwrap().access.unwrap();
    assert_eq!(
        (a.mode, a.trusted_apps),
        (DeviceAccessMode::Trusted, vec!["com.whatsapp".to_owned()])
    );
    // Another conversation, no 5-minute grant involved: still no card.
    svc.agent_call(agent_with(&phone, "tap", json!({}), "c2"))
        .await
        .unwrap();
    assert!(svc.approvals("u1").is_empty());

    // An unknown app cannot be always allowed; the question stays open.
    *front.lock().unwrap() = String::new();
    let svc2 = svc.clone();
    let phone2 = phone.clone();
    let tap = tokio::spawn(async move { svc2.agent_call(agent_with(&phone2, "tap", json!({}), "c1")).await });
    let ask = pending(&svc).await;
    assert_eq!(
        svc.decide_approval_always("u1", &ask.id).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceBadMessage)
    );
    assert_eq!(svc.approvals("u1").len(), 1);
    assert_eq!(
        svc.decide_approval_always("u2", &ask.id).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceApprovalExpired),
        "another user's approval"
    );
    svc.decide_approval("u1", &ask.id, false).unwrap();
    assert!(tap.await.unwrap().is_err());

    // The agent surface has no verb that touches the mode or the list.
    *front.lock().unwrap() = "com.android.settings".into();
    for verb in ["access", "set_access", "set_mode", "trusted_apps", "trust_app", "full"] {
        let err = svc
            .agent_call(agent_with(
                &phone,
                verb,
                json!({"mode": "full", "full_duration": "until_off", "apps": ["com.android.settings"]}),
                "c1",
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some(DeviceErrorCode::DeviceVerbRefused), "{verb}");
    }
    assert!(
        !aionui_devices::agent::PHONE_VERBS
            .iter()
            .chain(aionui_devices::agent::MAC_VERBS)
            .any(|v| v.contains("access") || v.contains("trust") || v.contains("mode"))
    );
    let a = svc.get("u1", &phone).await.unwrap().access.unwrap();
    assert_eq!(
        (a.mode, a.trusted_apps.len()),
        (DeviceAccessMode::Trusted, 1),
        "unchanged"
    );
    asks(&svc, &phone, "tap", json!({})).await;
}
