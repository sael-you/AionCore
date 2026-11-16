//! Host preparation for the unprivileged browser user (C30, decision d, owner
//! 2026-11-16). Run by AionCore before every instance start; idempotent.
//!
//! The container cannot give Chrome its own sandbox (NoNewPrivs + seccomp, no
//! user namespaces), so Chrome runs as the separate account `aionui-browser`
//! with `--no-sandbox`. That account must reach its files without being able
//! to read anything else of root's:
//! 1. `chmod o-rwx` every direct child of `/root` and `/root/.clauterm`
//!    except the two path components (`.clauterm`, `aionui-browser`);
//! 2. `chmod 711` on `/root` and `/root/.clauterm` (traverse, no listing);
//! 3. the browser root and its directories (see `layout.rs`).
//!
//! [`self_check`] refuses the start when any of those children is still
//! accessible to "other" (e.g. something re-created a 0777 directory).
//! Symlinks are skipped: their own mode is meaningless and their targets are
//! direct children themselves (e.g. `/root/.aionui-web` → `.clauterm/aionui`).

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use aionui_api_types::BrowserErrorCode;

use crate::error::BrowserError;
use crate::layout::{BROWSER_USER, BrowserLayout};

/// The two traversed directories and the name of the child each one keeps.
#[derive(Debug, Clone)]
pub struct HostPaths {
    /// `/root` and the child kept traversable (`.clauterm`).
    pub home: PathBuf,
    pub home_keep: String,
    /// `/root/.clauterm` and the child kept for the browser (`aionui-browser`).
    pub clauterm: PathBuf,
    pub clauterm_keep: String,
}

impl HostPaths {
    /// Derive from a browser root such as `/root/.clauterm/aionui-browser`.
    pub fn for_browser_root(root: &Path) -> Option<Self> {
        let clauterm = root.parent()?;
        let home = clauterm.parent()?;
        Some(Self {
            home: home.to_path_buf(),
            home_keep: clauterm.file_name()?.to_string_lossy().into_owned(),
            clauterm: clauterm.to_path_buf(),
            clauterm_keep: root.file_name()?.to_string_lossy().into_owned(),
        })
    }
}

fn other_bits(path: &Path) -> std::io::Result<Option<u32>> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(None);
    }
    Ok(Some(meta.permissions().mode() & 0o007))
}

