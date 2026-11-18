//! Live end-to-end checks against a real Chrome under Xvfb, run as the real
//! `aionui-browser` account (C30, TASK-111). Opt-in: `AIONUI_BROWSER_LIVE=1`
//! (needs Chrome at `AIONUI_BROWSER_CHROME` or /opt/google/chrome/chrome,
//! Xvfb, network, and root to switch uid). Optional:
//! `AIONUI_BROWSER_LIVE_PLAYWRIGHT=1` also runs playwright-cli through the
//! bridge and checks `ps` for the token.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aionui_api_types::{BrowserInstanceUpdate, BrowserSettingsUpdate};
use aionui_browser::bridge::{ENV_ACTIVE_PORT, ENV_ACTIVE_TOKEN, ENV_PLAYWRIGHT_HEADERS};
use aionui_browser::{BrowserLayout, BrowserService, BrowserServiceConfig, NavigationGuard};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn live() -> bool {
    std::env::var("AIONUI_BROWSER_LIVE").is_ok()
}

/// A plain HTTP server on 127.0.0.1 that counts requests (a blocked target).
fn counting_server() -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            h.fetch_add(1, Ordering::SeqCst);
            let mut s = stream;
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        }
    });
    (port, hits)
}

/// Raw CDP over the bridge, authenticated by the header.
struct Agent {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next: u64,
}

