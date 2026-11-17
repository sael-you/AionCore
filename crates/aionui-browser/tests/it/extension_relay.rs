//! C34 / TASK-125: the extension relay end to end with a FAKE extension (no
//! Chrome): pairing, hello, the Fetch guard, the synthesized Target layer an
//! agent sees, cookie refusal and stripping, the high-value pause, new-tab
//! gating, revoke, and the socket's Origin / first-frame checks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_api_types::{BrowserActivityOutcome, BrowserErrorCode, BrowserSettingsUpdate};
use aionui_browser::bridge::{ENV_ACTIVE_TOKEN, TOKEN_HEADER};
use aionui_browser::device_bridge::ENV_DEVICES;
use aionui_browser::routes::extension_origin;
use aionui_browser::{
    BrowserEventSink, BrowserLayout, BrowserRouterState, BrowserService, BrowserServiceConfig, NavigationGuard,
    browser_extension_routes,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Events(Mutex<Vec<(String, String, Value)>>);

impl BrowserEventSink for Events {
    fn emit(&self, user_id: &str, name: &str, data: Value) {
        self.0.lock().unwrap().push((user_id.into(), name.into(), data));
    }
}

impl Events {
    fn named(&self, name: &str) -> Vec<(String, Value)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, n, _)| n == name)
            .map(|(u, _, d)| (u.clone(), d.clone()))
            .collect()
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    svc: Arc<BrowserService>,
    events: Arc<Events>,
    port: u16,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let svc = BrowserService::new(BrowserServiceConfig {
        layout: BrowserLayout::new(dir.path().join("b"), dir.path().join("d")),
        host: None,
        idle_timeout: aionui_browser::DEFAULT_IDLE_TIMEOUT,
        guard: NavigationGuard::new(["aionui.test".to_owned()], Vec::<String>::new()),
        chrome_override: Some(dir.path().join("no-chrome")),
        run_as_override: None,
    })
    .unwrap();
    let events = Arc::new(Events::default());
    svc.set_event_sink(events.clone());
    svc.update_settings(
        "u1",
        &BrowserSettingsUpdate {
            devices_agents_enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let app = browser_extension_routes(BrowserRouterState { service: svc.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Harness {
        _dir: dir,
        svc,
        events,
        port,
    }
}

async fn open_extension(port: u16, origin: Option<&str>) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://127.0.0.1:{port}/api/browser/extension")
        .into_client_request()
        .unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
}

async fn next_json(ws: &mut Ws) -> Option<Value> {
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

/// A fake extension: answers commands like `chrome.debugger` would and
/// reports every frame it gets.
struct FakeExtension {
    tx: mpsc::UnboundedSender<Value>,
    seen: mpsc::UnboundedReceiver<Value>,
    device_id: String,
}

impl FakeExtension {
    /// Pair with a fresh code and serve the socket.
    async fn pair(h: &Harness) -> (Self, String) {
        let code = h.svc.device_hub().pairing_code("u1").unwrap().code;
        let mut ws = open_extension(h.port, Some(&extension_origin())).await.unwrap();
        send(
            &mut ws,
            json!({"type": "pair", "code": code, "device_name": "Mac", "browser": "Chrome 157"}),
        )
        .await;
        let welcome = next_json(&mut ws).await.unwrap();
        assert_eq!(welcome["type"], "welcome");
        let token = welcome["token"].as_str().unwrap().to_owned();
        let device_id = welcome["device_id"].as_str().unwrap().to_owned();
        (Self::serve(ws, device_id), token)
    }

    fn serve(mut ws: Ws, device_id: String) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let (seen_tx, seen) = mpsc::unbounded_channel::<Value>();
        let next_tab = Arc::new(Mutex::new(100i64));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = rx.recv() => {
                        let Some(v) = out else { break };
                        if ws.send(Message::Text(v.to_string().into())).await.is_err() { break }
                    }
                    msg = ws.next() => {
                        let Some(Ok(Message::Text(t))) = msg else { let _ = seen_tx.send(json!({"type": "closed"})); break };
                        let frame: Value = serde_json::from_str(&t).unwrap();
                        let reply = match frame["type"].as_str().unwrap_or_default() {
                            "command" => {
                                let method = frame["method"].as_str().unwrap_or_default();
                                let tab = frame["tab_id"].as_i64().unwrap_or_default();
                                Some(match method {
                                    // Tab 13 cannot be debugged (e.g. a chrome:// page).
                                    "Fetch.enable" if tab == 13 => json!({"type": "result", "id": frame["id"], "error": {"message": "Cannot access a chrome:// URL"}}),
                                    // Tab 7: an ordinary page with a Google sign-in iframe.
                                    "Page.getFrameTree" if tab == 7 => json!({"type": "result", "id": frame["id"], "result": {"frameTree": {
                                        "frame": {"id": "MAIN7", "url": "https://shop.example.com/checkout"},
                                        "childFrames": [{"frame": {"id": "F1", "parentId": "MAIN7", "url": "https://accounts.google.com/gsi/iframe"}}]
                                    }}}),
                                    "Page.getFrameTree" => json!({"type": "result", "id": frame["id"], "result": {"frameTree": {
                                        "frame": {"id": format!("MAIN{tab}"), "url": "about:blank"}
                                    }}}),
                                    "Target.getTargetInfo" => json!({"type": "result", "id": frame["id"], "result": {"targetInfo": {"targetId": format!("MAIN{tab}")}}}),
                                    "Runtime.evaluate" => json!({"type": "result", "id": frame["id"], "result": {"result": {"type": "number", "value": 2}}}),
                                    _ => json!({"type": "result", "id": frame["id"], "result": {}}),
                                })
                            }
                            "create_tab" => {
                                let mut n = next_tab.lock().unwrap();
                                *n += 1;
                                Some(json!({"type": "result", "id": frame["id"], "result": {"tab_id": *n}}))
                            }
                            "close_tab" => Some(json!({"type": "result", "id": frame["id"], "result": {}})),
                            _ => None,
                        };
                        let _ = seen_tx.send(frame);
                        if let Some(r) = reply
                            && ws.send(Message::Text(r.to_string().into())).await.is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        Self { tx, seen, device_id }
    }

    fn send(&self, v: Value) {
        self.tx.send(v).unwrap();
    }

    fn state(&self, new_tabs: bool, tabs: &[(i64, &str)]) {
        let tabs: Vec<Value> = tabs
            .iter()
            .map(|(id, url)| json!({"tab_id": id, "url": url, "title": format!("T{id}")}))
            .collect();
        self.send(json!({"type": "state", "new_tabs": new_tabs, "tabs": tabs}));
    }

    /// The next frame AionCore sent that matches.
    async fn expect(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        loop {
            let v = tokio::time::timeout(WAIT, self.seen.recv())
                .await
                .expect("the extension got nothing in time")
                .expect("fake extension stopped");
            if pred(&v) {
                return v;
            }
        }
    }

    /// No matching frame within `ms`.
    async fn none_within(&mut self, ms: u64, pred: impl Fn(&Value) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(v)) = tokio::time::timeout_at(deadline, self.seen.recv()).await {
            assert!(!pred(&v), "unexpected frame to the extension: {v}");
        }
    }
}

fn is_command(method: &'static str) -> impl Fn(&Value) -> bool {
    move |v| v["type"] == "command" && v["method"] == method
}

/// The agent's view: env → device port → CDP WebSocket with the header.
struct Agent {
    ws: Ws,
    next_id: u64,
    backlog: Vec<Value>,
}

async fn agent_env(svc: &Arc<BrowserService>) -> HashMap<String, String> {
    svc.agent_env("u1").await.into_iter().collect()
}

impl Agent {
    async fn connect(svc: &Arc<BrowserService>) -> Self {
        let env = agent_env(svc).await;
        let devices: Value = serde_json::from_str(&env[ENV_DEVICES]).unwrap();
        assert_eq!(devices[0]["name"], "Mac · Chrome");
        let port = devices[0]["port"].as_u64().unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/devtools/browser/aionui")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert(TOKEN_HEADER, env[ENV_ACTIVE_TOKEN].parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        Self {
            ws,
            next_id: 1,
            backlog: Vec::new(),
        }
    }

    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut frame = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            frame["sessionId"] = json!(s);
        }
        send(&mut self.ws, frame).await;
        loop {
            let v = next_json(&mut self.ws).await.expect("agent socket closed");
            if v["id"] == id {
                return v;
            }
            self.backlog.push(v);
        }
    }

    async fn event(&mut self, method: &str) -> Value {
        if let Some(i) = self.backlog.iter().position(|v| v["method"] == method) {
            return self.backlog.remove(i);
        }
        loop {
            let v = next_json(&mut self.ws).await.expect("agent socket closed");
            if v["method"] == method {
                return v;
            }
            self.backlog.push(v);
        }
    }
}

