//! Live end-to-end check of the Camoufox engine (TASK-136 K6), as the real
//! `aionui-browser` account. Opt-in: `AIONUI_BROWSER_LIVE_CAMOUFOX=1` (needs
//! network, Xvfb, node, root to switch uid). The engine installs into
//! `AIONUI_BROWSER_ROOT` (default `/root/.clauterm/aionui-browser`, the real
//! engines root, so the install stays usable); settings and staging live in a
//! temp data dir; the test user's profile is removed at the end.
//!
//! Checks: start; a public page renders; `navigator.webdriver` is false on a
//! fingerprint page; a loopback URL is refused (filter) and a page's own
//! loopback fetch never arrives (egress proxy); `cookie-list` is refused; the
//! VNC socket speaks RFB; stop leaves no process of the instance.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aionui_api_types::{BrowserEngine, BrowserInstanceUpdate, BrowserSettingsUpdate};
use aionui_browser::bridge::{ENV_CAMOUFOX_CLI, ENV_INSTANCES, ENV_PLAYWRIGHT_REMOTE_HEADERS};
use aionui_browser::{BrowserLayout, BrowserService, BrowserServiceConfig, NavigationGuard};

fn live() -> bool {
    std::env::var("AIONUI_BROWSER_LIVE_CAMOUFOX").is_ok()
}

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

