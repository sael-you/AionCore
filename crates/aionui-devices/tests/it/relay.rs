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
    let req = next(&mut app).await.unwrap();
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