fn error_message(v: &Value) -> &str {
    v["error"]["message"].as_str().unwrap_or_default()
}

async fn wait_tabs(svc: &Arc<BrowserService>, n: u16) {
    for _ in 0..200 {
        if svc
            .device_hub()
            .devices("u1")
            .first()
            .is_some_and(|d| d.shared_tabs == n)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {n} visible tabs, have {:?}", svc.device_hub().devices("u1"));
}

#[tokio::test]
async fn the_socket_needs_the_pinned_origin_and_a_valid_first_frame() {
    let h = harness().await;
    // Wrong or missing Origin: refused at the upgrade.
    assert!(open_extension(h.port, Some("https://evil.example")).await.is_err());
    assert!(open_extension(h.port, None).await.is_err());
    // Unknown token / bad code / garbage: one error frame, then close.
    for (first, code) in [
        (
            json!({"type": "hello", "token": "nope", "browser": "Chrome", "version": 1}),
            "BROWSER_DEVICE_REVOKED",
        ),
        (
            json!({"type": "pair", "code": "WRONG-CODE", "device_name": "Mac", "browser": "Chrome"}),
            "BROWSER_PAIRING_INVALID",
        ),
        (
            json!({"type": "state", "new_tabs": true, "tabs": []}),
            "BROWSER_BAD_MESSAGE",
        ),
    ] {
        let mut ws = open_extension(h.port, Some(&extension_origin())).await.unwrap();
        send(&mut ws, first).await;
        assert_eq!(
            next_json(&mut ws).await.unwrap(),
            json!({"type": "error", "code": code})
        );
        assert!(next_json(&mut ws).await.is_none(), "closed after the error");
    }
    // Silence: closed after the 5 s first-frame timeout.
    let mut ws = open_extension(h.port, Some(&extension_origin())).await.unwrap();
    let started = std::time::Instant::now();
    let first = tokio::time::timeout(Duration::from_secs(8), ws.next()).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(4900));
    let got = match first {
        Some(Ok(Message::Text(t))) => serde_json::from_str::<Value>(&t).unwrap(),
        other => panic!("expected an error frame, got {other:?}"),
    };
    assert_eq!(got["code"], "BROWSER_BAD_MESSAGE");
    assert!(h.svc.device_hub().devices("u1").is_empty(), "nothing was paired");
}