fn children_except(dir: &Path, keep: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy() != keep {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Steps 1 and 2. Returns the paths whose mode changed (for the info log).
pub fn harden(paths: &HostPaths) -> std::io::Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for (dir, keep) in [(&paths.home, &paths.home_keep), (&paths.clauterm, &paths.clauterm_keep)] {
        for child in children_except(dir, keep)? {
            if let Some(bits) = other_bits(&child)?
                && bits != 0
            {
                let mode = std::fs::symlink_metadata(&child)?.permissions().mode() & 0o7777;
                set_mode(&child, mode & !0o007)?;
                changed.push(child);
            }
        }
    }
    for dir in [&paths.home, &paths.clauterm] {
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o7777;
        if mode != 0o711 {
            set_mode(dir, 0o711)?;
            changed.push(dir.clone());
        }
    }
    Ok(changed)
}

/// Every child "other" can still reach, plus a traversed dir that "other"
/// can list or write. Empty = safe to start.
pub fn self_check(paths: &HostPaths) -> std::io::Result<Vec<PathBuf>> {
    let mut exposed = Vec::new();
    for (dir, keep) in [(&paths.home, &paths.home_keep), (&paths.clauterm, &paths.clauterm_keep)] {
        for child in children_except(dir, keep)? {
            if other_bits(&child)?.is_some_and(|bits| bits != 0) {
                exposed.push(child);
            }
        }
        if std::fs::metadata(dir)?.permissions().mode() & 0o006 != 0 {
            exposed.push(dir.clone());
        }
    }
    Ok(exposed)
}

/// `name:x:uid:gid:…` → (uid, gid) of `user` in an `/etc/passwd` text.
pub fn parse_passwd(text: &str, user: &str) -> Option<(u32, u32)> {
    text.lines().find_map(|line| {
        let mut f = line.split(':');
        if f.next()? != user {
            return None;
        }
        let _pw = f.next()?;
        Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
    })
}

/// The browser account's (uid, gid); created when missing (`/etc` is rebuilt
/// at every container start, so this runs again after each restart).
pub async fn ensure_browser_user() -> Result<(u32, u32), BrowserError> {
    if let Some(ids) = parse_passwd(&std::fs::read_to_string("/etc/passwd")?, BROWSER_USER) {
        return Ok(ids);
    }
    let mut cmd = aionui_runtime::Builder::clean_cli("useradd");
    cmd.args([
        "--system",
        "--no-create-home",
        "--home-dir",
        "/nonexistent",
        "--shell",
        "/usr/sbin/nologin",
        BROWSER_USER,
    ]);
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(BrowserError::Internal(format!(
            "useradd {BROWSER_USER} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    tracing::info!(user = BROWSER_USER, "created the browser account");
    parse_passwd(&std::fs::read_to_string("/etc/passwd")?, BROWSER_USER)
        .ok_or_else(|| BrowserError::Internal(format!("{BROWSER_USER} missing after useradd")))
}

fn create_dir_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_mode(path, mode)
}

/// `chown -R` (no symlink following) when the top entry is not owned yet.
fn chown_tree(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

fn ensure_owned_private_dir(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    create_dir_mode(path, 0o700)?;
    let meta = std::fs::metadata(path)?;
    if meta.uid() != uid || meta.gid() != gid {
        // A rebuilt container can give the account a new uid: re-own the tree.
        chown_tree(path, uid, gid)?;
    }
    set_mode(path, 0o700)
}

/// Step 3: root dir (root, 0711), `home/` and this instance's profile (owned
/// by the browser account, 0700).
pub fn prepare_dirs(
    layout: &BrowserLayout,
    user_id: &str,
    instance: u8,
    uid: u32,
    gid: u32,
) -> Result<PathBuf, BrowserError> {
    create_dir_mode(&layout.root, 0o711)?;
    ensure_owned_private_dir(&layout.home_dir(), uid, gid)?;
    let user_dir = layout.user_dir(user_id)?;
    ensure_owned_private_dir(&user_dir, uid, gid)?;
    let profile = layout.profile_dir(user_id, instance)?;
    ensure_owned_private_dir(&profile, uid, gid)?;
    // Per-instance HOME (`.config`, NSS db, crashpad database): instances
    // share nothing, and every helper's command line names its profile.
    ensure_owned_private_dir(&instance_home(&profile), uid, gid)?;
    Ok(profile)
}

/// Harden, then verify; `BROWSER_UNAVAILABLE` (and an error log naming the
/// paths) when the host is still exposed.
pub fn harden_and_verify(paths: &HostPaths) -> Result<(), BrowserError> {
    let changed = harden(paths)?;
    if !changed.is_empty() {
        tracing::info!(
            count = changed.len(),
            "browser host: removed 'other' access from root's directories"
        );
    }
    let exposed = self_check(paths)?;
    if exposed.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = exposed.iter().map(|p| p.display().to_string()).collect();
    tracing::error!(paths = ?list, "browser host self-check failed: still accessible to other users; not starting the browser");
    Err(BrowserError::coded(
        BrowserErrorCode::BrowserUnavailable,
        "host self-check failed",
    ))
}

// ---------------------------------------------------------------------------
// Processes of the browser account (root here has no CAP_KILL: every signal
// is sent by a `kill` run AS the account, which may signal its own uid).
// ---------------------------------------------------------------------------

/// `kill -<sig> -- -<pgid>` as the browser account.
pub(crate) async fn kill_group_as(uid: u32, gid: u32, pgid: u32, signal: &str) {
    let mut cmd = aionui_runtime::Builder::clean_cli("kill");
    cmd.uid(uid)
        .gid(gid)
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{pgid}"));
    let _ = cmd.output().await;
}

/// `kill -<sig> <pid>…` as the browser account.
pub(crate) async fn kill_pids_as(uid: u32, gid: u32, pids: &[u32], signal: &str) {
    if pids.is_empty() {
        return;
    }
    let mut cmd = aionui_runtime::Builder::clean_cli("kill");
    cmd.uid(uid).gid(gid).arg(format!("-{signal}"));
    for pid in pids {
        cmd.arg(pid.to_string());
    }
    let _ = cmd.output().await;
}

/// One process of the browser account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
}

/// `(real uid, ppid, pgid)` from `/proc/<pid>/status` and `/proc/<pid>/stat`.
fn proc_identity(pid: u32) -> Option<(u32, u32, u32)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uid = status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (ppid, pgid) = parse_stat_ppid_pgid(&stat)?;
    Some((uid, ppid, pgid))
}

/// `(ppid, pgid)` (fields 4 and 5) of a `/proc/<pid>/stat` line; the command
/// name in parentheses may itself contain spaces or `)`.
pub(crate) fn parse_stat_ppid_pgid(stat: &str) -> Option<(u32, u32)> {
    let after = &stat[stat.rfind(')')? + 1..];
    // state ppid pgrp …
    let mut f = after.split_whitespace().skip(1);
    Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
}

/// Every process running as `uid`.
pub(crate) fn processes_of(uid: u32) -> Vec<ProcInfo> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let (owner, ppid, pgid) = proc_identity(pid)?;
            (owner == uid).then_some(ProcInfo { pid, ppid, pgid })
        })
        .collect()
}