/// Run the pinned playwright-cli with the agent env; returns (exit ok, output).
fn cli(bin: &str, env: &[(String, String)], cwd: &Path, args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new(bin)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", cwd)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(cwd)
        .output()
        .expect("run playwright-cli");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// The browser account's uid.
fn browser_uid() -> u32 {
    let out = std::process::Command::new("id")
        .args(["-u", "aionui-browser"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// (hex local address, port) of the listening TCP sockets owned by `uid`.
fn listening_tcp_of(uid: u32) -> Vec<(String, u16)> {
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .flat_map(|t| {
            t.lines()
                .skip(1)
                .filter_map(|l| {
                    let f: Vec<&str> = l.split_whitespace().collect();
                    let (local, state, owner) = (f.get(1)?, f.get(3)?, f.get(7)?);
                    if *state != "0A" || owner.parse::<u32>().ok()? != uid {
                        return None;
                    }
                    let (addr, port) = local.split_once(':')?;
                    Some((addr.to_owned(), u16::from_str_radix(port, 16).ok()?))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn instance_processes(profile: &Path) -> Vec<String> {
    let ps = std::process::Command::new("ps")
        .args(["-eo", "user:20,pid,args"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| l.starts_with("aionui-browser") && l.contains(&*profile.to_string_lossy()))
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn live_camoufox_end_to_end() {
    if !live() {
        return;
    }
    let root = std::env::var_os("AIONUI_BROWSER_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root/.clauterm/aionui-browser"));
    let data = tempfile::Builder::new()
        .prefix("k6-live")
        .tempdir_in("/root/.clauterm")
        .unwrap();
    let svc = BrowserService::new(BrowserServiceConfig {
        layout: BrowserLayout::new(&root, data.path()),
        host: None,
        idle_timeout: Duration::from_secs(900),
        guard: NavigationGuard::from_origins(["https://y.talkseeds.com"]),
        chrome_override: None,
        run_as_override: None,
    })
    .unwrap();
    let user = "k6_live";
    svc.update_settings(
        user,
        &BrowserSettingsUpdate {
            max_tabs: Some(3),
            agents_enabled: Some(true),
            instances: Some(vec![BrowserInstanceUpdate {
                instance: 1,
                engine: Some(BrowserEngine::Camoufox),
                ..Default::default()
            }]),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // 1. Start (installs the engine on first use).
    let t0 = std::time::Instant::now();
    let inst = match svc.start_juggler(user, 1).await {
        Ok(i) => i,
        Err(e) => {
            let engines = svc.status(user).map(|s| s.engines).unwrap_or_default();
            panic!("camoufox did not start: {e:?}; engines: {engines:?}");
        }
    };
    eprintln!("start incl. install ms = {}", t0.elapsed().as_millis());
    let status = svc.status(user).unwrap();
    assert!(status.instances[0].running);
    assert_eq!(status.instances[0].engine_active, Some(BrowserEngine::Camoufox));
    let procs = instance_processes(&inst.profile);
    eprintln!("{} processes of the instance", procs.len());
    assert!(
        procs.iter().any(|l| l.contains("camoufox-bin")),
        "Camoufox runs as aionui-browser: {procs:?}"
    );

    // 2. The VNC view speaks RFB on its Unix socket (no TCP port).
    {
        let mut s = std::os::unix::net::UnixStream::connect(inst.vnc_socket()).expect("vnc socket");
        let mut hello = [0u8; 12];
        s.read_exact(&mut hello).unwrap();
        assert_eq!(&hello, b"RFB 003.008\n");
    }
    // /proc/<pid>/fd of another account needs ptrace rights; check the
    // account's TCP listeners instead: loopback only (the sidecar's endpoint),
    // none in the VNC range.
    for (addr, port) in listening_tcp_of(browser_uid()) {
        assert!(
            addr == "0100007F" || addr == "00000000000000000000000001000000",
            "non-loopback listener {addr}:{port}"
        );
        assert!(!(5900..6000).contains(&port), "a VNC TCP port is open: {port}");
    }

    // 3. An agent through the bridge with the pinned client.
    let env = svc.agent_env(user).await;
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
    let rows: serde_json::Value = serde_json::from_str(&get(ENV_INSTANCES)).unwrap();
    let endpoint = rows[0]["endpoint"].as_str().expect("camoufox endpoint").to_owned();
    assert_eq!(rows[0]["engine"], "camoufox");
    assert_eq!(rows[0]["protocol"], "playwright");
    let bin = get(ENV_CAMOUFOX_CLI);
    let agent_env: Vec<(String, String)> = vec![(
        ENV_PLAYWRIGHT_REMOTE_HEADERS.to_owned(),
        get(ENV_PLAYWRIGHT_REMOTE_HEADERS),
    )];
    let cwd = tempfile::tempdir_in("/root/.clauterm").unwrap();
    let run = |args: &[&str]| cli(&bin, &agent_env, cwd.path(), args);
    let (ok, out) = run(&["-s=k6", "attach", "--endpoint", &endpoint]);
    assert!(ok, "attach: {out}");
    let (ok, out) = run(&["-s=k6", "goto", "https://example.com"]);
    assert!(ok && out.contains("Example Domain"), "public page: {out}");
    let (ok, out) = run(&["-s=k6", "goto", "https://bot.sannysoft.com"]);
    assert!(ok, "fingerprint page: {out}");
    let (_, out) = run(&["-s=k6", "eval", "String(navigator.webdriver)"]);
    assert!(
        out.contains("\"false\"") || out.contains("false"),
        "navigator.webdriver: {out}"
    );
    assert!(!out.contains("\"true\""), "navigator.webdriver: {out}");

    // 4. Loopback: refused by the filter, and a page's own fetch never arrives.
    let (port, hits) = counting_server();
    let (_, out) = run(&["-s=k6", "goto", &format!("http://127.0.0.1:{port}/")]);
    assert!(out.contains("AionUi refused"), "loopback goto: {out}");
    let (_, out) = run(&[
        "-s=k6",
        "eval",
        &format!("fetch('http://127.0.0.1:{port}/x').then(()=>'reached', e=>'blocked')"),
    ]);
    eprintln!("page fetch to loopback: {out}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing reached the loopback server");

    // 4b. Review part 2: a route cannot rewrite a request to a local file,
    //     and raw response headers carry no Set-Cookie.
    let (_, out) = run(&[
        "-s=k6",
        "run-code",
        "async page => { let err = 'no error'; await page.route('**/redir', r => r.continue({ url: 'file:///etc/hostname' }).catch(e => { err = e.message; return r.abort(); })); await page.goto('https://example.com/redir').catch(() => {}); await page.unroute('**/redir'); return err; }",
    ]);
    assert!(out.contains("AionUi refused"), "route to file://: {out}");
    let (_, out) = run(&[
        "-s=k6",
        "run-code",
        "async page => { const r = await page.goto('https://github.com/'); const h = await r.headersArray(); return 'cookie headers: ' + h.filter(x => /cookie/i.test(x.name)).length + ' of ' + h.length; }",
    ]);
    eprintln!("raw headers: {out}");
    assert!(out.contains("cookie headers: 0 of "), "raw response headers: {out}");
    assert!(
        !out.contains("cookie headers: 0 of 0"),
        "headers were read at all: {out}"
    );
    let (_, out) = run(&["-s=k6", "localstorage-list"]);
    eprintln!(
        "localstorage-list: {}",
        out.lines().take(3).collect::<Vec<_>>().join(" | ")
    );

    // 5. Cookies are refused.
    let (_, out) = run(&["-s=k6", "cookie-list"]);
    assert!(out.contains("AionUi refused"), "cookie-list: {out}");
    let _ = run(&["-s=k6", "detach"]);

    // 6. Stop leaves no process of the instance.
    let profile = inst.profile.clone();
    svc.stop(user, 1).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let left = instance_processes(&profile);
    assert!(left.is_empty(), "left after stop: {left:?}");
    let x11vnc_left = std::process::Command::new("pgrep")
        .args(["-f", &*inst.vnc_socket().to_string_lossy()])
        .output()
        .unwrap();
    assert!(x11vnc_left.stdout.is_empty(), "x11vnc left after stop");
    let _ = std::fs::remove_dir_all(root.join(user));
}
