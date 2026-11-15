//! `aioncore mcp exec <id> (--project <pid> [--assistant <aid>] | --global)
//! --cli <cli> [--data-dir <dir>]` — the MCP market launcher (R9, Review R1
//! #1–#3, #8). Invoked by the native CLI configs `studio sync` writes and by
//! the AionUi assistant channel (`mcp_resolve`).
//!
//! - Data dir: `AIONUI_DATA_DIR`, else `--data-dir` (absolute).
//! - DB: opened read-only (no create, no migrator), migrations 904/905 required.
//! - Decision from the DB now: market row, not deleted, `active`, CLI allowed,
//!   exact assignment; the template's pinned command (never caller-supplied).
//! - Command: `<data_dir>/toolchain/bin/<cmd>` first (G4, e.g. uvx), else PATH.
//! - Child env: `env_clear()` + allowlist + plain env + this instance's
//!   declared secrets; never `AIONUI_*` (incl. the runtime token).
//! - stdio proxy filtering `tools/list` / `tools/call`; re-checks the launch
//!   before tool calls and every 30 s, and stops when it no longer holds.
//! - Never logs env or secret values. stdout is the MCP channel: diagnostics
//!   go to stderr only.
//!
//! Honest limit: a correctness gate, not a security boundary against a root
//! agent (it can read the DB and its key, and `/proc/<pid>/environ`).

use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use aionui_app::derive_encryption_key;
use aionui_auth::resolve_encryption_secret;
use aionui_db::{IUserRepository, SqliteMcpMarketRepository, SqliteUserRepository, open_database_read_only};
use aionui_mcp::market::{
    LaunchRequest, LaunchScope, MarketError, ProxyExit, child_env, resolve_launch, resolve_toolchain_command, run_stdio_proxy,
    still_allowed,
};

use crate::cli::{McpArgs, McpCommand, McpExecArgs};

const PREFIX: &str = "aioncore mcp exec";
/// Migrations the launcher's queries need.
const REQUIRED_MIGRATIONS: &[i64] = &[904, 905];
const RECHECK_EVERY: Duration = Duration::from_secs(30);
const SYSTEM_USER: &str = "system_default_user";

const EXIT_USAGE: u8 = 2;
const EXIT_REFUSED: u8 = 3;
const EXIT_INTERNAL: u8 = 4;

pub async fn run_mcp(args: McpArgs) -> ExitCode {
    match args.command {
        McpCommand::Exec(args) => run_exec(args).await,
    }
}

fn fail(code: u8, msg: &str) -> ExitCode {
    eprintln!("{PREFIX}: {msg}");
    ExitCode::from(code)
}

/// `AIONUI_DATA_DIR` (non-empty) wins over `--data-dir`; both must be absolute.
pub(crate) fn resolve_data_dir(args: &McpExecArgs) -> Option<PathBuf> {
    std::env::var_os("AIONUI_DATA_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| args.data_dir.clone())
        .filter(|p| p.is_absolute())
}

/// The storage-encryption key, resolved like the server does
/// (`AIONUI_ENCRYPTION_SECRET` env → users.encryption_secret → seeded from the
/// JWT secret), read-only.
async fn encryption_key(db: &aionui_db::Database) -> Result<[u8; 32], String> {
    let users = SqliteUserRepository::new(db.pool().clone());
    let user = users
        .get_system_user()
        .await
        .map_err(|_| "cannot read the system user".to_owned())?;
    let env_secret = std::env::var("AIONUI_ENCRYPTION_SECRET").ok();
    let db_secret = user.as_ref().and_then(|u| u.encryption_secret.clone());
    let seed = user.as_ref().and_then(|u| u.jwt_secret.clone()).unwrap_or_default();
    let (secret, _) = resolve_encryption_secret(env_secret.as_deref(), db_secret.as_deref(), &seed);
    if secret.is_empty() {
        return Err("no storage-encryption secret is provisioned".to_owned());
    }
    Ok(derive_encryption_key(&secret))
}

/// Owned copy of the launch request for the proxy's re-check closure.
struct OwnedRequest {
    user_id: String,
    instance: String,
    project: Option<String>,
    assistant: Option<String>,
    cli: String,
}

impl OwnedRequest {
    fn borrow(&self) -> LaunchRequest<'_> {
        LaunchRequest {
            user_id: &self.user_id,
            instance: &self.instance,
            scope: match self.project.as_deref() {
                Some(project_id) => LaunchScope::Project {
                    project_id,
                    assistant_id: self.assistant.as_deref(),
                },
                None => LaunchScope::Global,
            },
            cli: &self.cli,
        }
    }
}

fn init_stderr_logging() {
    // stdout carries MCP frames; warnings go to stderr (the CLI's MCP log).
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
        .try_init();
}

