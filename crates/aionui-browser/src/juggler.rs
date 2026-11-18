//! One running Camoufox instance (TASK-136 K6): Xvfb + x11vnc + the
//! Playwright sidecar, all as the browser account.
//!
//! - Xvfb: the instance's display (same as Chrome's).
//! - Egress proxy: the same [`crate::proxy::EgressProxy`] as Chrome; the
//!   sidecar sets Firefox's prefs so every connection, loopback included,
//!   goes through it ([`crate::camoufox`], `sidecar.mjs`).
//! - x11vnc: the panel's view, on a Unix socket inside the instance HOME
//!   (`-rfbport 0 -rfbportv6 -1`: no TCP port at all); only AionCore connects.
//! - Sidecar: `node sidecar.mjs` launches Camoufox with the persistent
//!   profile and serves it on a loopback WebSocket with a random path; the
//!   endpoint is read from its first stdout line and only ever used by the
//!   bridge's Playwright filter ([`crate::pwfilter`]), never logged.
//!
//! Stop: close the sidecar's stdin (it closes the browser), wait up to 8 s,
//! then `kill` every process group as the account (root here lacks
//! CAP_KILL, see [`crate::instance`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::time::Duration;

use aionui_api_types::{BrowserEngine, BrowserErrorCode};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, watch};

use crate::camoufox::CamoufoxPaths;
use crate::error::BrowserError;
use crate::guard::NavigationGuard;
use crate::host::kill_group_as as kill_as;
use crate::instance::{allocate_display, release_display, wait_for};

/// How long the sidecar may take to print its endpoint (first start builds
/// Camoufox's font and startup caches).
const SIDECAR_START: Duration = Duration::from_secs(90);
const STOP_GRACE: Duration = Duration::from_secs(8);

/// Everything needed to launch one Camoufox instance.
#[derive(Debug, Clone)]
pub struct JugglerSpec {
    pub user_id: String,
    pub instance: u8,
    /// The engine's version directory (`engines/camoufox/<version>`).
    pub version_dir: PathBuf,
    pub profile: PathBuf,
    pub home: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub max_tabs: Arc<AtomicU8>,
    pub guard: Arc<NavigationGuard>,
}

pub struct JugglerInstance {
    pub user_id: String,
    pub instance: u8,
    pub engine: BrowserEngine,
    pub profile: PathBuf,
    pub max_tabs: Arc<AtomicU8>,
    guard: Arc<NavigationGuard>,
    uid: u32,
    gid: u32,
    display: u16,
    endpoint: String,
    vnc_socket: PathBuf,
    groups: Vec<u32>,
    sidecar_stdin: Mutex<Option<ChildStdin>>,
    sidecar: Mutex<Option<Child>>,
    /// Xvfb and x11vnc: their handles live as long as the instance
    /// (`kill_on_drop`), the kill itself is done as the account in `stop`.
    _helpers: Vec<Child>,
    _proxy: crate::proxy::EgressProxy,
    last_use: AtomicI64,
    agent_until: AtomicI64,
    exited: watch::Receiver<bool>,
}

