//! `aioncore mcp servers --channel standard|crucial --conversation <id>
//! [--user <id>] [--data-dir <dir>]` — the built-in server-pool MCP (card
//! C36, TASK-130), one stdio MCP server per channel per conversation:
//!
//! - `aionui-servers` (standard): routine commands run; a crucial command on a
//!   production write server is not run and the agent is sent to the crucial
//!   channel.
//! - `aionui-servers-crucial`: registered with `require_approval`, so the CLI
//!   asks the owner before every call (TASK-97 ask path); the agent may also
//!   use it to declare a command crucial itself.
//!
//! Every call re-reads the conversation's grant (kit levels), the server and
//! its key from the database (assignment changes apply at the next call) and
//! appends one `server_audit` row. Stdout is the MCP channel; diagnostics go
//! to stderr only and never carry a key or a command's output.
//!
//! Honest limit (same as `mcp exec`): a guard rail for agents, not a security
//! boundary against a root process that can read the DB and its key.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use aionui_db::{
    IServerPoolRepository, ServerAuditParams, SqliteKitRepository, SqliteServerPoolRepository, open_database_existing,
};
use aionui_mcp::server_pool::{
    ExecChannel, ExecDecision, ServerPoolService, decide, granted_servers, redact_command, run_ssh, server_response,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::Deserialize;

use crate::cli::McpServersArgs;

const PREFIX: &str = "aioncore mcp servers";
/// Kits (907) for the grant, the server pool (910) for servers and audit.
const REQUIRED_MIGRATIONS: &[i64] = &[907, 910];
const SYSTEM_USER: &str = "system_default_user";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

const EXIT_USAGE: u8 = 2;
const EXIT_REFUSED: u8 = 3;
const EXIT_INTERNAL: u8 = 4;

fn fail(code: u8, msg: &str) -> ExitCode {
    eprintln!("{PREFIX}: {msg}");
    ExitCode::from(code)
}

pub async fn run_servers(args: McpServersArgs) -> ExitCode {
    let Some(channel) = ExecChannel::parse(&args.channel) else {
        return fail(EXIT_USAGE, "--channel must be standard or crucial");
    };
    let Some(data_dir) = super::cmd_mcp::data_dir_from(args.data_dir.as_ref()) else {
        return fail(
            EXIT_USAGE,
            "no absolute data dir: set AIONUI_DATA_DIR or pass --data-dir <abs path>",
        );
    };
    if args.conversation.trim().is_empty() {
        return fail(EXIT_USAGE, "--conversation must not be empty");
    }
    let db = match open_database_existing(&data_dir.join("aionui-backend.db"), REQUIRED_MIGRATIONS).await {
        Ok(db) => db,
        Err(e) => return fail(EXIT_REFUSED, &format!("cannot open the AionUi database: {e}")),
    };
    let key = match super::cmd_mcp::encryption_key(&db).await {
        Ok(k) => k,
        Err(e) => return fail(EXIT_INTERNAL, &e),
    };
    let user_id = args
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var("AIONUI_USER_ID").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| SYSTEM_USER.to_owned());
    let pool_repo: Arc<dyn IServerPoolRepository> = Arc::new(SqliteServerPoolRepository::new(db.pool().clone()));
    let server = ServersMcp {
        channel,
        user_id,
        conversation_id: args.conversation.clone(),
        kits: Arc::new(SqliteKitRepository::new(db.pool().clone())),
        pool: ServerPoolService::new(pool_repo, key),
        scratch_root: data_dir.join("run").join("servers"),
    };
    match server.serve(transport::io::stdio()).await {
        Ok(peer) => match peer.waiting().await {
            Ok(_) => ExitCode::SUCCESS,
            Err(_) => fail(EXIT_INTERNAL, "MCP stdio session ended with an error"),
        },
        Err(_) => fail(EXIT_INTERNAL, "failed to start the MCP stdio server"),
    }
}

#[derive(Clone)]
struct ServersMcp {
    channel: ExecChannel,
    user_id: String,
    conversation_id: String,
    kits: Arc<SqliteKitRepository>,
    pool: ServerPoolService,
    scratch_root: PathBuf,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListServersParams {}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunCommandParams {
    /// Server name from list_servers.
    server: String,
    /// Shell command run by the server's login shell.
    command: String,
    /// Seconds before the command is killed (default 120, max 600).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

fn text(body: String) -> CallToolResult {
    CallToolResult::success(vec![Content::text(body)])
}

fn error(body: String) -> CallToolResult {
    CallToolResult::error(vec![Content::text(body)])
}

#[tool_router]
impl ServersMcp {
    #[tool(
        name = "list_servers",
        description = "List the SSH servers assigned to this conversation: name, host, user, environment (production/staging/sandbox/local), tier (read/write), labels and the owner's notes. The tier is GUIDANCE from the owner, not a lock: on a read server do not change anything; follow the project's rules. On production write servers, crucial commands (destructive file operations, service restarts, database writes, package removal or upgrade, firewall/network, user or key changes, force pushes, unknown scripts) must be sent to the aionui-servers-crucial server's run_command, which asks the owner first."
    )]
    async fn list_servers(&self, Parameters(_): Parameters<ListServersParams>) -> CallToolResult {
        match granted_servers(
            self.kits.as_ref(),
            self.pool.repo().as_ref(),
            &self.user_id,
            &self.conversation_id,
        )
        .await
        {
            Ok(rows) => {
                let servers: Vec<serde_json::Value> = rows
                    .iter()
                    .map(server_response)
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "host": s.host,
                            "port": s.port,
                            "user": s.ssh_user,
                            "environment": s.environment,
                            "tier": s.tier,
                            "labels": s.labels,
                            "notes": s.notes,
                            "enabled": s.enabled,
                            "crucial_commands_ask_owner": s.crucial_commands_ask,
                        })
                    })
                    .collect();
                text(
                    serde_json::to_string_pretty(&serde_json::json!({
                        "servers": servers,
                        "tier_note": aionui_api_types::SERVER_TIER_NOTE,
                    }))
                    .unwrap_or_default(),
                )
            }
            Err(e) => {
                eprintln!("{PREFIX}: list_servers failed: {e}");
                error("cannot read the servers of this conversation".to_owned())
            }
        }
    }

    #[tool(
        name = "run_command",
        description = "Run one shell command on an assigned server over SSH and return its exit code, stdout and stderr (64 KB each). Every call is audited. On aionui-servers, a crucial command on a production write server is NOT run: send the same call to aionui-servers-crucial instead. On aionui-servers-crucial the owner approves every call before it runs; use it also when you judge a command crucial yourself."
    )]
    async fn run_command(&self, Parameters(p): Parameters<RunCommandParams>) -> CallToolResult {
        self.run(p).await
    }
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for ServersMcp {}