#[tokio::test]
async fn pair_then_hello_and_revoke_ends_everything() {
    let h = harness().await;
    let (mut ext, token) = FakeExtension::pair(&h).await;
    let devices = h.svc.device_hub().devices("u1");
    assert_eq!(devices.len(), 1);
    assert!(devices[0].online);
    assert!(
        devices[0].extension_version.is_none(),
        "an extension that sends none has none"
    );
    let listed = serde_json::to_string(&devices).unwrap();
    assert!(!listed.contains(&token), "the token never appears in the device list");
    assert!(!h.events.named("browser.devicesChanged").is_empty());
    assert!(h.events.0.lock().unwrap().iter().all(|(u, _, _)| u == "u1"));

    // A second connection with the token replaces the first.
    let mut ws = open_extension(h.port, Some(&extension_origin())).await.unwrap();
    send(
        &mut ws,
        json!({"type": "hello", "token": token, "browser": "Chrome 157", "version": 1, "extension_version": "0.1.2"}),
    )
    .await;
    let welcome = next_json(&mut ws).await.unwrap();
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["device_id"], ext.device_id.as_str());
    assert!(welcome.get("token").is_none(), "the token is only sent once, on pair");
    // TASK-133: the extension's own version is stored and listed (display only).
    assert_eq!(
        h.svc.device_hub().devices("u1")[0].extension_version.as_deref(),
        Some("0.1.2")
    );
    ext.expect(|v| v["type"] == "closed").await;
    send(&mut ws, json!({"type": "ping"})).await;
    assert_eq!(next_json(&mut ws).await.unwrap(), json!({"type": "pong"}));

    // Another user cannot revoke it; the owner can.
    assert_eq!(
        h.svc.revoke_device("u2", &ext.device_id).unwrap_err().code(),
        Some(BrowserErrorCode::BrowserDeviceNotFound)
    );
    h.svc.revoke_device("u1", &ext.device_id).unwrap();
    assert_eq!(
        next_json(&mut ws).await.unwrap(),
        json!({"type": "error", "code": "BROWSER_DEVICE_REVOKED"})
    );
    assert!(next_json(&mut ws).await.is_none());
    let mut again = open_extension(h.port, Some(&extension_origin())).await.unwrap();
    send(
        &mut again,
        json!({"type": "hello", "token": token, "browser": "Chrome", "version": 1}),
    )
    .await;
    assert_eq!(next_json(&mut again).await.unwrap()["code"], "BROWSER_DEVICE_REVOKED");
    assert!(h.svc.device_hub().devices("u1").is_empty());
    assert!(!agent_env(&h.svc).await.contains_key(ENV_DEVICES));
}