/// Whether `p` belongs to a live instance: its group is a tracked group, an
/// ancestor (through the account's processes) is a tracked process, or its
/// command line names a live profile directory.
pub(crate) fn belongs_to_live(
    p: &ProcInfo,
    all: &[ProcInfo],
    keep_pids: &[u32],
    cmdline: impl Fn(u32) -> Option<String>,
    keep_profiles: &[std::path::PathBuf],
) -> bool {
    if keep_pids.contains(&p.pgid) || keep_pids.contains(&p.pid) {
        return true;
    }
    let mut cur = p.ppid;
    for _ in 0..64 {
        if keep_pids.contains(&cur) {
            return true;
        }
        match all.iter().find(|q| q.pid == cur) {
            Some(parent) => cur = parent.ppid,
            None => break,
        }
    }
    cmdline(p.pid).is_some_and(|c| {
        keep_profiles
            .iter()
            .any(|dir| names_profile(&c, &dir.to_string_lossy()))
    })
}

/// HOME of the Chrome of `profile` (inside the profile, wiped with it).
pub fn instance_home(profile: &Path) -> std::path::PathBuf {
    profile.join(".home")
}

/// `cmdline` names `dir` as a whole path: `<dir>/…` or `=<dir>` at an
/// argument end (never `<dir>2` or `<dir>-x`).
pub(crate) fn names_profile(cmdline: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    !dir.is_empty()
        && (cmdline.contains(&format!("{dir}/"))
            || cmdline.contains(&format!("={dir} "))
            || cmdline.ends_with(&format!("={dir}")))
}

fn read_cmdline(pid: u32) -> Option<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
}

