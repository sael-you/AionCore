//! One running Chrome instance (C30): Xvfb + headful Chrome as the browser
//! account, AionCore's own browser-level CDP session, the tab model and the
//! navigation guard.
//!
//! Guard before the first request: AionCore's own browser-level session runs
//! `Fetch.enable` with the guard's patterns BEFORE anything else, which
//! intercepts the requests of every target of the browser, including the
//! first navigation of a brand-new target. (Per-target `Fetch.enable` after a
//! `waitForDebuggerOnStart` auto-attach is too late: Chrome 147 had already
//! sent a `createTarget{url}` target's first request before the pause was
//! released — reproduced with a raw CDP probe on 2026-11-16; browser-level
//! Fetch paused that same request.) Paused requests are decided by
//! [`NavigationGuard::decide`]. AionCore also auto-attaches (no pause) to
//! every page so navigations that never touch the network (`file:`,
//! `chrome:`) can be sent back to `about:blank` on `targetInfoChanged`.
//!
//! Stop: root here lacks CAP_KILL and cannot signal the browser account's
//! processes, so the stop is `Browser.close` over our own session, with a
//! `kill` helper spawned AS the browser account as fallback.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::time::Duration;

use aionui_api_types::{BrowserEngine, BrowserErrorCode};
use serde_json::{Value, json};
use tokio::process::Child;
use tokio::sync::{Mutex, broadcast, watch};

use crate::cdp::{CdpClient, CdpEvent};
use crate::error::BrowserError;
use crate::guard::{GuardDecision, NavigationGuard};
use crate::host::kill_group_as as kill_as;

/// Everything needed to launch one instance.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub user_id: String,
    pub instance: u8,
    /// The engine and its binary (C35; every engine speaks CDP on the pipe).
    pub engine: BrowserEngine,
    pub chrome: PathBuf,
    pub profile: PathBuf,
    pub home: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub hide_automation: bool,
    pub max_tabs: Arc<AtomicU8>,
    pub guard: Arc<NavigationGuard>,
}

/// One page target as the panel and status see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabRecord {
    pub id: String,
    pub url: String,
    pub title: String,
}

/// Notices the panel shows (blocked navigation, tab limit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceNotice {
    pub code: BrowserErrorCode,
    pub message: String,
}

pub struct ChromeInstance {
    pub user_id: String,
    pub instance: u8,
    pub hide_automation: bool,
    /// The engine this instance was launched with.
    pub engine: BrowserEngine,
    pub version: String,
    /// The only CDP channel (Chrome's pipe), shared by AionCore and agent links.
    mux: crate::mux::CdpMux,
    /// The profile directory (the reaper keeps processes that use it).
    pub profile: PathBuf,
    /// Chrome's private endpoint pieces (never logged or sent out).
    uid: u32,
    gid: u32,
    chrome_child: Mutex<Option<Child>>,
    xvfb_child: Mutex<Option<Child>>,
    chrome_pid: Option<u32>,
    xvfb_pid: Option<u32>,
    display: u16,
    tabs: watch::Sender<Vec<TabRecord>>,
    notices: broadcast::Sender<InstanceNotice>,
    max_tabs: Arc<AtomicU8>,
    last_activity_ms: AtomicI64,
    agent_last_ms: AtomicI64,
    agent_tab: std::sync::Mutex<Option<String>>,
    exited: watch::Receiver<bool>,
    /// The egress proxy of this Chrome (lives and dies with the instance).
    _proxy: crate::proxy::EgressProxy,
}

