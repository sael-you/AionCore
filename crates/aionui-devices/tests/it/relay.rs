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

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
const WAIT: Duration = Duration::from_secs(5);

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

async fn harness() -> (Arc<DevicesService>, u16) {
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

async fn open(port: u16, origin: Option<&str>) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://127.0.0.1:{port}/api/devices/relay")
        .into_client_request()
        .unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
}

async fn next(ws: &mut Ws) -> Option<Value> {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return serde_json::from_str(&t).ok(),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) | Err(_) => return None,
            Ok(Some(Ok(_))) => {}
        }
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// Pair a fake Mac helper serving `tap` and `screenshot`.
async fn pair_mac(svc: &Arc<DevicesService>, port: u16) -> (Ws, String, String) {
    let code = svc.pairing_code("u1", DevicePairingKind::MacHelper, None).unwrap().code;
    let mut ws = open(port, None).await.unwrap();
    send(
        &mut ws,
        json!({"type": "pair", "code": code, "kind": "mac_helper", "name": "MacBook", "version": "0.1.0",
               "capabilities": ["tap", "screenshot", "stop"]}),
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
        if svc.get("u1", id).await.map(|d| d.status) == Ok(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{id} never became {want:?}: {:?}", svc.get("u1", id).await);
}

trait OkCode {
    fn ok(self) -> Result<DeviceStatus, ()>;
}
impl OkCode for Result<aionui_api_types::devices::Device, aionui_devices::DevicesError> {
    fn ok(self) -> Result<DeviceStatus, ()> {
        self.map(|d| d.status).map_err(|_| ())
    }
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