impl std::fmt::Debug for JugglerInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the endpoint: it is the sidecar's only access check.
        f.debug_struct("JugglerInstance")
            .field("user_id", &self.user_id)
            .field("instance", &self.instance)
            .field("display", &self.display)
            .finish_non_exhaustive()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn node_binary() -> Option<PathBuf> {
    ["/usr/local/bin/node", "/usr/bin/node"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

/// The sidecar's argv JSON (no secrets in it).
pub fn sidecar_config(paths: &CamoufoxPaths, profile: &Path, home: &Path, proxy_port: u16) -> String {
    serde_json::json!({
        "executable": paths.binary,
        "profile": profile,
        "proxyPort": proxy_port,
        "downloads": home.join("Downloads"),
    })
    .to_string()
}

/// Read `{"endpoint": "ws://127.0.0.1:<port>/<path>"}`; anything else (a
/// non-loopback host) is refused.
pub fn parse_endpoint(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let endpoint = v["endpoint"].as_str()?;
    let url = url::Url::parse(endpoint).ok()?;
    (url.scheme() == "ws" && url.host_str() == Some("127.0.0.1") && url.port().is_some() && url.path().len() > 8)
        .then(|| endpoint.to_owned())
}

/// The process group of the browser the sidecar started (Playwright starts
/// it detached, so it leads its own group).
fn browser_group(uid: u32, sidecar_pid: u32) -> Option<u32> {
    crate::host::processes_of(uid)
        .into_iter()
        .find(|p| p.ppid == sidecar_pid)
        .map(|p| p.pgid)
}

/// `rm -f` run AS the account.
async fn remove_as_account(uid: u32, gid: u32, path: &Path) {
    let _ = crate::host::as_account(uid, gid, r#"rm -f -- "$1""#, &[&path.to_string_lossy()], None).await;
}

/// Connect to the Unix socket `path` that lives in a directory the browser
/// account owns, without following a link the account may have planted:
/// the directory is opened with `O_NOFOLLOW` (a symlinked `.home` fails) and
/// must be a directory owned by `uid`; the socket entry itself must be a
/// socket owned by `uid` (`fstatat` with `AT_SYMLINK_NOFOLLOW`); the connect
/// then goes through the opened directory (`/proc/self/fd/<fd>/<name>`), so
/// swapping the directory afterwards changes nothing.
pub fn connect_socket_nofollow(path: &Path, uid: u32) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let refuse = |what: &str| std::io::Error::new(std::io::ErrorKind::PermissionDenied, what.to_owned());
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(refuse("bad socket path"));
    };
    let c_dir = CString::new(dir.as_os_str().as_bytes()).map_err(|_| refuse("bad socket path"))?;
    let c_name = CString::new(name.as_bytes()).map_err(|_| refuse("bad socket path"))?;
    // SAFETY: plain libc calls on NUL-terminated strings we own; the fd is
    // wrapped in OwnedFd at once.
    let dir_fd = unsafe {
        let fd = libc::open(
            c_dir.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        OwnedFd::from_raw_fd(fd)
    };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and out-pointer.
    if unsafe { libc::fstat(dir_fd.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR || st.st_uid != uid {
        return Err(refuse("the socket directory is not the account's directory"));
    }
    // SAFETY: valid fd, NUL-terminated name, valid out-pointer.
    if unsafe { libc::fstatat(dir_fd.as_raw_fd(), c_name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFSOCK || st.st_uid != uid {
        return Err(refuse("not the account's socket"));
    }
    let via = PathBuf::from(format!("/proc/self/fd/{}", dir_fd.as_raw_fd())).join(name);
    std::os::unix::net::UnixStream::connect(via)
}

impl JugglerInstance {
    pub async fn launch(spec: JugglerSpec) -> Result<Arc<Self>, BrowserError> {
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

    async fn launch_on(spec: JugglerSpec, display: u16) -> Result<Arc<Self>, BrowserError> {
        let failed = |what: &str| BrowserError::coded(BrowserErrorCode::BrowserStartFailed, what.to_owned());
        let paths = CamoufoxPaths::new(&spec.version_dir);
        let node = node_binary().ok_or_else(|| failed("node is not installed"))?;
        let mut groups: Vec<u32> = Vec::new();
        let mut helpers: Vec<Child> = Vec::new();
        let (uid, gid) = (spec.uid, spec.gid);
        let cleanup = |groups: Vec<u32>| async move {
            for g in groups {
                kill_as(uid, gid, g, "KILL").await;
            }
        };
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
        .uid(uid)
        .gid(gid)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        let xvfb_child = xvfb.spawn().map_err(|_| failed("Xvfb did not start"))?;
        groups.extend(xvfb_child.id());
        helpers.push(xvfb_child);
        if !wait_for(Path::new(&format!("/tmp/.X11-unix/X{display}")), Duration::from_secs(8)).await {
            cleanup(groups).await;
            return Err(failed("Xvfb display did not appear"));
        }

        let Ok(proxy) = crate::proxy::EgressProxy::start(spec.guard.clone(), uid).await else {
            cleanup(groups).await;
            return Err(failed("the egress proxy did not start"));
        };

        let vnc_socket = spec.home.join("vnc.sock");
        // The HOME is the account's directory: root never deletes in it (a
        // planted `.home` symlink would redirect the delete).
        remove_as_account(uid, gid, &vnc_socket).await;
        let mut vnc = aionui_runtime::Builder::new(&paths.x11vnc);
        base_env(&mut vnc);
        vnc.env("LD_LIBRARY_PATH", &paths.vnc_libs)
            .args(["-display".into(), format!(":{display}"), "-unixsock".into()])
            .arg(&vnc_socket)
            .args([
                "-rfbport",
                "0",
                // `-rfbport 0` closes IPv4 only; without this x11vnc still
                // listens on [::1]:5900 (checked live 2026-11-17).
                "-rfbportv6",
                "-1",
                "-forever",
                "-shared",
                "-nopw",
                "-noxrecord",
                "-quiet",
                "-nossl",
            ])
            .uid(uid)
            .gid(gid)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match vnc.spawn() {
            Ok(child) => {
                groups.extend(child.id());
                helpers.push(child);
            }
            Err(_) => {
                cleanup(groups).await;
                return Err(failed("x11vnc did not start"));
            }
        }

        let mut sidecar = aionui_runtime::Builder::new(&node);
        base_env(&mut sidecar);
        sidecar
            .arg(&paths.sidecar)
            .arg(sidecar_config(&paths, &spec.profile, &spec.home, proxy.port))
            .current_dir(&spec.home)
            .uid(uid)
            .gid(gid)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = match sidecar.spawn() {
            Ok(c) => c,
            Err(_) => {
                cleanup(groups).await;
                return Err(failed("the Camoufox sidecar did not start"));
            }
        };
        let sidecar_pid = child.id();
        groups.extend(sidecar_pid);
        let stdin = child.stdin.take();
        let Some(stdout) = child.stdout.take() else {
            cleanup(groups).await;
            return Err(failed("the Camoufox sidecar has no output"));
        };
        let mut lines = BufReader::new(stdout).lines();
        let endpoint = match tokio::time::timeout(SIDECAR_START, lines.next_line()).await {
            Ok(Ok(Some(line))) => parse_endpoint(&line),
            _ => None,
        };
        let Some(endpoint) = endpoint else {
            cleanup(groups).await;
            return Err(failed("Camoufox did not start (sidecar gave no endpoint)"));
        };
        if let Some(pid) = sidecar_pid
            && let Some(g) = browser_group(uid, pid)
        {
            groups.push(g);
        }
        if !wait_for(&vnc_socket, Duration::from_secs(5)).await {
            tracing::warn!(user_id = %spec.user_id, instance = spec.instance, "camoufox: x11vnc socket missing; the panel has no view");
        }
        let (exited_tx, exited_rx) = watch::channel(false);
        let inst = Arc::new(Self {
            user_id: spec.user_id.clone(),
            instance: spec.instance,
            engine: BrowserEngine::Camoufox,
            profile: spec.profile.clone(),
            max_tabs: spec.max_tabs.clone(),
            guard: spec.guard.clone(),
            uid,
            gid,
            display,
            endpoint,
            vnc_socket,
            groups,
            sidecar_stdin: Mutex::new(stdin),
            sidecar: Mutex::new(Some(child)),
            _helpers: helpers,
            _proxy: proxy,
            last_use: AtomicI64::new(now_ms()),
            agent_until: AtomicI64::new(0),
            exited: exited_rx,
        });
        // Drain the sidecar's stdout (nothing more is expected) and watch it.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        let watched = inst.clone();
        tokio::spawn(async move {
            let child = watched.sidecar.lock().await.take();
            if let Some(mut child) = child {
                let _ = child.wait().await;
            }
            let _ = exited_tx.send(true);
        });
        // (`display` would name tracing's own `display` inside the macro.)
        let x_display = u32::from(display);
        tracing::info!(user_id = %spec.user_id, instance = spec.instance, x_display, "camoufox instance started");
        Ok(inst)
    }

    /// The sidecar's endpoint, for the bridge's filter only.
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn guard(&self) -> Arc<NavigationGuard> {
        self.guard.clone()
    }

    pub fn vnc_socket(&self) -> &Path {
        &self.vnc_socket
    }

    /// Connect to the instance's x11vnc as root without trusting the
    /// account's directory: see [`connect_socket_nofollow`].
    pub async fn connect_vnc(&self) -> std::io::Result<tokio::net::UnixStream> {
        let std_stream = connect_socket_nofollow(&self.vnc_socket, self.uid)?;
        std_stream.set_nonblocking(true)?;
        tokio::net::UnixStream::from_std(std_stream)
    }

    pub fn process_groups(&self) -> Vec<u32> {
        self.groups.clone()
    }

    pub fn is_running(&self) -> bool {
        !*self.exited.borrow()
    }

    pub async fn exited(&self) {
        let mut rx = self.exited.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn touch(&self) {
        self.last_use.store(now_ms(), Ordering::Relaxed);
    }

    pub fn idle_ms(&self) -> i64 {
        now_ms() - self.last_use.load(Ordering::Relaxed)
    }

    /// An agent used the instance (status `agent_active` for 30 s).
    pub fn mark_agent(&self) {
        self.touch();
        self.agent_until.store(now_ms() + 30_000, Ordering::Relaxed);
    }

    pub fn agent_active(&self) -> bool {
        self.agent_until.load(Ordering::Relaxed) > now_ms()
    }

    pub fn max_tabs(&self) -> u8 {
        self.max_tabs.load(Ordering::Relaxed)
    }

    /// Close the browser through the sidecar, then make sure nothing of the
    /// instance survives.
    pub async fn stop(&self) {
        // Closing stdin asks the sidecar to close Camoufox and exit.
        self.sidecar_stdin.lock().await.take();
        let _ = tokio::time::timeout(STOP_GRACE, self.exited()).await;
        for g in &self.groups {
            kill_as(self.uid, self.gid, *g, "KILL").await;
        }
        remove_as_account(self.uid, self.gid, &self.vnc_socket).await;
        release_display(self.display);
        tracing::info!(user_id = %self.user_id, instance = self.instance, "camoufox instance stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_must_be_a_loopback_websocket_with_a_path() {
        assert_eq!(
            parse_endpoint(r#"{"endpoint":"ws://127.0.0.1:33075/37116cf62509d71619b8967cd50fbe61"}"#).as_deref(),
            Some("ws://127.0.0.1:33075/37116cf62509d71619b8967cd50fbe61")
        );
        for bad in [
            r#"{"endpoint":"ws://0.0.0.0:33075/37116cf62509d71619b8967cd50fbe61"}"#,
            r#"{"endpoint":"ws://example.com:33075/37116cf62509d71619b8967cd50fbe61"}"#,
            r#"{"endpoint":"wss://127.0.0.1:33075/37116cf62509d71619b8967cd50fbe61"}"#,
            r#"{"endpoint":"ws://127.0.0.1:33075/"}"#,
            r#"{"endpoint":"/tmp/browser.sock"}"#,
            "not json",
        ] {
            assert!(parse_endpoint(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_vnc_connect_never_follows_a_planted_link() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::net::UnixListener;
        let t = tempfile::tempdir().unwrap();
        let me = std::fs::metadata(t.path()).unwrap().uid();
        let real = t.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let _listener = UnixListener::bind(real.join("vnc.sock")).unwrap();
        // The honest layout connects.
        assert!(connect_socket_nofollow(&real.join("vnc.sock"), me).is_ok());
        // `.home` replaced by a symlink to another directory: refused.
        let planted = t.path().join("home-link");
        std::os::unix::fs::symlink(&real, &planted).unwrap();
        assert!(
            connect_socket_nofollow(&planted.join("vnc.sock"), me).is_err(),
            "symlinked directory"
        );
        // The socket entry replaced by a symlink to another socket: refused.
        let dir2 = t.path().join("dir2");
        std::fs::create_dir(&dir2).unwrap();
        std::os::unix::fs::symlink(real.join("vnc.sock"), dir2.join("vnc.sock")).unwrap();
        assert!(
            connect_socket_nofollow(&dir2.join("vnc.sock"), me).is_err(),
            "symlinked socket"
        );
        // A regular file, or another owner's directory: refused.
        std::fs::write(dir2.join("file.sock"), b"x").unwrap();
        assert!(
            connect_socket_nofollow(&dir2.join("file.sock"), me).is_err(),
            "not a socket"
        );
        assert!(
            connect_socket_nofollow(&real.join("vnc.sock"), me + 1).is_err(),
            "wrong owner"
        );
    }

    #[test]
    fn sidecar_config_names_the_proxy_and_profile() {
        let paths = CamoufoxPaths::new(Path::new("/e/camoufox/v"));
        let cfg: serde_json::Value =
            serde_json::from_str(&sidecar_config(&paths, Path::new("/p"), Path::new("/p/.home"), 41000)).unwrap();
        assert_eq!(cfg["executable"], "/e/camoufox/v/camoufox-bin");
        assert_eq!(cfg["profile"], "/p");
        assert_eq!(cfg["proxyPort"], 41000);
        assert_eq!(cfg["downloads"], "/p/.home/Downloads");
    }
}