impl std::fmt::Debug for ChromeInstance {
    // Hand-written: never print Chrome's private port or endpoint path.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromeInstance")
            .field("user_id", &self.user_id)
            .field("instance", &self.instance)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

const START_TIMEOUT: Duration = Duration::from_secs(25);
/// "agent is driving" lasts this long after the last agent command.
const AGENT_ACTIVE_MS: i64 = 5_000;

fn now_ms() -> i64 {
    aionui_common::now_ms()
}

/// Displays in use by this process (X server numbers).
static DISPLAYS: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

fn allocate_display() -> Option<u16> {
    let mut used = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let n = (110..400).find(|n| {
        !used.contains(n)
            && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
            && !Path::new(&format!("/tmp/.X{n}-lock")).exists()
    })?;
    used.push(n);
    Some(n)
}

fn release_display(n: u16) {
    DISPLAYS.lock().unwrap_or_else(|e| e.into_inner()).retain(|d| *d != n);
}

/// Chrome's launch arguments. No CDP port at all (`--remote-debugging-pipe`).
/// All traffic goes through the egress proxy (only the browser account may
/// use it, so its port in argv opens nothing).
///
/// The same list for every engine (C35): they are all Chromium, and each of
/// the protections below is a Chromium switch.
pub fn chrome_args(profile: &Path, hide_automation: bool, proxy_port: u16) -> Vec<String> {
    let mut args = vec![
        // The container forbids every Chrome sandbox (NoNewPrivs, seccomp, no
        // user namespaces); isolation = the separate account (decision d).
        "--no-sandbox".to_owned(),
        format!("--user-data-dir={}", profile.display()),
        // CDP over fds 3/4 only: no debugging TCP port that a local process
        // could reach around the bridge (train-8 review).
        "--remote-debugging-pipe".to_owned(),
        // /dev/shm is 64 MB here: without this Chrome crashes or renders blank.
        "--disable-dev-shm-usage".to_owned(),
        "--no-first-run".to_owned(),
        "--password-store=basic".to_owned(),
        // No crash uploads (dumps can hold page memory: cookies, tokens).
        // These two flags do not stop chrome_crashpad_handler for a non-root
        // user (probed 2026-11-16; --disable-crashpad-for-testing does, but
        // then every navigation fails with ERR_ABORTED), so the launch also
        // stops the handlers (`stop_crash_handlers`) and turns metrics
        // reporting off in Local State.
        "--disable-breakpad".to_owned(),
        "--disable-crash-reporter".to_owned(),
        "--window-position=0,0".to_owned(),
        "--window-size=1280,800".to_owned(),
        // Network-level egress guard (train-7 review): every request, WebSocket
        // and TLS tunnel goes through AionCore's proxy; `<-loopback>` removes
        // Chrome's implicit loopback bypass; WebRTC may not send UDP around it.
        format!("--proxy-server=http://127.0.0.1:{proxy_port}"),
        "--proxy-bypass-list=<-loopback>".to_owned(),
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_owned(),
    ];
    if hide_automation {
        args.push("--disable-blink-features=AutomationControlled".to_owned());
    }
    args.push("about:blank".to_owned());
    args
}

async fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Chrome's `--remote-debugging-pipe` descriptors: Chrome reads commands on
/// fd 3 and writes replies/events on fd 4. Every end is close-on-exec and
/// >= 10 in AionCore; only the two child ends are mapped into Chrome.
struct CdpPipes {
    to_chrome: OwnedFd,
    chrome_reads: OwnedFd,
    chrome_writes: OwnedFd,
    from_chrome: OwnedFd,
}

fn pipe_high() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: plain syscalls on descriptors this function owns.
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let raise = |fd: i32| -> std::io::Result<OwnedFd> {
            let high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
            libc::close(fd);
            if high < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(OwnedFd::from_raw_fd(high))
        };
        Ok((raise(fds[0])?, raise(fds[1])?))
    }
}

impl CdpPipes {
    fn new() -> std::io::Result<Self> {
        let (chrome_reads, to_chrome) = pipe_high()?;
        let (from_chrome, chrome_writes) = pipe_high()?;
        Ok(Self {
            to_chrome,
            chrome_reads,
            chrome_writes,
            from_chrome,
        })
    }

    fn child_map(&self) -> Vec<(i32, i32)> {
        vec![(self.chrome_reads.as_raw_fd(), 3), (self.chrome_writes.as_raw_fd(), 4)]
    }

    /// Our two ends; the child ends close here (Chrome has its copies).
    fn into_parent_ends(self) -> (OwnedFd, OwnedFd) {
        (self.to_chrome, self.from_chrome)
    }
}