impl ServersMcp {
    async fn audit(
        &self,
        server_id: &str,
        server_name: &str,
        decision: &str,
        reason: &str,
        command: &str,
        exit: Option<i64>,
        ms: Option<i64>,
    ) {
        let redacted = redact_command(command);
        let result = self
            .pool
            .repo()
            .insert_server_audit(&ServerAuditParams {
                user_id: &self.user_id,
                conversation_id: &self.conversation_id,
                server_id,
                server_name,
                channel: self.channel.as_str(),
                decision,
                reason,
                command: &redacted,
                exit_code: exit,
                duration_ms: ms,
            })
            .await;
        if let Err(e) = result {
            eprintln!("{PREFIX}: audit write failed: {e}");
        }
    }

    async fn run(&self, p: RunCommandParams) -> CallToolResult {
        let granted = match granted_servers(
            self.kits.as_ref(),
            self.pool.repo().as_ref(),
            &self.user_id,
            &self.conversation_id,
        )
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("{PREFIX}: grant lookup failed: {e}");
                return error("cannot read the servers of this conversation".to_owned());
            }
        };
        let name = p.server.trim();
        let granted_row = granted.iter().find(|s| s.name == name);
        // An ungranted (but existing) server is still named in the refusal.
        let row = match granted_row {
            Some(r) => Some(r.clone()),
            None => self
                .pool
                .repo()
                .find_server_by_name(&self.user_id, name)
                .await
                .ok()
                .flatten(),
        };
        let decision = decide(self.channel, row.as_ref(), granted_row.is_some(), &p.command);
        let (server_id, server_name) = row
            .as_ref()
            .map(|r| (r.id.clone(), r.name.clone()))
            .unwrap_or_else(|| (String::new(), name.to_owned()));
        let crucial = match decision {
            ExecDecision::Run { crucial } => crucial,
            ExecDecision::NeedsApproval(ref msg) | ExecDecision::Refuse(ref msg) => {
                self.audit(
                    &server_id,
                    &server_name,
                    decision.audit_label(),
                    msg,
                    &p.command,
                    None,
                    None,
                )
                .await;
                return error(msg.clone());
            }
        };
        let Some(server) = row else {
            return error("unknown server".to_owned());
        };
        let key = match self.pool.private_key(&self.user_id, &server.id).await {
            Ok(Some(k)) => k,
            Ok(None) => return error(format!("server {} has no private key stored", server.name)),
            Err(_) => {
                self.audit(
                    &server.id,
                    &server.name,
                    "failed",
                    "cannot decrypt the server key",
                    &p.command,
                    None,
                    None,
                )
                .await;
                return error("cannot read the server key".to_owned());
            }
        };
        let timeout = Duration::from_secs(
            p.timeout_secs
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        );
        let reason = crucial.unwrap_or_default();
        match run_ssh(&server, &key, &p.command, timeout, &self.scratch_root).await {
            Ok(out) => {
                let label = if out.exit_code == Some(0) { "ran" } else { "failed" };
                let audit_reason = if out.timed_out {
                    format!(
                        "timed out after {} s{}",
                        timeout.as_secs(),
                        if reason.is_empty() {
                            String::new()
                        } else {
                            format!("; {reason}")
                        }
                    )
                } else {
                    reason.clone()
                };
                self.audit(
                    &server.id,
                    &server.name,
                    label,
                    &audit_reason,
                    &p.command,
                    out.exit_code.map(i64::from),
                    Some(out.duration_ms),
                )
                .await;
                let exit = out.exit_code.map(|c| c.to_string()).unwrap_or_else(|| {
                    if out.timed_out {
                        "timeout".into()
                    } else {
                        "signal".into()
                    }
                });
                text(format!(
                    "server: {}\nexit_code: {exit}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    server.name, out.stdout, out.stderr
                ))
            }
            Err(e) => {
                self.audit(
                    &server.id,
                    &server.name,
                    "failed",
                    "ssh could not be started",
                    &p.command,
                    None,
                    None,
                )
                .await;
                eprintln!("{PREFIX}: cannot start ssh: {e}");
                error("ssh could not be started on the AionUi host".to_owned())
            }
        }
    }
}