/// Stop every process of the browser account that belongs to no live
/// instance (orphans of an AionCore that restarted): TERM, wait up to 5 s,
/// then KILL. Refuses uid 0. Returns how many were found.
pub async fn reap_strays(uid: u32, gid: u32, keep_pids: &[u32], keep_profiles: &[std::path::PathBuf]) -> usize {
    if uid == 0 {
        return 0;
    }
    let strays = || -> Vec<u32> {
        let all = processes_of(uid);
        all.iter()
            .filter(|p| !belongs_to_live(p, &all, keep_pids, read_cmdline, keep_profiles))
            .map(|p| p.pid)
            .collect()
    };
    let found = strays();
    if found.is_empty() {
        return 0;
    }
    tracing::info!(
        count = found.len(),
        "browser: stopping processes left by a previous AionCore"
    );
    kill_pids_as(uid, gid, &found, "TERM").await;
    for _ in 0..50 {
        if strays().is_empty() {
            return found.len();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let left = strays();
    kill_pids_as(uid, gid, &left, "KILL").await;
    found.len()
}

/// Chrome's profile locks (`Singleton{Lock,Socket,Cookie}`, symlinks).
pub(crate) fn remove_profile_locks(profile: &Path) {
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(profile.join(name));
    }
}

/// Stop the `chrome_crashpad_handler`s of the Chrome using `profile` (as the
/// account; they re-parent to init with their own group, and their database
/// lives under the instance HOME inside the profile). Polls up to 3 s.
pub(crate) async fn stop_crash_handlers(uid: u32, gid: u32, profile: &Path) {
    if uid == 0 {
        return;
    }
    let dir = profile.to_string_lossy().into_owned();
    for _ in 0..30 {
        let handlers: Vec<u32> = processes_of(uid)
            .into_iter()
            .filter(|p| {
                std::fs::read_to_string(format!("/proc/{}/comm", p.pid))
                    .is_ok_and(|c| c.trim().starts_with("chrome_crashpad"))
                    && read_cmdline(p.pid).is_some_and(|c| names_profile(&c, &dir))
            })
            .map(|p| p.pid)
            .collect();
        if !handlers.is_empty() {
            kill_pids_as(uid, gid, &handlers, "KILL").await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if handlers.is_empty()
            && !processes_of(uid).iter().any(|p| {
                std::fs::read_to_string(format!("/proc/{}/comm", p.pid))
                    .is_ok_and(|c| c.trim().starts_with("chrome_crashpad"))
            })
        {
            return;
        }
    }
}

/// `user_experience_metrics.reporting_enabled = false` in the profile's
/// Local State (merged; the file stays the account's).
pub(crate) fn disable_metrics_reporting(profile: &Path, uid: u32, gid: u32) {
    let path = profile.join("Local State");
    let mut state: serde_json::Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    state["user_experience_metrics"]["reporting_enabled"] = serde_json::json!(false);
    if std::fs::write(&path, state.to_string()).is_ok() {
        let _ = std::os::unix::fs::lchown(&path, Some(uid), Some(gid));
    }
}

/// The browser account's ids if it exists (never creates it).
pub fn existing_browser_user() -> Option<(u32, u32)> {
    parse_passwd(&std::fs::read_to_string("/etc/passwd").ok()?, BROWSER_USER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    /// A fake `/root` with a `.clauterm` holding the browser root, a 0777 dir
    /// (like `.token-pool`), a world-readable file and a symlink.
    fn fake_host() -> (tempfile::TempDir, HostPaths, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("root");
        let clauterm = home.join(".clauterm");
        let browser = clauterm.join("aionui-browser");
        std::fs::create_dir_all(&browser).unwrap();
        std::fs::create_dir_all(home.join(".token-pool")).unwrap();
        set_mode(&home.join(".token-pool"), 0o777).unwrap();
        std::fs::write(home.join("notes.txt"), "x").unwrap();
        set_mode(&home.join("notes.txt"), 0o644).unwrap();
        std::fs::create_dir_all(clauterm.join("aionui")).unwrap();
        set_mode(&clauterm.join("aionui"), 0o755).unwrap();
        std::os::unix::fs::symlink(clauterm.join("aionui"), home.join(".aionui-web")).unwrap();
        set_mode(&home, 0o755).unwrap();
        set_mode(&clauterm, 0o755).unwrap();
        let paths = HostPaths::for_browser_root(&browser).unwrap();
        (t, paths, browser)
    }

    #[test]
    fn harden_closes_other_access_and_keeps_the_path_traversable() {
        let (_t, paths, browser) = fake_host();
        assert_eq!(paths.home_keep, ".clauterm");
        assert_eq!(paths.clauterm_keep, "aionui-browser");
        assert!(!self_check(&paths).unwrap().is_empty(), "exposed before hardening");

        harden(&paths).unwrap();
        assert_eq!(mode(&paths.home), 0o711);
        assert_eq!(mode(&paths.clauterm), 0o711);
        assert_eq!(mode(&paths.home.join(".token-pool")), 0o770);
        assert_eq!(mode(&paths.home.join("notes.txt")), 0o640);
        assert_eq!(mode(&paths.clauterm.join("aionui")), 0o750);
        assert!(
            std::fs::symlink_metadata(paths.home.join(".aionui-web"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(browser.exists());
        assert!(self_check(&paths).unwrap().is_empty());
        assert!(harden(&paths).unwrap().is_empty(), "idempotent: nothing left to change");
    }

    #[test]
    fn self_check_names_a_re_exposed_child_and_refuses() {
        let (_t, paths, _browser) = fake_host();
        harden_and_verify(&paths).unwrap();
        set_mode(&paths.clauterm.join("aionui"), 0o755).unwrap();
        assert_eq!(self_check(&paths).unwrap(), vec![paths.clauterm.join("aionui")]);
        set_mode(&paths.home, 0o755).unwrap();
        assert!(
            self_check(&paths).unwrap().contains(&paths.home),
            "a listable /root is exposed too"
        );
    }

    #[test]
    fn stat_fields_survive_odd_command_names() {
        assert_eq!(parse_stat_ppid_pgid("123 (chrome) S 1 456 456 0 -1"), Some((1, 456)));
        assert_eq!(
            parse_stat_ppid_pgid("123 (we ird) name)) S 7 789 789 0"),
            Some((7, 789))
        );
        assert_eq!(parse_stat_ppid_pgid("garbage"), None);
    }

    #[test]
    fn this_process_is_found_by_its_uid() {
        let me = std::process::id();
        let uid = unsafe { libc::getuid() };
        assert!(processes_of(uid).iter().any(|p| p.pid == me));
    }

    /// Instance 1 = chrome 100 (group 100) with a renderer 101 and a helper
    /// 102 that left the group but descends from 100, a crashpad 103 that
    /// was re-parented to init but names the profile; 200 + 201 belong to a
    /// dead instance. Only the dead ones are strays.
    #[test]
    fn live_instance_processes_are_kept_and_orphans_are_not() {
        let p = |pid, ppid, pgid| ProcInfo { pid, ppid, pgid };
        let all = vec![
            p(100, 1, 100),
            p(101, 100, 100),
            p(102, 100, 102),
            p(103, 1, 103),
            p(200, 1, 200),
            p(201, 200, 200),
        ];
        let live = [std::path::PathBuf::from("/b/u/1")];
        let cmd = |pid: u32| match pid {
            103 => Some("chrome_crashpad_handler --database=/b/u/1/Crashpad".to_owned()),
            201 => Some("chrome --user-data-dir=/b/u/2".to_owned()),
            _ => None,
        };
        let kept: Vec<u32> = all
            .iter()
            .filter(|x| belongs_to_live(x, &all, &[100], cmd, &live))
            .map(|x| x.pid)
            .collect();
        assert_eq!(kept, vec![100, 101, 102, 103]);
    }

    #[test]
    fn profile_names_match_whole_paths_only() {
        let crashpad = "chrome_crashpad_handler --database=/b/u/1/.home/.config/google-chrome/Crash Reports --url=x";
        assert!(names_profile(crashpad, "/b/u/1"));
        assert!(!names_profile(crashpad, "/b/u/2"));
        assert!(names_profile("chrome --user-data-dir=/b/u/1 --no-sandbox", "/b/u/1"));
        assert!(names_profile("chrome --user-data-dir=/b/u/1", "/b/u/1"));
        assert!(!names_profile("chrome --user-data-dir=/b/u/12", "/b/u/1"));
        assert!(!names_profile("chrome --user-data-dir=/b/u/1-x", "/b/u/1"));
    }

    #[test]
    fn metrics_reporting_is_turned_off_and_other_state_kept() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("Local State"), r#"{"browser":{"x":1}}"#).unwrap();
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        disable_metrics_reporting(t.path(), uid, gid);
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(t.path().join("Local State")).unwrap()).unwrap();
        assert_eq!(v["user_experience_metrics"]["reporting_enabled"], false);
        assert_eq!(v["browser"]["x"], 1);
    }

    #[tokio::test]
    async fn reaping_never_touches_root() {
        assert_eq!(reap_strays(0, 0, &[], &[]).await, 0);
    }

    #[test]
    fn passwd_lookup() {
        let text = "root:x:0:0:root:/root:/bin/bash\naionui-browser:x:999:996::/nonexistent:/usr/sbin/nologin\n";
        assert_eq!(parse_passwd(text, "aionui-browser"), Some((999, 996)));
        assert_eq!(parse_passwd(text, "aionui"), None);
    }

    #[test]
    fn prepare_dirs_creates_the_private_tree() {
        let t = tempfile::tempdir().unwrap();
        let layout = BrowserLayout::new(t.path().join("aionui-browser"), t.path().join("data"));
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let profile = prepare_dirs(&layout, "u1", 2, uid, gid).unwrap();
        assert_eq!(profile, layout.root.join("u1/2"));
        assert_eq!(mode(&layout.root), 0o711);
        for p in [layout.home_dir(), layout.root.join("u1"), profile] {
            assert_eq!(mode(&p), 0o700, "{}", p.display());
        }
    }
}