impl ChromeInstance {
    pub async fn launch(spec: LaunchSpec) -> Result<Arc<Self>, BrowserError> {
        let display = allocate_display()
            .ok_or_else(|| BrowserError::coded(BrowserErrorCode::BrowserStartFailed, "no free X display"))?;
        match Self::launch_on(spec, display).await {
            Ok(i) => Ok(i),
            Err(e) => {
                release_display(display);
                Err(e)
            }
        }
    }

    async fn launch_on(spec: LaunchSpec, display: u16) -> Result<Arc<Self>, BrowserError> {
        let failed = |what: &str| BrowserError::coded(BrowserErrorCode::BrowserStartFailed, what.to_owned());
        let base_env = |cmd: &mut aionui_runtime::Builder| {
            cmd.env_clear()
                .env("HOME", &spec.home)
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .env("LANG", "en_US.UTF-8")
                .env("DISPLAY", format!(":{display}"));
        };

        let mut xvfb = aionui_runtime::Builder::new("Xvfb");
        base_env(&mut xvfb);
        xvfb.args([
            format!(":{display}"),
            "-screen".into(),
            "0".into(),
            "1920x1080x24".into(),
            "-nolisten".into(),
            "tcp".into(),
        ])
        .uid(spec.uid)
        .gid(spec.gid)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        // A headless engine (chrome-headless-shell) draws no window: no Xvfb.
        let mut xvfb_child = None;
        let mut xvfb_pid = None;
        if crate::engine::spec(spec.engine).needs_display {
            let mut child = xvfb.spawn().map_err(|_| failed("Xvfb did not start"))?;
            xvfb_pid = child.id();
            if !wait_for(Path::new(&format!("/tmp/.X11-unix/X{display}")), Duration::from_secs(8)).await {
                if let Some(pid) = xvfb_pid {
                    kill_as(spec.uid, spec.gid, pid, "KILL").await;
                }
                let _ = child.wait().await;
                return Err(failed("Xvfb display did not appear"));
            }
            xvfb_child = Some(child);
        }

        let proxy = match crate::proxy::EgressProxy::start(spec.guard.clone(), spec.uid).await {
            Ok(p) => p,
            Err(_) => {
                if let Some(pid) = xvfb_pid {
                    kill_as(spec.uid, spec.gid, pid, "KILL").await;
                }
                return Err(failed("the egress proxy did not start"));
            }
        };
        // A Chrome that died with AionCore leaves its profile lock behind.
        // Both run AS the account (the profile is the account's directory).
        crate::host::remove_profile_locks(&spec.profile, spec.uid, spec.gid).await;
        crate::host::disable_metrics_reporting(&spec.profile, spec.uid, spec.gid).await;
        let pipes = match CdpPipes::new() {
            Ok(p) => p,
            Err(_) => {
                if let Some(pid) = xvfb_pid {
                    kill_as(spec.uid, spec.gid, pid, "KILL").await;
                }
                return Err(failed("could not create the CDP pipe"));
            }
        };
        let mut chrome = aionui_runtime::Builder::new(&spec.chrome);
        base_env(&mut chrome);
        chrome
            .args(chrome_args(&spec.profile, spec.hide_automation, proxy.port))
            .current_dir(&spec.home)
            .uid(spec.uid)
            .gid(spec.gid)
            .map_fds(pipes.child_map())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let spawned = chrome.spawn();
        let (to_chrome, from_chrome) = pipes.into_parent_ends();
        let chrome_child = match spawned {
            Ok(c) => c,
            Err(_) => {
                if let Some(pid) = xvfb_pid {
                    kill_as(spec.uid, spec.gid, pid, "TERM").await;
                }
                return Err(failed("Chrome did not start"));
            }
        };
        let chrome_pid = chrome_child.id();
        let kill_all = || async {
            for pid in [chrome_pid, xvfb_pid].into_iter().flatten() {
                kill_as(spec.uid, spec.gid, pid, "KILL").await;
            }
        };
        let mux = match crate::mux::CdpMux::over_pipe(to_chrome, from_chrome) {
            Ok(m) => m,
            Err(_) => {
                kill_all().await;
                return Err(failed("could not open the CDP pipe"));
            }
        };
        // Readiness: Chrome answers on the pipe once the browser is up.
        let version =
            match tokio::time::timeout(START_TIMEOUT, mux.root().call("Browser.getVersion", json!({}), None)).await {
                Ok(Ok(v)) => v.get("product").and_then(Value::as_str).unwrap_or_default().to_owned(),
                _ => {
                    kill_all().await;
                    return Err(failed("Chrome did not answer on the CDP pipe"));
                }
            };

        // The browser is up: no crash handler may run (no dumps, no uploads).
        crate::host::stop_crash_handlers(spec.uid, spec.gid, &spec.profile).await;

        let (exited_tx, exited_rx) = watch::channel(false);
        let (tabs_tx, _) = watch::channel(Vec::new());
        let (notices, _) = broadcast::channel(64);
        let inst = Arc::new(Self {
            user_id: spec.user_id.clone(),
            instance: spec.instance,
            hide_automation: spec.hide_automation,
            engine: spec.engine,
            // `Chrome/157…`, `HeadlessChrome/157…`: keep the number.
            version: version.rsplit('/').next().unwrap_or_default().to_owned(),
            mux,
            profile: spec.profile.clone(),
            uid: spec.uid,
            gid: spec.gid,
            chrome_child: Mutex::new(Some(chrome_child)),
            xvfb_child: Mutex::new(xvfb_child),
            chrome_pid,
            xvfb_pid,
            display,
            tabs: tabs_tx,
            notices,
            max_tabs: spec.max_tabs.clone(),
            last_activity_ms: AtomicI64::new(now_ms()),
            agent_last_ms: AtomicI64::new(0),
            agent_tab: std::sync::Mutex::new(None),
            exited: exited_rx,
            _proxy: proxy,
        });

        inst.clone().spawn_event_loop(spec.guard.clone());
        inst.clone().spawn_exit_watch(exited_tx);
        // Order matters: browser-wide interception before anything can navigate.
        // Fail CLOSED: a Chrome without its guard is stopped, never handed out.
        if let Err(e) = inst.setup_guard(&spec.guard).await {
            tracing::error!(user_id = %inst.user_id, instance = inst.instance, error = %e, "browser guard setup failed; stopping Chrome");
            inst.stop().await;
            return Err(failed("the navigation guard could not be installed"));
        }
        tracing::info!(
            user_id = %inst.user_id,
            instance = inst.instance,
            engine = inst.engine.id(),
            chrome = %inst.version,
            hide_automation = inst.hide_automation,
            "server browser instance started"
        );
        Ok(inst)
    }

