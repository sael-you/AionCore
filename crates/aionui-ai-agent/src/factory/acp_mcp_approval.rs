//! TASK-97 on the ACP lane: MCP servers whose every call needs the user's OK (a
//! production write market instance) must make the CLI ask, whatever its
//! approval mode. Only gemini can receive market instances here (market CLIs are
//! claude, codex and gemini; claude/codex take the direct-CLI path), so gemini is
//! the one vendor with a mechanism; any other ACP vendor loses such a server
//! (fail closed) instead of running it unattended.
//!
//! gemini's mechanism is its policy engine. Verified against the installed
//! gemini-cli 0.68.0 bundle (`/usr/local/lib/node_modules/@google/gemini-cli/bundle/`):
//! - `--admin-policy` "Additional admin policy files or directories to load"
//!   (`gemini --help`); `getPolicyDirectories` ADDS those paths to the system,
//!   user, workspace and default policy dirs, while `--policy` would REPLACE the
//!   user's own policy dir. `getPolicyTier` puts an admin path in
//!   `ADMIN_POLICY_TIER = 5`, priority `5 + priority/1000`.
//! - YOLO's allow-all rule is default tier, priority 998 → 1.998
//!   (`policies/yolo.toml`, `PRIORITY_YOLO_ALLOW_ALL`); "Always allow" answers
//!   land at `WORKSPACE_POLICY_TIER + …` (3.x). An admin `ask_user` rule at 999
//!   (5.999) outranks both.
//! - `PolicyRuleSchema` requires `toolName`; with `mcpName` set, `toolName = "*"`
//!   becomes `mcp_<server>_*` (`formatMcpToolName`), which `matchesWildcard`
//!   matches against the server name and the `mcp_<server>_` prefix every MCP
//!   tool gets from `generateValidName`.
//! - ACP mode is interactive (`--acp` → `interactive`), so `ask_user` turns into
//!   an ACP `session/request_permission` rather than a deny.
//! - `createPolicyEngineConfig` IGNORES `--admin-policy` when the system policies
//!   dir already holds a `.toml` file: then the rule cannot be installed and the
//!   servers are dropped.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::McpServer;

const GEMINI_BACKEND: &str = "gemini";
const ADMIN_POLICY_FLAG: &str = "--admin-policy";
/// Under `{data_dir}/session-runtime` (not swept at startup: the sweep only
/// removes `claude-mcp-*` directories). One file per conversation, rewritten on
/// every agent build.
const POLICY_SUBDIR: &str = "gemini-policies";

/// gemini's system policies dir (`Storage.getSystemConfigDir()` + `policies`).
fn gemini_system_policies_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/GeminiCli/policies")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\ProgramData\gemini-cli\policies")
    } else {
        PathBuf::from("/etc/gemini-cli/policies")
    }
}

/// Extra launch args that make `backend` ask before every call to `servers`, or
/// `Err(reason)` when that cannot be guaranteed (the caller then drops them).
pub(crate) fn approval_launch_args(
    backend: Option<&str>,
    conversation_id: &str,
    data_dir: &Path,
    servers: &[String],
) -> Result<Vec<String>, String> {
    approval_launch_args_with(
        backend,
        conversation_id,
        data_dir,
        servers,
        &gemini_system_policies_dir(),
    )
}

fn approval_launch_args_with(
    backend: Option<&str>,
    conversation_id: &str,
    data_dir: &Path,
    servers: &[String],
    system_policies_dir: &Path,
) -> Result<Vec<String>, String> {
    if servers.is_empty() {
        return Ok(Vec::new());
    }
    if backend != Some(GEMINI_BACKEND) {
        return Err(format!(
            "ACP backend {backend:?} has no verified ask-before-call mechanism"
        ));
    }
    if has_toml_file(system_policies_dir) {
        return Err(format!(
            "gemini ignores --admin-policy while {} holds policies",
            system_policies_dir.display()
        ));
    }
    let path = policy_file_path(data_dir, conversation_id);
    write_policy_file(&path, &gemini_policy_toml(servers)).map_err(|e| format!("policy file not written: {e}"))?;
    Ok(vec![ADMIN_POLICY_FLAG.to_owned(), path.to_string_lossy().into_owned()])
}