#[tokio::test]
async fn agents_see_only_guarded_allowed_tabs_and_drive_them_through_the_filter() {
    let h = harness().await;
    let (mut ext, _token) = FakeExtension::pair(&h).await;
    ext.state(
        false,
        &[
            (1, "https://example.com/"),
            (12, "http://192.168.1.1/"),
            (13, "chrome://settings"),
        ],
    );
    // Every shared tab gets the Fetch guard first.
    for _ in 0..3 {
        let enable = ext.expect(is_command("Fetch.enable")).await;
        let patterns = enable["params"]["patterns"].as_array().unwrap();
        assert!(patterns.iter().any(|p| p["urlPattern"] == "*://*aionui.test*"));
        assert!(
            patterns
                .iter()
                .any(|p| p["urlPattern"] == "*://mail.google.com/*" && p["resourceType"] == "Document")
        );
    }
    // Tab 12 is on a private address, tab 13 refused the debugger: hidden.
    wait_tabs(&h.svc, 1).await;

    let mut agent = Agent::connect(&h.svc).await;
    let targets = agent.call("Target.getTargets", json!({}), None).await;
    let infos = targets["result"]["targetInfos"].as_array().unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0]["targetId"], "tab-1");
    assert_eq!(infos[0]["url"], "https://example.com/");

    let auto = agent
        .call(
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
            None,
        )
        .await;
    assert_eq!(auto["result"], json!({}));
    let attached = agent.event("Target.attachedToTarget").await;
    let sid = attached["params"]["sessionId"].as_str().unwrap().to_owned();
    assert_eq!(attached["params"]["targetInfo"]["targetId"], "tab-1");

    // A page command goes to the tab, in order, and the answer comes back.
    let r = agent
        .call("Runtime.evaluate", json!({"expression": "1+1"}), Some(&sid))
        .await;
    assert_eq!(r["result"]["result"]["value"], 2);
    assert_eq!(r["sessionId"], sid.as_str());
    let cmd = ext.expect(is_command("Runtime.evaluate")).await;
    assert_eq!(cmd["tab_id"], 1);

    // Session-scoped auto-attach is only acknowledged; unknown sessions fail.
    let nested = agent
        .call(
            "Target.setAutoAttach",
            json!({"autoAttach": true, "flatten": true}),
            Some(&sid),
        )
        .await;
    assert_eq!(nested["result"], json!({}));
    let bogus = agent
        .call("Runtime.evaluate", json!({"expression": "1"}), Some("FOREIGN"))
        .await;
    assert!(error_message(&bogus).contains("No session"));

    // Cookie APIs and Fetch are refused before the extension sees them.
    for method in ["Network.getAllCookies", "Storage.getCookies", "Fetch.continueRequest"] {
        let r = agent.call(method, json!({}), Some(&sid)).await;
        assert!(error_message(&r).starts_with("BROWSER_METHOD_BLOCKED"), "{method}: {r}");
    }
    ext.none_within(200, |v| {
        v["type"] == "command"
            && (v["method"].as_str().unwrap_or("").contains("Cookies") || v["method"] == "Fetch.continueRequest")
    })
    .await;

    // Events: forwarded with the session, cookies stripped, Fetch hidden.
    ext.send(json!({"type": "event", "tab_id": 1, "method": "Network.responseReceivedExtraInfo", "params": {
        "requestId": "r", "headers": {"set-cookie": "sid=secret", "x-ok": "1"}, "blockedCookies": [{"cookie": {"name": "sid"}}]
    }}));
    let ev = agent.event("Network.responseReceivedExtraInfo").await;
    assert_eq!(ev["sessionId"], sid.as_str());
    assert_eq!(ev["params"]["headers"], json!({"x-ok": "1"}));
    assert!(!ev.to_string().contains("secret"));

    // Fetch guard: a private address fails, a public one continues; neither
    // reaches the agent.
    ext.send(
        json!({"type": "event", "tab_id": 1, "method": "Fetch.requestPaused", "params": {
            "requestId": "p1", "request": {"url": "http://10.0.0.5/admin"}, "resourceType": "XHR", "frameId": "MAIN1"
        }}),
    );
    let fail = ext.expect(is_command("Fetch.failRequest")).await;
    assert_eq!(
        fail["params"],
        json!({"requestId": "p1", "errorReason": "BlockedByClient"})
    );
    ext.send(
        json!({"type": "event", "tab_id": 1, "method": "Fetch.requestPaused", "params": {
            "requestId": "p2", "request": {"url": "https://aionui.test/api"}, "resourceType": "XHR", "frameId": "MAIN1"
        }}),
    );
    assert_eq!(
        ext.expect(is_command("Fetch.failRequest")).await["params"]["requestId"],
        "p2",
        "the AionUi origin is blocked"
    );
    ext.send(json!({"type": "event", "tab_id": 1, "method": "Fetch.requestPaused", "params": {
        "requestId": "p3", "request": {"url": "https://cdn.example.com/a.js"}, "resourceType": "Script", "frameId": "MAIN1"
    }}));
    assert_eq!(
        ext.expect(is_command("Fetch.continueRequest")).await["params"]["requestId"],
        "p3"
    );
    let marker = agent.call("Target.getTargets", json!({}), None).await;
    assert!(marker["result"].is_object());
    assert!(
        agent
            .backlog
            .iter()
            .all(|v| !v["method"].as_str().unwrap_or("").starts_with("Fetch."))
    );

    // Activity: method names and origins only.
    let log = h.svc.device_hub().activity("u1", &ext.device_id, Some(500)).unwrap();
    assert!(log.iter().any(|a| a.method == "Runtime.evaluate"
        && a.outcome == BrowserActivityOutcome::Ok
        && a.origin == "https://example.com"));
    assert!(
        log.iter()
            .any(|a| a.method == "Network.getAllCookies" && a.outcome == BrowserActivityOutcome::Refused)
    );
    assert!(!serde_json::to_string(&log).unwrap().contains("1+1"), "never params");

    // Unsharing the tab detaches the agent's session.
    ext.state(false, &[]);
    let detached = agent.event("Target.detachedFromTarget").await;
    assert_eq!(detached["params"]["sessionId"], sid.as_str());
}

