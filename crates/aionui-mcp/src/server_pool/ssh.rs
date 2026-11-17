//! Run one command on a pool server over `ssh` (C36 §4).
//!
//! - Spawned through `aionui_runtime::Builder::clean_cli` (core/AGENTS.md),
//!   env cleared except `PATH`/`HOME`/`LANG`, stdin closed, killed on timeout.
//! - The decrypted key and a one-entry known_hosts file live in a private
//!   0700 scratch dir under the caller's root (AionUi's data dir), removed
//!   when the run ends; never in a workspace.
//! - `-F /dev/null` (no user ssh config), `IdentitiesOnly`, `IdentityAgent=none`
//!   (stored keys only), `StrictHostKeyChecking=yes` against the stored host
//!   key, `BatchMode`, no forwarding. Host/user were validated by the service
//!   (no leading '-'), and `--` ends the options anyway.
//!
//! Options verified against `ssh -G` of the installed OpenSSH 9.2p1 (exit 0
//! with all of them, 2026-11-17).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aionui_db::models::ServerRow;

/// Output kept per stream (bytes).
pub const OUTPUT_MAX: usize = 64 * 1024;
const CONNECT_TIMEOUT_SECS: u32 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOutput {
    /// `None` when killed (timeout) or ended by a signal.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration_ms: i64,
}

/// Removes the scratch dir (key included) on every exit path.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn private_dir(root: &Path) -> std::io::Result<Scratch> {
    std::fs::create_dir_all(root)?;
    let dir = root.join(format!("ssh-{}", aionui_common::generate_prefixed_id("run")));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir(&dir)?;
    Ok(Scratch(dir))
}

fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(content.as_bytes())
}

/// The known_hosts lines for `server` (`[host]:port` when not 22).
pub fn known_hosts(server: &ServerRow) -> String {
    let host = if server.port == 22 {
        server.host.clone()
    } else {
        format!("[{}]:{}", server.host, server.port)
    };
    server
        .host_key
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| format!("{host} {}\n", l.trim()))
        .collect()
}

/// The ssh arguments (no secret in them: the key is a file path).
pub fn ssh_args(server: &ServerRow, key_file: &Path, known_hosts_file: &Path, command: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["-F".into(), "/dev/null".into()];
    for opt in [
        "BatchMode=yes".to_owned(),
        "IdentitiesOnly=yes".to_owned(),
        "IdentityAgent=none".to_owned(),
        "StrictHostKeyChecking=yes".to_owned(),
        format!("UserKnownHostsFile={}", known_hosts_file.display()),
        "GlobalKnownHostsFile=/dev/null".to_owned(),
        "UpdateHostKeys=no".to_owned(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "PasswordAuthentication=no".to_owned(),
        "KbdInteractiveAuthentication=no".to_owned(),
        "ForwardAgent=no".to_owned(),
        "ForwardX11=no".to_owned(),
        "ClearAllForwardings=yes".to_owned(),
        "LogLevel=ERROR".to_owned(),
    ] {
        args.push("-o".into());
        args.push(opt);
    }
    args.extend([
        "-i".into(),
        key_file.display().to_string(),
        "-p".into(),
        server.port.to_string(),
        "-l".into(),
        server.ssh_user.clone(),
        "--".into(),
        server.host.clone(),
        command.to_owned(),
    ]);
    args
}

fn clip(bytes: &[u8]) -> String {
    let cut = &bytes[..bytes.len().min(OUTPUT_MAX)];
    let mut out = String::from_utf8_lossy(cut).into_owned();
    if bytes.len() > OUTPUT_MAX {
        out.push_str("\n[output truncated]");
    }
    out
}

/// Run `command` on `server` with `private_key`. `scratch_root` = a directory
/// only the AionUi user can read (e.g. `<data_dir>/run`).
pub async fn run_ssh(
    server: &ServerRow,
    private_key: &str,
    command: &str,
    timeout: Duration,
    scratch_root: &Path,
) -> std::io::Result<SshOutput> {
    let scratch = private_dir(scratch_root)?;
    let key_file = scratch.0.join("id");
    let known_file = scratch.0.join("known_hosts");
    write_private(&key_file, private_key)?;
    write_private(&known_file, &known_hosts(server))?;

    let mut builder = aionui_runtime::Builder::clean_cli("ssh");
    builder.env_clear();
    for name in ["PATH", "HOME", "LANG"] {
        if let Some(value) = std::env::var_os(name) {
            builder.env(name, value);
        }
    }
    builder.env("NO_COLOR", "1").env("TERM", "dumb");
    builder.args(ssh_args(server, &key_file, &known_file, command));

    let started = Instant::now();
    let result = tokio::time::timeout(timeout, builder.output()).await;
    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    drop(scratch);
    match result {
        Ok(Ok(output)) => Ok(SshOutput {
            exit_code: output.status.code(),
            stdout: clip(&output.stdout),
            stderr: clip(&output.stderr),
            timed_out: false,
            duration_ms,
        }),
        Ok(Err(e)) => Err(e),
        // `clean_cli` sets kill_on_drop: dropping the future kills ssh.
        Err(_) => Ok(SshOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: format!("timed out after {} s", timeout.as_secs()),
            timed_out: true,
            duration_ms,
        }),
    }
}