/// One admin-tier `ask_user` rule per server, matching every tool it offers.
fn gemini_policy_toml(servers: &[String]) -> String {
    let mut out = String::from("# Written by AionUi (TASK-97): every call to these MCP servers needs the user's OK.\n");
    for server in servers {
        // A JSON string literal is a valid TOML basic string (same escapes).
        let name = serde_json::Value::String(server.clone()).to_string();
        out.push_str(&format!(
            "\n[[rule]]\ntoolName = \"*\"\nmcpName = {name}\ndecision = \"ask_user\"\npriority = 999\n"
        ));
    }
    out
}

fn policy_file_path(data_dir: &Path, conversation_id: &str) -> PathBuf {
    let safe: String = conversation_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    aionui_session::session_scratch_root(data_dir)
        .join(POLICY_SUBDIR)
        .join(format!("{safe}.toml"))
}

fn has_toml_file(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
    })
}

/// Write via a fresh sibling + rename, so a reader never sees half a file and a
/// planted symlink at `path` is replaced rather than followed.
fn write_policy_file(path: &Path, body: &str) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no parent dir"))?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut file| std::io::Write::write_all(&mut file, body.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The server name of an SDK entry (`None` for a variant this code never builds).
pub(crate) fn sdk_server_name(server: &McpServer) -> Option<&str> {
    match server {
        McpServer::Stdio(s) => Some(&s.name),
        McpServer::Http(h) => Some(&h.name),
        McpServer::Sse(s) => Some(&s.name),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn nothing_to_confirm_adds_no_args_for_any_backend() {
        let dir = tempfile::tempdir().unwrap();
        for backend in [Some("gemini"), Some("qwen"), None] {
            assert_eq!(
                approval_launch_args_with(backend, "c1", dir.path(), &[], &dir.path().join("none")).unwrap(),
                Vec::<String>::new()
            );
        }
    }

    #[test]
    fn gemini_gets_an_admin_policy_that_asks_for_each_server() {
        let data = tempfile::tempdir().unwrap();
        let system = data.path().join("etc-policies-missing");
        let args = approval_launch_args_with(
            Some("gemini"),
            "conv/1",
            data.path(),
            &names(&["paddle-live", "stripe-live"]),
            &system,
        )
        .unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--admin-policy");
        let path = PathBuf::from(&args[1]);
        assert_eq!(
            path,
            data.path().join("session-runtime/gemini-policies/conv_1.toml"),
            "per-conversation file, path-safe name"
        );
        let body = std::fs::read_to_string(&path).unwrap();
        let rule = |server: &str| {
            format!("\n[[rule]]\ntoolName = \"*\"\nmcpName = \"{server}\"\ndecision = \"ask_user\"\npriority = 999\n")
        };
        assert_eq!(
            body,
            format!(
                "# Written by AionUi (TASK-97): every call to these MCP servers needs the user's OK.\n{}{}",
                rule("paddle-live"),
                rule("stripe-live")
            ),
            "one admin ask_user rule per server, no `modes` (so yolo is covered too)"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // A rebuild rewrites the same file with the current set.
        approval_launch_args_with(Some("gemini"), "conv/1", data.path(), &names(&["paddle-live"]), &system).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.matches("[[rule]]").count(), 1);
    }

    #[test]
    fn other_acp_backends_and_shadowing_system_policies_fail_closed() {
        let data = tempfile::tempdir().unwrap();
        let servers = names(&["paddle-live"]);
        let missing = data.path().join("missing");
        for backend in [Some("qwen"), Some("codebuddy"), None] {
            assert!(approval_launch_args_with(backend, "c1", data.path(), &servers, &missing).is_err());
        }
        let system = data.path().join("system");
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(system.join("org.toml"), "").unwrap();
        let err = approval_launch_args_with(Some("gemini"), "c1", data.path(), &servers, &system).unwrap_err();
        assert!(err.contains("ignores --admin-policy"), "{err}");
    }
}