    async fn setup_guard(&self, guard: &NavigationGuard) -> Result<(), BrowserError> {
        let patterns: Vec<Value> = guard
            .fetch_patterns()
            .into_iter()
            .map(|p| json!({"urlPattern": p, "requestStage": "Request"}))
            .collect();
        self.cdp()
            .call("Fetch.enable", json!({"patterns": patterns}), None)
            .await?;
        self.cdp()
            .call(
                "Target.setAutoAttach",
                json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
                None,
            )
            .await?;
        self.cdp()
            .call("Target.setDiscoverTargets", json!({"discover": true}), None)
            .await?;
        Ok(())
    }

    /// Process groups of this instance (Chrome and Xvfb lead their own).
    pub fn process_groups(&self) -> Vec<u32> {
        [self.chrome_pid, self.xvfb_pid].into_iter().flatten().collect()
    }

    fn spawn_exit_watch(self: Arc<Self>, exited: watch::Sender<bool>) {
        tokio::spawn(async move {
            let child = self.chrome_child.lock().await.take();
            if let Some(mut child) = child {
                let _ = child.wait().await;
            }
            let _ = exited.send(true);
            if let Some(pid) = self.xvfb_pid {
                kill_as(self.uid, self.gid, pid, "TERM").await;
            }
            if let Some(mut x) = self.xvfb_child.lock().await.take() {
                let _ = tokio::time::timeout(Duration::from_secs(5), x.wait()).await;
            }
            release_display(self.display);
            tracing::info!(user_id = %self.user_id, instance = self.instance, "server browser instance exited");
        });
    }