/// Why an ssh run failed, from its stderr (messages of OpenSSH 9.2p1:
/// "Host key verification failed.", "REMOTE HOST IDENTIFICATION HAS CHANGED",
/// "No ... host key is known for", "Permission denied (", "Connection
/// refused", "Could not resolve hostname", "Connection timed out").
pub fn failure_code(out: &SshOutput) -> &'static str {
    let err = out.stderr.as_str();
    if out.timed_out || err.contains("Connection timed out") || err.contains("Operation timed out") {
        "timeout"
    } else if err.contains("HOST IDENTIFICATION HAS CHANGED") || err.contains("Host key for") {
        "host_key_mismatch"
    } else if err.contains("host key is known") || err.contains("Host key verification failed") {
        "host_key_unknown"
    } else if err.contains("Permission denied") || err.contains("Too many authentication failures") {
        "auth_failed"
    } else if err.contains("Connection refused")
        || err.contains("Could not resolve hostname")
        || err.contains("No route to host")
        || err.contains("Network is unreachable")
        || err.contains("Connection closed")
        || err.contains("Connection reset")
    {
        "unreachable"
    } else {
        "failed"
    }
}

/// The public host key(s) `host:port` presents, as `<type> <base64>` lines
/// (`ssh-keyscan`, 10 s). Empty when the host gave none.
pub async fn scan_host_key(host: &str, port: u16) -> std::io::Result<String> {
    let mut builder = aionui_runtime::Builder::clean_cli("ssh-keyscan");
    builder.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        builder.env("PATH", path);
    }
    builder.args(["-T", "10", "-p", &port.to_string(), "--", host]);
    let output = tokio::time::timeout(Duration::from_secs(20), builder.output())
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "ssh-keyscan timed out"))??;
    let mut lines: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // "<host> <type> <base64>"; comment lines start with '#'.
        if fields.len() >= 3 && !fields[0].starts_with('#') {
            let entry = format!("{} {}", fields[1], fields[2]);
            if !lines.contains(&entry) {
                lines.push(entry);
            }
        }
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(port: i64) -> ServerRow {
        ServerRow {
            id: "srv_1".into(),
            user_id: "u".into(),
            name: "web-1".into(),
            host: "10.0.0.5".into(),
            port,
            ssh_user: "deploy".into(),
            host_key: "ssh-ed25519 AAAA1\nssh-rsa BBBB2".into(),
            labels: "[]".into(),
            environment: "staging".into(),
            tier: "read".into(),
            notes: String::new(),
            enabled: true,
            has_key: true,
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        }
    }

    #[test]
    fn failures_are_classified_from_stderr() {
        let out = |stderr: &str, timed_out| SshOutput {
            exit_code: Some(255),
            stdout: String::new(),
            stderr: stderr.into(),
            timed_out,
            duration_ms: 1,
        };
        assert_eq!(
            failure_code(&out("deploy@h: Permission denied (publickey).", false)),
            "auth_failed"
        );
        assert_eq!(
            failure_code(&out("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @", false)),
            "host_key_mismatch"
        );
        assert_eq!(
            failure_code(&out(
                "No ED25519 host key is known for h and you have requested strict checking.\nHost key verification failed.",
                false
            )),
            "host_key_unknown"
        );
        assert_eq!(
            failure_code(&out("ssh: connect to host h port 22: Connection refused", false)),
            "unreachable"
        );
        assert_eq!(failure_code(&out("", true)), "timeout");
        assert_eq!(failure_code(&out("something else", false)), "failed");
    }

    #[test]
    fn known_hosts_pins_host_and_port() {
        assert_eq!(
            known_hosts(&server(22)),
            "10.0.0.5 ssh-ed25519 AAAA1\n10.0.0.5 ssh-rsa BBBB2\n"
        );
        assert_eq!(
            known_hosts(&server(2222)),
            "[10.0.0.5]:2222 ssh-ed25519 AAAA1\n[10.0.0.5]:2222 ssh-rsa BBBB2\n"
        );
    }

    #[test]
    fn args_end_options_before_host_and_disable_agents() {
        let args = ssh_args(&server(22), Path::new("/d/id"), Path::new("/d/kh"), "uptime");
        let dashdash = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[dashdash + 1..], ["10.0.0.5", "uptime"]);
        for must in [
            "IdentityAgent=none",
            "StrictHostKeyChecking=yes",
            "IdentitiesOnly=yes",
            "BatchMode=yes",
        ] {
            assert!(args.iter().any(|a| a == must), "{must}");
        }
        assert_eq!(args[0..2], ["-F", "/dev/null"]);
    }

    #[tokio::test]
    async fn key_file_is_private_and_removed() {
        let root = std::env::temp_dir().join(format!("aionui-ssh-test-{}", std::process::id()));
        let scratch = private_dir(&root).unwrap();
        let key = scratch.0.join("id");
        write_private(&key, "secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(
                std::fs::metadata(&scratch.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let dir = scratch.0.clone();
        drop(scratch);
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    /// The real binary refuses an unknown host key without prompting (no
    /// network: 127.0.0.1 port 1 refuses the connection either way).
    #[tokio::test]
    async fn unreachable_server_fails_fast_without_prompting() {
        let root = std::env::temp_dir().join(format!("aionui-ssh-run-{}", std::process::id()));
        let out = run_ssh(
            &ServerRow {
                host: "127.0.0.1".into(),
                port: 1,
                ..server(1)
            },
            "-----BEGIN OPENSSH PRIVATE KEY-----\nx\n-----END OPENSSH PRIVATE KEY-----\n",
            "uptime",
            Duration::from_secs(30),
            &root,
        )
        .await
        .unwrap();
        assert!(!out.timed_out);
        assert_ne!(out.exit_code, Some(0));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0, "scratch removed");
        let _ = std::fs::remove_dir_all(root);
    }
}