impl Agent {
    async fn connect(port: &str, token: &str) -> Self {
        let mut req = format!("ws://127.0.0.1:{port}/devtools/browser/aionui")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("X-AionUi-Bridge-Token", token.parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("bridge accepts the header");
        Self { ws, next: 1 }
    }

    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Value {
        let id = self.next;
        self.next += 1;
        let mut frame = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            frame["sessionId"] = json!(s);
        }
        self.ws.send(Message::Text(frame.to_string().into())).await.unwrap();
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(30), self.ws.next())
                .await
                .expect("reply in time")
                .unwrap()
                .unwrap();
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["id"] == json!(id) {
                    return v;
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_server_browser_end_to_end() {
    if !live() {
        return;
    }
    let chrome = std::env::var_os("AIONUI_BROWSER_CHROME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/google/chrome/chrome"));
    let t = tempfile::Builder::new()
        .prefix("c30-live")
        .tempdir_in("/var/tmp")
        .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    }
    let root = t.path().to_path_buf();
    let chrome_bin = chrome.clone();
    let make_svc = move || {
        BrowserService::new(BrowserServiceConfig {
            layout: BrowserLayout::new(root.join("aionui-browser"), root.join("data")),
            host: None,
            idle_timeout: Duration::from_secs(900),
            guard: NavigationGuard::from_origins(["https://y.talkseeds.com"]),
            chrome_override: Some(chrome_bin.clone()),
            run_as_override: None,
        })
        .unwrap()
    };
    let svc = make_svc();
    let user = "live_user";
    svc.update_settings(
        user,
        &BrowserSettingsUpdate {
            max_tabs: Some(3),
            instances: Some(vec![BrowserInstanceUpdate {
                instance: 2,
                hide_automation: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // 1. Start as the browser account; status reports it, never a port.
    let t0 = std::time::Instant::now();
    let inst = svc.start(user, 1).await.expect("instance 1 starts");
    eprintln!("start ms = {}", t0.elapsed().as_millis());
    let status = svc.status(user).unwrap();
    assert!(status.instances[0].running);
    let ps = std::process::Command::new("ps")
        .args(["-eo", "user:20,args"])
        .output()
        .unwrap();
    let ps = String::from_utf8_lossy(&ps.stdout);
    assert!(
        ps.lines()
            .any(|l| l.starts_with("aionui-browser") && l.contains("--no-sandbox")),
        "Chrome runs as aionui-browser"
    );

    // 2. Bridge: header required; env carries everything; agent can drive.
    let env: HashMap<_, _> = svc.agent_env(user).await.into_iter().collect();
    let (port, token) = (env[ENV_ACTIVE_PORT].clone(), env[ENV_ACTIVE_TOKEN].clone());
    assert_eq!(env[ENV_PLAYWRIGHT_HEADERS], format!("X-AionUi-Bridge-Token: {token}"));
    let http = reqwest::Client::new();
    assert_eq!(
        http.get(format!("http://127.0.0.1:{port}/json/version"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let v: Value = http
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .header("X-AionUi-Bridge-Token", &token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !v["webSocketDebuggerUrl"].as_str().unwrap().contains(&token),
        "no token in URLs"
    );
    let mut agent = Agent::connect(&port, &token).await;

    // 3. Guard before the first request: a new target straight at a blocked
    //    local server never reaches it; a public page loads.
    let (blocked_port, hits) = counting_server();
    let created = agent
        .call(
            "Target.createTarget",
            json!({"url": format!("http://127.0.0.1:{blocked_port}/first")}),
            None,
        )
        .await;
    assert!(created["result"]["targetId"].is_string(), "{created}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "a new target's first navigation is guarded"
    );
    let target = created["result"]["targetId"].as_str().unwrap().to_owned();
    let attached = agent
        .call(
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
            None,
        )
        .await;
    let session = attached["result"]["sessionId"].as_str().unwrap().to_owned();
    // Page JS fetch to the blocked server: refused as well.
    let _ = agent
        .call(
            "Runtime.evaluate",
            json!({"expression": format!("fetch('http://127.0.0.1:{blocked_port}/js').then(()=>1,()=>0)"), "awaitPromise": true}),
            Some(&session),
        )
        .await;
    assert_eq!(hits.load(Ordering::SeqCst), 0, "page JS cannot reach internal IPs");
    let nav = agent
        .call("Page.navigate", json!({"url": "https://example.com/"}), Some(&session))
        .await;
    assert!(nav["result"]["frameId"].is_string(), "{nav}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let title = agent
        .call(
            "Runtime.evaluate",
            json!({"expression": "document.title"}),
            Some(&session),
        )
        .await;
    assert!(
        title["result"]["result"]["value"]
            .as_str()
            .unwrap_or("")
            .contains("Example"),
        "{title}"
    );
    let aionui = agent
        .call(
            "Page.navigate",
            json!({"url": "https://y.talkseeds.com/"}),
            Some(&session),
        )
        .await;
    assert!(
        aionui["result"]["errorText"].as_str().unwrap_or("").contains("BLOCKED"),
        "AionUi origin blocked: {aionui}"
    );

    // 3b. Network-level egress guard (train-7 review): names that RESOLVE to
    //     loopback, WebSockets and the AionUi ports are refused by the proxy.
    let nav = agent
        .call("Page.navigate", json!({"url": "http://example.com/"}), Some(&session))
        .await;
    assert!(nav["result"]["frameId"].is_string(), "{nav}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let targets = [
        "http://localhost:25809/api/system/info".to_owned(),
        format!("http://127.0.0.1:{blocked_port}/aioncore-port"),
        format!("http://localtest.me:{blocked_port}/rebind"),
    ];
    let fetches = agent
        .call(
            "Runtime.evaluate",
            json!({
                "expression": format!(
                    "Promise.all({}.map(u => fetch(u, {{method: 'POST', mode: 'no-cors', body: 'x'}}).then(() => 'ok', () => 'refused')))",
                    serde_json::to_string(&targets).unwrap()
                ),
                "awaitPromise": true,
                "returnByValue": true
            }),
            Some(&session),
        )
        .await;
    eprintln!("page fetch results = {}", fetches["result"]["result"]["value"]);
    let sockets = [
        "ws://localhost:25809/api/browser/ws".to_owned(),
        format!("ws://127.0.0.1:{blocked_port}/ws"),
        format!("ws://localtest.me:{blocked_port}/ws"),
    ];
    let ws = agent
        .call(
            "Runtime.evaluate",
            json!({
                "expression": format!(
                    "Promise.all({}.map(u => new Promise(r => {{ const w = new WebSocket(u); w.onopen = () => r('open'); w.onerror = () => r('refused'); setTimeout(() => r('timeout'), 6000); }})))",
                    serde_json::to_string(&sockets).unwrap()
                ),
                "awaitPromise": true,
                "returnByValue": true
            }),
            Some(&session),
        )
        .await;
    let ws_values = ws["result"]["result"]["value"].clone();
    eprintln!("page websocket results = {ws_values}");
    assert!(
        ws_values.as_array().is_some_and(|v| v.iter().all(|x| x != "open")),
        "no WebSocket to an internal address opens: {ws}"
    );
    // Top-level navigation to a public NAME that resolves to loopback: only
    // the proxy can catch it (the URL guard sees a public name).
    let _ = agent
        .call(
            "Page.navigate",
            json!({"url": format!("http://localtest.me:{blocked_port}/top")}),
            Some(&session),
        )
        .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "fetch, WebSocket and navigation to loopback names never reach the server"
    );
    // Public sites still work through the proxy: Discord's login page renders.
    let nav = agent
        .call(
            "Page.navigate",
            json!({"url": "https://discord.com/login"}),
            Some(&session),
        )
        .await;
    assert!(nav["result"]["frameId"].is_string(), "{nav}");
    let mut discord = Value::Null;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        discord = agent
            .call(
                "Runtime.evaluate",
                json!({"expression": "!!document.querySelector('input[name=email]')", "returnByValue": true}),
                Some(&session),
            )
            .await;
        if discord["result"]["result"]["value"] == json!(true) {
            break;
        }
    }
    assert_eq!(
        discord["result"]["result"]["value"],
        json!(true),
        "Discord login renders: {discord}"
    );

    // 4. Cookie APIs refused, page JS allowed (owner change).
    let cookies = agent.call("Network.getAllCookies", json!({}), None).await;
    assert!(
        cookies["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("BROWSER_METHOD_BLOCKED")
    );
    let close = agent.call("Browser.close", json!({}), None).await;
    assert!(
        close["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("BROWSER_METHOD_BLOCKED")
    );

    // 5. Tab limit (max 3: the initial tab + the created one + one more).
    let third = agent
        .call("Target.createTarget", json!({"url": "about:blank"}), None)
        .await;
    assert!(third["result"]["targetId"].is_string(), "{third}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let fourth = agent
        .call("Target.createTarget", json!({"url": "about:blank"}), None)
        .await;
    assert!(
        fourth["error"]["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("BROWSER_TAB_LIMIT"),
        "{fourth}"
    );
    assert!(inst.agent_active());

    // 6. Optional: playwright-cli through the bridge, token never in argv.
    if std::env::var("AIONUI_BROWSER_LIVE_PLAYWRIGHT").is_ok() {
        let mut cmd = std::process::Command::new("playwright-cli");
        cmd.args(["attach", "--cdp", &format!("http://127.0.0.1:{port}")]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "attach: {}", String::from_utf8_lossy(&out.stderr));
        let ps = std::process::Command::new("ps").args(["-eo", "args"]).output().unwrap();
        let ps = String::from_utf8_lossy(&ps.stdout).into_owned();
        assert!(ps.contains("--cdp=http://127.0.0.1:"), "the daemon is attached");
        assert!(!ps.contains(&token), "the bridge token is in no process's argv");
        let mut detach = std::process::Command::new("playwright-cli");
        detach.arg("detach");
        for (k, v) in &env {
            detach.env(k, v);
        }
        let _ = detach.output();
    }

    // 7. Agents off → the open connection and new ones are refused.
    svc.update_settings(
        user,
        &BrowserSettingsUpdate {
            agents_enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let off = agent.call("Target.getTargets", json!({}), None).await;
    assert!(
        off["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("BROWSER_AGENTS_DISABLED")
    );
    assert!(svc.agent_env(user).await.is_empty());
    svc.update_settings(
        user,
        &BrowserSettingsUpdate {
            agents_enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // 8. hide_automation on instance 2 only.
    let i2 = svc.start(user, 2).await.expect("instance 2 starts");
    assert!(i2.hide_automation && !inst.hide_automation);
    let webdriver = |i: Arc<aionui_browser::instance::ChromeInstance>| async move {
        let tabs = i.tabs().borrow().clone();
        let s = i
            .cdp()
            .call(
                "Target.attachToTarget",
                json!({"targetId": tabs[0].id, "flatten": true}),
                None,
            )
            .await
            .unwrap();
        let sid = s["sessionId"].as_str().unwrap().to_owned();
        i.cdp()
            .call(
                "Runtime.evaluate",
                json!({"expression": "navigator.webdriver"}),
                Some(&sid),
            )
            .await
            .unwrap()["result"]["value"]
            .clone()
    };
    assert_eq!(webdriver(inst.clone()).await, json!(true));
    assert_eq!(webdriver(i2.clone()).await, json!(false));

    // 8b. No debugging TCP port (train-8 review): nothing owned by the browser
    //     account listens on any TCP socket; CDP runs over the pipe only.
    let uid_out = std::process::Command::new("id")
        .args(["-u", "aionui-browser"])
        .output()
        .unwrap();
    let browser_uid = String::from_utf8_lossy(&uid_out.stdout).trim().to_owned();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let text = std::fs::read_to_string(table).unwrap_or_default();
        let listening: Vec<&str> = text
            .lines()
            .skip(1)
            .filter(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() > 7 && f[3] == "0A" && f[7] == browser_uid
            })
            .collect();
        assert!(
            listening.is_empty(),
            "{table}: browser account listens on {listening:?}"
        );
    }
    // Process groups of both instances (evidence for the reaper), then a reap
    // with both instances live must touch neither.
    let ps = std::process::Command::new("ps")
        .args(["-u", "aionui-browser", "-o", "pid=,ppid=,pgid=,comm="])
        .output()
        .unwrap();
    eprintln!(
        "instance groups: 1 = {:?}, 2 = {:?}\naccount processes (pid ppid pgid comm):\n{}",
        inst.process_groups(),
        i2.process_groups(),
        String::from_utf8_lossy(&ps.stdout)
    );
    let crashpad = std::process::Command::new("pgrep")
        .args(["-u", "aionui-browser", "chrome_crashpad"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&crashpad.stdout).trim().is_empty(),
        "no crash handler (no crash uploads) runs for the browser account"
    );
    assert_eq!(svc.reap_orphans().await, 0, "live instances are never reaped");
    for i in [&inst, &i2] {
        assert!(i.is_running());
        i.cdp()
            .call("Browser.getVersion", json!({}), None)
            .await
            .expect("still answers over its pipe");
    }

    // 9. Idle footprint, then stop over CDP (root cannot signal the account).
    tokio::time::sleep(Duration::from_secs(5)).await;
    let rss = std::process::Command::new("ps")
        .args(["-u", "aionui-browser", "-o", "rss=,pcpu="])
        .output()
        .unwrap();
    let (mut kb, mut cpu) = (0u64, 0f64);
    for line in String::from_utf8_lossy(&rss.stdout).lines() {
        let mut f = line.split_whitespace();
        kb += f.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        cpu += f.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    }
    eprintln!("idle (2 instances) RSS sum MB = {}, CPU % sum = {cpu:.1}", kb / 1024);
    // 10. Restart cleanup: a new service (a restarted AionCore) finds the
    //     previous Chrome processes (root cannot signal them), stops them, and
    //     a fresh launch on the same profile works (locks removed).
    let before = std::process::Command::new("pgrep")
        .args(["-u", "aionui-browser"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&before.stdout).trim().is_empty(),
        "previous Chrome still running"
    );
    let svc2 = make_svc();
    let reaped = svc2.reap_orphans().await;
    assert!(reaped > 0, "orphans found");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let after = std::process::Command::new("pgrep")
        .args(["-u", "aionui-browser"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&after.stdout).trim().is_empty(),
        "every orphan stopped (reaped {reaped})"
    );
    // Symlink attack (train-8 final review): the account plants links in its
    // own profile; root must never follow them (writes/reads run as the account).
    let profile1 = t.path().join("aionui-browser").join(user).join("1");
    let secret = t.path().join("root-secret.json");
    std::fs::write(&secret, r#"{"secret":"s3cr3t-root"}"#).unwrap();
    let decoy = t.path().join("root-decoy");
    std::fs::write(&decoy, "keep").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let _ = std::fs::remove_file(profile1.join("Local State"));
    let _ = std::fs::remove_file(profile1.join("SingletonLock"));
    std::os::unix::fs::symlink(&secret, profile1.join("Local State")).unwrap();
    std::os::unix::fs::symlink(&decoy, profile1.join("SingletonLock")).unwrap();
    let again = svc2
        .start(user, 1)
        .await
        .expect("relaunch on the same profile after a restart");
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            r#"{"secret":"s3cr3t-root"}"#,
            "root never wrote through the planted Local State link"
        );
        let meta = std::fs::metadata(&secret).unwrap();
        assert_eq!((meta.uid(), meta.permissions().mode() & 0o777), (0, 0o600));
        assert!(decoy.exists(), "the planted SingletonLock target was not removed");
        let local_state = std::fs::read_to_string(profile1.join("Local State")).unwrap_or_default();
        assert!(
            !local_state.contains("s3cr3t"),
            "no root-only content leaked into the profile"
        );
        assert!(
            !std::fs::symlink_metadata(profile1.join("Local State"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced by the account's own file"
        );
    }
    assert!(again.is_running());
    svc2.stop(user, 1).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let left = std::process::Command::new("pgrep")
        .args(["-u", "aionui-browser"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&left.stdout).trim().is_empty(),
        "stop leaves no browser process"
    );
}

/// C35 (TASK-134): every engine of the Chromium family is downloaded through
/// its own verified chain, starts with the same protections and renders a
/// public page. Opt-in: `AIONUI_BROWSER_LIVE_ENGINES=1` (network, ~700 MB of
/// downloads, Xvfb, root). Runs as `nobody`, not as the `aionui-browser`
/// account: a test service reaps untracked processes of its account, which
/// would stop a production browser started meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_every_engine_installs_starts_and_renders() {
    use aionui_api_types::{BROWSER_ENGINES, BrowserEngine, BrowserEngineVerification, BrowserInstancePatch};

    if std::env::var("AIONUI_BROWSER_LIVE_ENGINES").is_err() {
        return;
    }
    const UID: u32 = 65534;
    let only = std::env::var("AIONUI_BROWSER_LIVE_ENGINE").ok();
    // `AIONUI_BROWSER_LIVE_ENGINES_ROOT` keeps the downloads between runs.
    let t = tempfile::Builder::new()
        .prefix("c35-live")
        .tempdir_in("/var/tmp")
        .unwrap();
    let base = std::env::var_os("AIONUI_BROWSER_LIVE_ENGINES_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| t.path().to_path_buf());
    std::fs::create_dir_all(&base).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o711)).unwrap();
    }
    let layout = BrowserLayout::new(base.join("aionui-browser"), base.join("data"));
    let svc = BrowserService::new(BrowserServiceConfig {
        layout: layout.clone(),
        host: None,
        idle_timeout: Duration::from_secs(900),
        guard: NavigationGuard::from_origins(["https://y.talkseeds.com"]),
        chrome_override: None,
        run_as_override: Some((UID, UID)),
    })
    .unwrap();
    let user = "engine_user";
    let account_processes = || {
        let out = std::process::Command::new("ps")
            .args(["-u", &UID.to_string(), "-o", "comm=,args="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Camoufox is not a CDP engine: `live_camoufox` covers it.
    for engine in BROWSER_ENGINES
        .into_iter()
        .filter(|e| aionui_browser::engine::spec(*e).protocol == aionui_browser::engine::EngineProtocol::Cdp)
    {
        if only.as_deref().is_some_and(|o| o != engine.id()) {
            continue;
        }
        let spec = aionui_browser::engine::spec(engine);
        svc.patch_instance(
            user,
            1,
            &BrowserInstancePatch {
                engine: Some(engine),
                ..Default::default()
            },
        )
        .unwrap();

        // 1. First start downloads the engine through its chain and records it.
        let t0 = std::time::Instant::now();
        let inst = match svc.start(user, 1).await {
            Ok(inst) => inst,
            Err(e) => panic!(
                "{engine:?} starts: {e}; engines: {:?}",
                svc.status(user).unwrap().engines
            ),
        };
        assert_eq!(inst.engine, engine);
        let status = svc.status(user).unwrap();
        assert_eq!(status.instances[0].engine_active, Some(engine));
        let row = status.engines.iter().find(|e| e.engine == engine).unwrap();
        let version = row.installed_version.clone().expect("installed version reported");
        assert_eq!(row.error, None);
        let record: Value = serde_json::from_slice(
            &std::fs::read(
                layout
                    .engine_current(engine)
                    .join(aionui_browser::install::VERIFY_RECORD),
            )
            .unwrap(),
        )
        .unwrap();
        let want = match spec.verification() {
            BrowserEngineVerification::SignedRepository => "signed-repository",
            BrowserEngineVerification::TlsOnly => "tls-only",
            BrowserEngineVerification::PinnedSha256 => "pinned-sha256",
        };
        assert_eq!(record["verification"], want, "{engine:?}");
        assert_eq!(record["sha256"].as_str().map(str::len), Some(64));
        eprintln!(
            "{}: version {version} (reports {}), {want}, sha256 {}, install+start {} ms",
            engine.id(),
            inst.version,
            record["sha256"].as_str().unwrap(),
            t0.elapsed().as_millis()
        );

        // 2. Same protections: the engine's binary runs as the account with
        //    the pipe, the proxy and no debugging port; no crash handler.
        let ps = account_processes();
        let main = ps
            .lines()
            .find(|l| l.contains("--remote-debugging-pipe") && !l.contains("--type="))
            .unwrap_or_else(|| panic!("{engine:?}: no browser process as the account:\n{ps}"));
        assert!(
            main.contains(&format!("/{} ", spec.binary)),
            "{engine:?} runs its own binary: {main}"
        );
        assert!(main.contains("--proxy-server=http://127.0.0.1:") && main.contains("--proxy-bypass-list=<-loopback>"));
        assert!(!ps.contains("--remote-debugging-port"), "{ps}");
        assert_eq!(
            ps.lines().any(|l| l.starts_with("Xvfb")),
            spec.needs_display,
            "{engine:?} Xvfb only for headful engines:\n{ps}"
        );
        // comm is the first column (arguments mention `--crashpad-handler-pid`).
        assert!(
            !ps.lines()
                .any(|l| l.split_whitespace().next().is_some_and(|c| c.contains("crashpad"))),
            "{engine:?} crash handler still runs:\n{ps}"
        );

        // 3. A public page renders through the bridge: Discord's login form.
        let env: HashMap<_, _> = svc.agent_env(user).await.into_iter().collect();
        let mut agent = Agent::connect(&env[ENV_ACTIVE_PORT], &env[ENV_ACTIVE_TOKEN]).await;
        let created = agent
            .call("Target.createTarget", json!({"url": "about:blank"}), None)
            .await;
        let target = created["result"]["targetId"].as_str().expect("target").to_owned();
        let attached = agent
            .call(
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
                None,
            )
            .await;
        let session = attached["result"]["sessionId"].as_str().expect("session").to_owned();
        let cookies = agent.call("Network.getAllCookies", json!({}), None).await;
        assert!(
            cookies["error"].is_object(),
            "{engine:?} filter still refuses cookies: {cookies}"
        );
        let nav = agent
            .call(
                "Page.navigate",
                json!({"url": "https://discord.com/login"}),
                Some(&session),
            )
            .await;
        assert!(nav["result"]["frameId"].is_string(), "{engine:?} {nav}");
        let mut discord = Value::Null;
        // A cold profile's first load of Discord's app can take over 30 s.
        for _ in 0..90 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            discord = agent
                .call(
                    "Runtime.evaluate",
                    json!({"expression": "!!document.querySelector('input[name=email]')", "returnByValue": true}),
                    Some(&session),
                )
                .await;
            if discord["result"]["result"]["value"] == json!(true) {
                break;
            }
        }
        if discord["result"]["result"]["value"] != json!(true) {
            let seen = agent
                .call(
                    "Runtime.evaluate",
                    json!({"expression": "location.href + ' | ' + document.title + ' | ' + (document.body ? document.body.innerText.slice(0, 300) : '')", "returnByValue": true}),
                    Some(&session),
                )
                .await;
            svc.stop(user, 1).await.unwrap();
            panic!("{engine:?}: Discord login did not render: {discord}; page: {seen}");
        }
        let blocked = agent
            .call(
                "Page.navigate",
                json!({"url": "https://y.talkseeds.com/"}),
                Some(&session),
            )
            .await;
        assert!(
            blocked["result"]["errorText"]
                .as_str()
                .unwrap_or("")
                .contains("BLOCKED"),
            "{engine:?} guard blocks the AionUi origin: {blocked}"
        );

        // 4. No TCP listener owned by the account (CDP is the pipe only).
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let text = std::fs::read_to_string(table).unwrap_or_default();
            let listening: Vec<&str> = text
                .lines()
                .skip(1)
                .filter(|l| {
                    let f: Vec<&str> = l.split_whitespace().collect();
                    f.len() > 7 && f[3] == "0A" && f[7] == UID.to_string()
                })
                .collect();
            assert!(listening.is_empty(), "{engine:?} {table}: listens on {listening:?}");
        }

        drop(agent);
        svc.stop(user, 1).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(account_processes().trim(), "", "{engine:?} left processes behind");
        eprintln!("{}: OK", engine.id());
    }
    // Engines sit side by side; Chrome keeps its C30 directory.
    if only.is_none() {
        assert!(layout.chrome_binary().exists());
        assert!(layout.engine_binary(BrowserEngine::Edge).exists());
    }
}