    fn spawn_event_loop(self: Arc<Self>, guard: Arc<NavigationGuard>) {
        let mut events = self.cdp().subscribe();
        let weak = Arc::downgrade(&self);
        drop(self);
        tokio::spawn(async move {
            // targetId → our auto-attach session (for guard redirects).
            let mut sessions: HashMap<String, String> = HashMap::new();
            let mut pages: Vec<TabRecord> = Vec::new();
            loop {
                let ev = match events.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "server browser event loop lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(inst) = weak.upgrade() else { break };
                inst.handle_event(&ev, &guard, &mut sessions, &mut pages).await;
            }
        });
    }

    async fn handle_event(
        &self,
        ev: &CdpEvent,
        guard: &NavigationGuard,
        sessions: &mut HashMap<String, String>,
        pages: &mut Vec<TabRecord>,
    ) {
        match ev.method.as_str() {
            "Target.attachedToTarget" => {
                let Some(session) = ev.params["sessionId"].as_str().map(str::to_owned) else {
                    return;
                };
                let target_id = ev.params["targetInfo"]["targetId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let kind = ev.params["targetInfo"]["type"].as_str().unwrap_or_default().to_owned();
                if kind == "page" {
                    sessions.insert(target_id, session);
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(sid) = ev.params["sessionId"].as_str() {
                    sessions.retain(|_, s| s != sid);
                }
            }
            "Fetch.requestPaused" => {
                // Browser-level interception: no session id (a target's own
                // session would carry one; answer in the same scope).
                let session = ev.session_id.clone();
                let request_id = ev.params["requestId"].as_str().unwrap_or_default().to_owned();
                let url = ev.params["request"]["url"].as_str().unwrap_or_default();
                let decision = guard.decide(url);
                if decision == GuardDecision::Block {
                    let host = url::Url::parse(url)
                        .ok()
                        .and_then(|u| u.host_str().map(str::to_owned))
                        .unwrap_or_else(|| url.split(':').next().unwrap_or("").to_owned());
                    tracing::info!(user_id = %self.user_id, instance = self.instance, %host, "server browser: blocked a request");
                    let _ = self.notices.send(InstanceNotice {
                        code: BrowserErrorCode::BrowserUrlBlocked,
                        message: host,
                    });
                    self.cdp().notify(
                        "Fetch.failRequest",
                        json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
                        session.as_deref(),
                    );
                } else {
                    self.cdp().notify(
                        "Fetch.continueRequest",
                        json!({"requestId": request_id}),
                        session.as_deref(),
                    );
                }
            }
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                let info = &ev.params["targetInfo"];
                if info["type"].as_str() != Some("page") {
                    return;
                }
                let record = TabRecord {
                    id: info["targetId"].as_str().unwrap_or_default().to_owned(),
                    url: info["url"].as_str().unwrap_or_default().to_owned(),
                    title: info["title"].as_str().unwrap_or_default().to_owned(),
                };
                // Local schemes never reach Fetch: send the tab back.
                if !record.url.is_empty()
                    && guard.decide(&record.url) == GuardDecision::Block
                    && let Some(session) = sessions.get(&record.id)
                {
                    let _ = self.notices.send(InstanceNotice {
                        code: BrowserErrorCode::BrowserUrlBlocked,
                        message: record.url.split(':').next().unwrap_or("").to_owned(),
                    });
                    self.cdp()
                        .notify("Page.navigate", json!({"url": "about:blank"}), Some(session));
                }
                match pages.iter_mut().find(|p| p.id == record.id) {
                    Some(existing) => *existing = record,
                    None => {
                        let limit = usize::from(self.max_tabs.load(Ordering::Relaxed).max(1));
                        if pages.len() >= limit {
                            // A popup beyond the limit (explicit creates are refused earlier).
                            let _ = self.notices.send(InstanceNotice {
                                code: BrowserErrorCode::BrowserTabLimit,
                                message: format!("max {limit} tabs"),
                            });
                            self.cdp()
                                .notify("Target.closeTarget", json!({"targetId": record.id}), None);
                            return;
                        }
                        pages.push(record);
                    }
                }
                self.tabs.send_replace(pages.clone());
            }
            "Target.targetDestroyed" => {
                if let Some(id) = ev.params["targetId"].as_str() {
                    pages.retain(|p| p.id != id);
                    sessions.remove(id);
                    self.tabs.send_replace(pages.clone());
                }
            }
            _ => {}
        }
    }

    pub fn cdp(&self) -> &CdpClient {
        self.mux.root()
    }

    /// A CDP link for one agent connection (its own default session).
    pub async fn open_agent_link(&self, target: crate::mux::LinkTarget) -> Result<crate::mux::AgentLink, BrowserError> {
        self.mux.open_link(target).await
    }

    pub fn tabs(&self) -> watch::Receiver<Vec<TabRecord>> {
        self.tabs.subscribe()
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.borrow().len()
    }

    pub fn max_tabs(&self) -> u8 {
        self.max_tabs.load(Ordering::Relaxed)
    }

    pub fn notices(&self) -> broadcast::Receiver<InstanceNotice> {
        self.notices.subscribe()
    }

    pub fn is_running(&self) -> bool {
        !*self.exited.borrow() && !self.cdp().is_closed()
    }

    pub fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn idle_ms(&self) -> i64 {
        now_ms() - self.last_activity_ms.load(Ordering::Relaxed)
    }

    /// An agent command reached this instance (optionally for a tab).
    pub fn mark_agent(&self, tab: Option<&str>) {
        self.agent_last_ms.store(now_ms(), Ordering::Relaxed);
        self.touch();
        if let Some(tab) = tab {
            *self.agent_tab.lock().unwrap_or_else(|e| e.into_inner()) = Some(tab.to_owned());
        }
    }

    pub fn agent_active(&self) -> bool {
        now_ms() - self.agent_last_ms.load(Ordering::Relaxed) < AGENT_ACTIVE_MS
    }

    pub fn agent_tab(&self) -> Option<String> {
        self.agent_tab.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Resolves once Chrome has exited.
    pub async fn exited(&self) {
        let mut rx = self.exited.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Close Chrome over CDP; fall back to a kill run as the browser account.
    pub async fn stop(&self) {
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.cdp().call("Browser.close", json!({}), None),
        )
        .await;
        if tokio::time::timeout(Duration::from_secs(8), self.exited())
            .await
            .is_err()
            && let Some(pid) = self.chrome_pid
        {
            tracing::warn!(user_id = %self.user_id, instance = self.instance, "Browser.close did not stop Chrome; killing it");
            kill_as(self.uid, self.gid, pid, "KILL").await;
            let _ = tokio::time::timeout(Duration::from_secs(5), self.exited()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_args_carry_no_port_or_token_and_honour_hide_automation() {
        let args = chrome_args(Path::new("/b/u/1"), false, 41000);
        assert!(args.contains(&"--remote-debugging-pipe".to_owned()));
        assert!(
            !args.contains(&"--disable-crashpad-for-testing".to_owned()),
            "breaks every navigation (ERR_ABORTED)"
        );
        assert!(args.contains(&"--disable-crash-reporter".to_owned()));
        assert!(
            !args.iter().any(|a| a.starts_with("--remote-debugging-port")),
            "no TCP debugging port"
        );
        assert!(args.contains(&"--user-data-dir=/b/u/1".to_owned()));
        assert!(args.contains(&"--disable-dev-shm-usage".to_owned()));
        assert!(!args.iter().any(|a| a.contains("AutomationControlled")));
        assert!(!args.iter().any(|a| a.contains("token")));
        assert!(args.contains(&"--proxy-server=http://127.0.0.1:41000".to_owned()));
        assert!(args.contains(&"--proxy-bypass-list=<-loopback>".to_owned()));
        assert!(args.contains(&"--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_owned()));
        let hidden = chrome_args(Path::new("/b/u/1"), true, 41000);
        assert!(hidden.contains(&"--disable-blink-features=AutomationControlled".to_owned()));
        assert_eq!(hidden.len(), args.len() + 1, "no other flag changes");
    }
}