#[tokio::test]
async fn high_value_origins_pause_for_the_owner() {
    let h = harness().await;
    let (mut ext, _token) = FakeExtension::pair(&h).await;
    ext.state(false, &[(5, "https://mail.google.com/mail/u/0/")]);
    ext.expect(is_command("Fetch.enable")).await;
    wait_tabs(&h.svc, 1).await;
    let mut agent = Agent::connect(&h.svc).await;
    let attach = agent
        .call(
            "Target.attachToTarget",
            json!({"targetId": "tab-5", "flatten": true}),
            None,
        )
        .await;
    let sid = attach["result"]["sessionId"].as_str().unwrap().to_owned();

    // Read-only methods pass without asking (train-9 review allowlist).
    let shot = agent.call("Page.captureScreenshot", json!({}), Some(&sid)).await;
    assert!(shot.get("error").is_none(), "{shot}");
    ext.expect(is_command("Page.captureScreenshot")).await;
    assert!(h.svc.device_hub().approvals().list("u1").is_empty());

    // Default deny: script through compileScript waits, and a deny refuses it.
    let id = agent.next_id;
    agent.next_id += 1;
    send(
        &mut agent.ws,
        json!({"id": id, "method": "Runtime.compileScript", "params": {"expression": "1", "sourceURL": "", "persistScript": true}, "sessionId": sid}),
    )
    .await;
    let approval = loop {
        if let Some(a) = h.svc.device_hub().approvals().list("u1").into_iter().next() {
            break a;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(approval.method, "Runtime.compileScript");
    h.svc
        .device_hub()
        .approvals()
        .decide("u1", &approval.approval_id, false)
        .unwrap();
    let refused = loop {
        let v = next_json(&mut agent.ws).await.unwrap();
        if v["id"] == id {
            break v;
        }
    };
    assert!(error_message(&refused).starts_with("BROWSER_APPROVAL_DENIED"));
    ext.none_within(150, is_command("Runtime.compileScript")).await;

    // Typing waits for the owner: deny refuses that command only.
    let id = agent.next_id;
    send(
        &mut agent.ws,
        json!({"id": id, "method": "Input.insertText", "params": {"text": "hello"}, "sessionId": sid}),
    )
    .await;
    agent.next_id += 1;
    let approval = loop {
        if let Some(a) = h.svc.device_hub().approvals().list("u1").into_iter().next() {
            break a;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(approval.origin, "https://mail.google.com");
    assert_eq!(approval.device_name, "Mac · Chrome");
    assert_eq!(approval.summary, "Input.insertText: hello");
    assert!(h.svc.device_hub().approvals().list("u2").is_empty());
    ext.none_within(150, is_command("Input.insertText")).await;
    h.svc
        .device_hub()
        .approvals()
        .decide("u1", &approval.approval_id, false)
        .unwrap();
    let denied = loop {
        let v = next_json(&mut agent.ws).await.unwrap();
        if v["id"] == id {
            break v;
        }
    };
    assert!(error_message(&denied).starts_with("BROWSER_APPROVAL_DENIED"));
    ext.none_within(150, is_command("Input.insertText")).await;

    // Approve: this command and the burst after it (same tab + origin) pass.
    let id = agent.next_id;
    send(
        &mut agent.ws,
        json!({"id": id, "method": "Runtime.evaluate", "params": {"expression": "document.title"}, "sessionId": sid}),
    )
    .await;
    agent.next_id += 1;
    let approval = loop {
        if let Some(a) = h.svc.device_hub().approvals().list("u1").into_iter().next() {
            break a;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    h.svc
        .device_hub()
        .approvals()
        .decide("u1", &approval.approval_id, true)
        .unwrap();
    ext.expect(is_command("Runtime.evaluate")).await;
    let typed = agent.call("Input.insertText", json!({"text": "x"}), Some(&sid)).await;
    assert!(typed.get("error").is_none(), "{typed}");
    ext.expect(is_command("Input.insertText")).await;

    let requested = h.events.named("browser.approvalRequested");
    let resolved = h.events.named("browser.approvalResolved");
    // compileScript (denied), insertText (denied), evaluate (approved).
    assert_eq!(requested.len(), 3);
    assert_eq!(resolved.len(), 3);
    assert!(requested.iter().chain(resolved.iter()).all(|(u, _)| u == "u1"));
    let outcomes: Vec<_> = h
        .svc
        .device_hub()
        .activity("u1", &ext.device_id, Some(500))
        .unwrap()
        .into_iter()
        .map(|a| a.outcome)
        .collect();
    for o in [
        BrowserActivityOutcome::Paused,
        BrowserActivityOutcome::Denied,
        BrowserActivityOutcome::Approved,
    ] {
        assert!(outcomes.contains(&o), "{o:?} in {outcomes:?}");
    }
}

#[tokio::test]
async fn new_tabs_need_the_extension_switch_and_the_guard() {
    let h = harness().await;
    let (mut ext, _token) = FakeExtension::pair(&h).await;
    ext.state(false, &[(1, "https://example.com/")]);
    ext.expect(is_command("Fetch.enable")).await;
    wait_tabs(&h.svc, 1).await;
    let mut agent = Agent::connect(&h.svc).await;
    agent
        .call(
            "Target.setAutoAttach",
            json!({"autoAttach": true, "flatten": true}),
            None,
        )
        .await;
    agent.event("Target.attachedToTarget").await;

    let off = agent
        .call("Target.createTarget", json!({"url": "https://example.org/"}), None)
        .await;
    assert!(error_message(&off).starts_with("BROWSER_METHOD_BLOCKED"), "{off}");
    let close = agent
        .call("Target.closeTarget", json!({"targetId": "tab-1"}), None)
        .await;
    assert!(error_message(&close).starts_with("BROWSER_METHOD_BLOCKED"));
    ext.none_within(150, |v| v["type"] == "create_tab" || v["type"] == "close_tab")
        .await;

    ext.state(true, &[(1, "https://example.com/")]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let blocked = agent
        .call("Target.createTarget", json!({"url": "http://127.0.0.1:8080/"}), None)
        .await;
    assert!(error_message(&blocked).starts_with("BROWSER_URL_BLOCKED"));

    // The extension opens and shares it; the agent gets the target once guarded.
    let id = agent.next_id;
    agent.next_id += 1;
    send(
        &mut agent.ws,
        json!({"id": id, "method": "Target.createTarget", "params": {"url": "https://example.org/"}}),
    )
    .await;
    let create = ext.expect(|v| v["type"] == "create_tab").await;
    assert_eq!(create["url"], "https://example.org/");
    ext.state(true, &[(1, "https://example.com/"), (101, "https://example.org/")]);
    assert_eq!(ext.expect(is_command("Fetch.enable")).await["tab_id"], 101);
    let reply = loop {
        let v = next_json(&mut agent.ws).await.unwrap();
        if v["id"] == id {
            break v;
        }
        agent.backlog.push(v);
    };
    assert_eq!(reply["result"]["targetId"], "tab-101");
    let attached = agent.event("Target.attachedToTarget").await;
    assert_eq!(attached["params"]["targetInfo"]["targetId"], "tab-101");

    let closed = agent
        .call("Target.closeTarget", json!({"targetId": "tab-101"}), None)
        .await;
    assert_eq!(closed["result"]["success"], true);
    assert_eq!(ext.expect(|v| v["type"] == "close_tab").await["tab_id"], 101);
}

#[tokio::test]
async fn the_switches_and_the_token_gate_the_device_bridge() {
    let h = harness().await;
    let (ext, _token) = FakeExtension::pair(&h).await;
    let env = agent_env(&h.svc).await;
    let devices: Value = serde_json::from_str(&env[ENV_DEVICES]).unwrap();
    assert_eq!(devices[0]["device_id"], ext.device_id.as_str());
    let port = devices[0]["port"].as_u64().unwrap();
    // Next to the named server profiles, under the same name (TASK-124 + C34).
    let instances: Value = serde_json::from_str(&env[aionui_browser::bridge::ENV_INSTANCES]).unwrap();
    let names: Vec<&str> = instances
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(names, ["Main", "Instance 2", "Instance 3", "Mac · Chrome"]);
    let mac = &instances[3];
    assert_eq!(mac["kind"], "device");
    assert_eq!(mac["port"].as_u64(), Some(port));
    assert!(mac.get("instance").is_none(), "no server profile number");
    let http = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/json/version");
    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        http.get(&url)
            .header(TOKEN_HEADER, "nope")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let ok = http
        .get(&url)
        .header(TOKEN_HEADER, &env[ENV_ACTIVE_TOKEN])
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    // "Agents may use my Mac browser" off: no env entry, and the port refuses.
    h.svc
        .update_settings(
            "u1",
            &BrowserSettingsUpdate {
                devices_agents_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let env_off = agent_env(&h.svc).await;
    assert!(!env_off.contains_key(ENV_DEVICES));
    assert!(!env_off[aionui_browser::bridge::ENV_INSTANCES].contains("Mac"));
    let off = http
        .get(&url)
        .header(TOKEN_HEADER, &env[ENV_ACTIVE_TOKEN])
        .send()
        .await
        .unwrap();
    assert_eq!(off.status(), 403);
    let body: Value = off.json().await.unwrap();
    assert_eq!(body["code"], "BROWSER_AGENTS_DISABLED");

    // Offline device: unavailable, no queueing.
    h.svc
        .update_settings(
            "u1",
            &BrowserSettingsUpdate {
                devices_agents_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    drop(ext);
    for _ in 0..200 {
        if !h.svc.device_hub().devices("u1")[0].online {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let offline = http
        .get(&url)
        .header(TOKEN_HEADER, &env[ENV_ACTIVE_TOKEN])
        .send()
        .await
        .unwrap();
    assert_eq!(offline.status(), 503);
}

/// Train-9 review: a high-value IFRAME makes the whole tab high-value.
#[tokio::test]
async fn a_high_value_iframe_makes_the_tab_high_value() {
    let h = harness().await;
    let (mut ext, _token) = FakeExtension::pair(&h).await;
    ext.state(
        false,
        &[(7, "https://shop.example.com/checkout"), (8, "https://example.com/")],
    );
    for _ in 0..2 {
        ext.expect(is_command("Page.getFrameTree")).await;
    }
    wait_tabs(&h.svc, 2).await;
    let mut agent = Agent::connect(&h.svc).await;
    let s7 = agent
        .call(
            "Target.attachToTarget",
            json!({"targetId": "tab-7", "flatten": true}),
            None,
        )
        .await["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let s8 = agent
        .call(
            "Target.attachToTarget",
            json!({"targetId": "tab-8", "flatten": true}),
            None,
        )
        .await["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();

    // The ordinary tab runs script without asking.
    let ok = agent
        .call("Runtime.evaluate", json!({"expression": "1+1"}), Some(&s8))
        .await;
    assert_eq!(ok["result"]["result"]["value"], 2);

    // The tab with the Google iframe waits for the owner.
    let id = agent.next_id;
    agent.next_id += 1;
    send(
        &mut agent.ws,
        json!({"id": id, "method": "Runtime.evaluate", "params": {"expression": "document.title"}, "sessionId": s7}),
    )
    .await;
    let approval = loop {
        if let Some(a) = h.svc.device_hub().approvals().list("u1").into_iter().next() {
            break a;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(approval.origin, "https://accounts.google.com");
    assert_eq!(approval.tab_id, 7);
    h.svc
        .device_hub()
        .approvals()
        .decide("u1", &approval.approval_id, false)
        .unwrap();
    let refused = loop {
        let v = next_json(&mut agent.ws).await.unwrap();
        if v["id"] == id {
            break v;
        }
    };
    assert!(error_message(&refused).starts_with("BROWSER_APPROVAL_DENIED"));

    // A high-value document paused in a subframe of tab 8 marks it too.
    ext.send(json!({"type": "event", "tab_id": 8, "method": "Fetch.requestPaused", "params": {
        "requestId": "d1", "request": {"url": "https://www.paypal.com/checkoutnow"}, "resourceType": "Document", "frameId": "PP"
    }}));
    assert_eq!(
        ext.expect(is_command("Fetch.continueRequest")).await["params"]["requestId"],
        "d1"
    );
    let id = agent.next_id;
    agent.next_id += 1;
    send(
        &mut agent.ws,
        json!({"id": id, "method": "Input.dispatchMouseEvent", "params": {"type": "mousePressed", "x": 1, "y": 1}, "sessionId": s8}),
    )
    .await;
    let approval = loop {
        if let Some(a) = h.svc.device_hub().approvals().list("u1").into_iter().next() {
            break a;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(approval.origin, "https://www.paypal.com");
    assert_eq!(approval.tab_id, 8);
}