async fn run_exec(args: McpExecArgs) -> ExitCode {
    init_stderr_logging();
    let Some(data_dir) = resolve_data_dir(&args) else {
        return fail(
            EXIT_USAGE,
            "no absolute data dir: set AIONUI_DATA_DIR or pass --data-dir <abs path>",
        );
    };
    let db_path = data_dir.join("aionui-backend.db");
    let db = match open_database_read_only(&db_path, REQUIRED_MIGRATIONS).await {
        Ok(db) => db,
        Err(e) => return fail(EXIT_REFUSED, &format!("cannot open the AionUi database read-only: {e}")),
    };
    let key = match encryption_key(&db).await {
        Ok(k) => k,
        Err(e) => return fail(EXIT_INTERNAL, &e),
    };
    let request = Arc::new(OwnedRequest {
        user_id: std::env::var("AIONUI_USER_ID")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| SYSTEM_USER.to_owned()),
        instance: args.instance.clone(),
        project: args.project.clone(),
        assistant: args.assistant.clone(),
        cli: args.cli.clone(),
    });
    let repo = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let plan = match resolve_launch(repo.as_ref(), &key, &request.borrow()).await {
        Ok(plan) => plan,
        Err(MarketError::Forbidden(reason)) => return fail(EXIT_REFUSED, &format!("refused: {reason}")),
        Err(e) => return fail(EXIT_INTERNAL, &e.to_string()),
    };

    // G4: a pinned runtime in `<data_dir>/toolchain/bin` (e.g. uvx) wins over PATH.
    let toolchain = resolve_toolchain_command(&plan.command, &data_dir, std::env::var_os("PATH").as_deref());
    let program = toolchain
        .as_ref()
        .map(|t| t.program.to_string_lossy().into_owned())
        .unwrap_or_else(|| plan.command.clone());
    let resolved = match aionui_runtime::ensure_runtime_command(&program).await {
        Ok(r) => r,
        Err(e) => return fail(EXIT_INTERNAL, &format!("cannot resolve the server command: {e}")),
    };
    let mut runtime_env: Vec<(String, String)> = resolved
        .env
        .iter()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .collect();
    if let Some(t) = toolchain {
        runtime_env.extend(t.env);
    }
    let mut builder = aionui_runtime::Builder::from_resolved(&resolved);
    builder
        .env_clear()
        .envs(child_env(std::env::vars(), &runtime_env, &plan))
        .args(&plan.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_dir()) {
        builder.current_dir(home);
    }
    let mut child = match builder.spawn() {
        Ok(c) => c,
        Err(e) => return fail(EXIT_INTERNAL, &format!("cannot start {}: {e}", plan.instance_name)),
    };
    let (Some(child_in), Some(child_out)) = (child.stdin.take(), child.stdout.take()) else {
        return fail(EXIT_INTERNAL, "child stdio not piped");
    };

    let gate_repo = repo.clone();
    let gate_req = request.clone();
    let gate = move || {
        let repo = gate_repo.clone();
        let req = gate_req.clone();
        async move { still_allowed(repo.as_ref(), &req.borrow()).await }
    };
    let exit = run_stdio_proxy(
        tokio::io::stdin(),
        tokio::io::stdout(),
        child_out,
        child_in,
        plan.policy.clone(),
        gate,
        RECHECK_EVERY,
    )
    .await;

    finish(&mut child, exit, &plan.instance_name).await
}

async fn finish(child: &mut tokio::process::Child, exit: std::io::Result<ProxyExit>, instance: &str) -> ExitCode {
    match exit {
        Ok(ProxyExit::ServerClosed) => match child.wait().await {
            Ok(status) => ExitCode::from(status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1)),
            Err(_) => ExitCode::from(1),
        },
        Ok(ProxyExit::ClientClosed) => {
            // Give the server a moment to exit on stdin EOF, then stop it.
            if tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_err()
            {
                let _ = aionui_runtime::kill_process_tree(child).await;
            }
            ExitCode::SUCCESS
        }
        Ok(ProxyExit::Revoked) => {
            let _ = aionui_runtime::kill_process_tree(child).await;
            fail(
                EXIT_REFUSED,
                &format!("stopped: {instance} is no longer active or assigned"),
            )
        }
        Err(e) => {
            let _ = aionui_runtime::kill_process_tree(child).await;
            fail(EXIT_INTERNAL, &format!("stdio proxy failed: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(data_dir: Option<&str>) -> McpExecArgs {
        McpExecArgs {
            instance: "x".into(),
            project: Some("p".into()),
            global: false,
            assistant: None,
            cli: "claude".into(),
            data_dir: data_dir.map(PathBuf::from),
        }
    }

    #[test]
    fn relative_data_dir_is_refused() {
        // AIONUI_DATA_DIR is not set in the test environment for this crate.
        if std::env::var_os("AIONUI_DATA_DIR").is_none() {
            assert_eq!(resolve_data_dir(&args(Some("data"))), None);
            assert_eq!(resolve_data_dir(&args(Some("/abs"))), Some(PathBuf::from("/abs")));
            assert!(std::path::Path::new("/abs").is_absolute());
        }
    }
}
