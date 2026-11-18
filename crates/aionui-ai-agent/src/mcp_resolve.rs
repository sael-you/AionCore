//! Neutral MCP server resolution (Wave 0c E) — the SINGLE source of truth for
//! turning a conversation's configured MCP servers into the SDK-free
//! [`SessionMcpServer`] shape the clean-slate session stack carries in
//! `SessionConfig.init.mcp_servers`.
//!
//! The legacy per-backend resolvers (`factory::acp::load_user_mcp_servers` →
//! `Vec<agent_client_protocol::McpServer>`, `factory::aionrs::load_user_mcp_servers`
//! → `HashMap<String, McpServerConfig>`) emit SDK/engine-specific types. This
//! module emits the NEUTRAL `aionui_api_types::SessionMcpServer` so the app
//! boundary (`aionui-app`) can convert it once into the crate-local
//! `aionui_session::McpServerSpec`, and each backend serializes that into its own
//! wire shape. Same row-walking + selection + stdio-launch-resolution logic as
//! the legacy ACP path, but vendor-neutral.

use std::sync::Arc;

use aionui_api_types::{SessionMcpServer, SessionMcpToolFilter, SessionMcpTransport, TEAM_MCP_SERVER_NAME};
use aionui_db::models::McpServerRow;
use aionui_db::{DbError, GatedMcpRow, IMcpServerRepository, MarketDelivery, McpMarketScope};
use aionui_realtime::EventBroadcaster;
use aionui_runtime::ensure_runtime_command;
use tracing::{info, warn};

/// Resolve a conversation's user-configured MCP servers into neutral
/// [`SessionMcpServer`]s. `selected_ids = Some` → that frozen snapshot defines the
/// session (injected regardless of the row's global `enabled` flag); `None` → all
/// enabled rows. Builtin rows are excluded (guide/team MCP are folded separately
/// by the caller). Stdio launch commands are RESOLVED here (e.g. `npx` → the
/// bundled-node absolute path) so the spec that reaches `open_session` is final —
/// the Wave 0c contract that `McpTransport::Stdio.command` is pre-resolved.
///
/// Best-effort: a repo error, a capability-unsupported transport, or a malformed
/// `transport_config` row is warn-logged and skipped, never fatal. `broadcaster`
/// is accepted for parity with the legacy reporter path (runtime-resolution status
/// reporting) and reserved for that use.
pub async fn resolve_session_mcp_servers(
    repo: &dyn IMcpServerRepository,
    user_id: &str,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
    market: Option<&McpMarketLaunch>,
    _broadcaster: Arc<dyn EventBroadcaster>,
) -> Vec<SessionMcpServer> {
    let rows = match load_gated_rows(repo, user_id, selected_ids, conversation_id, market).await {
        Ok(r) => r,
        Err(err) => {
            warn!(conversation_id, error = %err, "mcp_resolve: list() failed; skipping injection");
            return Vec::new();
        }
    };

    let mut servers = Vec::with_capacity(rows.len());
    for GatedMcpRow { row, market: delivery } in rows {
        let selected = is_selected(selected_ids, &row, delivery.as_ref());
        // `aionui-team` is a reserved wire-level name: the team coordination MCP
        // must win, so a user row that collides with it is skipped (never
        // injected), regardless of selection state.
        if !selected
            || row.builtin
            || row.name == TEAM_MCP_SERVER_NAME
            || row.name == SERVER_POOL_STANDARD
            || row.name == SERVER_POOL_CRUCIAL
        {
            continue;
        }
        if let (Some(delivery), Some(launch)) = (delivery.as_ref(), market) {
            match market_session_server(&row, delivery, launch) {
                Some(server) => servers.push(server),
                None => {
                    warn!(conversation_id, server_name = %row.name, "mcp_resolve: market row without project scope; skipping")
                }
            }
            continue;
        }
        match row_to_session_mcp_server(&row).await {
            Ok(server) => servers.push(server),
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    error = %err,
                    "mcp_resolve: failed to convert row; skipping"
                );
            }
        }
    }

    servers.extend(server_pool_session_servers(repo, user_id, conversation_id, market).await);

    if !servers.is_empty() {
        info!(
            conversation_id,
            count = servers.len(),
            "mcp_resolve: resolved user MCP servers"
        );
    }
    servers
}

/// MCP server name of the server-pool standard channel (C36).
pub const SERVER_POOL_STANDARD: &str = aionui_mcp::server_pool::STANDARD_SERVER_NAME;
/// MCP server name of the server-pool crucial channel (C36): every call asks
/// the owner (`require_approval`, the TASK-97 ask path of each CLI).
pub const SERVER_POOL_CRUCIAL: &str = aionui_mcp::server_pool::CRUCIAL_SERVER_NAME;

/// The two built-in server-pool MCPs (card C36) for a session, launched as
/// `aioncore mcp servers --channel … --conversation … --user … --data-dir …`.
/// Delivered when the market launcher is available and the user has at least
/// one enabled server; which servers this conversation may use is decided by
/// the helper on every call (so assignment changes apply at the next call).
/// Best-effort: a lookup error is logged and delivers nothing.
pub async fn server_pool_session_servers(
    repo: &dyn IMcpServerRepository,
    user_id: &str,
    conversation_id: &str,
    market: Option<&McpMarketLaunch>,
) -> Vec<SessionMcpServer> {
    let Some(launch) = market else {
        return Vec::new();
    };
    match repo.server_pool_enabled(user_id).await {
        Ok(true) => {}
        Ok(false) => return Vec::new(),
        Err(err) => {
            warn!(conversation_id, error = %err, "mcp_resolve: server pool lookup failed; skipping");
            return Vec::new();
        }
    }
    [
        ("standard", SERVER_POOL_STANDARD, false),
        ("crucial", SERVER_POOL_CRUCIAL, true),
    ]
    .into_iter()
    .map(|(channel, name, require_approval)| SessionMcpServer {
        id: name.to_owned(),
        name: name.to_owned(),
        transport: SessionMcpTransport::Stdio {
            command: launch.launcher.clone(),
            args: vec![
                "mcp".to_owned(),
                "servers".to_owned(),
                "--channel".to_owned(),
                channel.to_owned(),
                "--conversation".to_owned(),
                conversation_id.to_owned(),
                "--user".to_owned(),
                user_id.to_owned(),
                "--data-dir".to_owned(),
                launch.data_dir.clone(),
            ],
            env: std::collections::HashMap::new(),
        },
        tool_filter: SessionMcpToolFilter {
            enabled_tools: None,
            disabled_tools: Vec::new(),
            require_approval,
        },
    })
    .collect()
}

/// Whether a gated row belongs to the session: in the frozen selection (or
/// enabled when there is none), or granted by the conversation's kits (C27).
pub fn is_selected(selected_ids: Option<&[String]>, row: &McpServerRow, delivery: Option<&MarketDelivery>) -> bool {
    delivery.is_some_and(|d| d.conversation_id.is_some())
        || selected_ids
            .map(|ids| ids.iter().any(|id| id == &row.id))
            .unwrap_or(row.enabled)
}

/// What the market gate and the launcher rewrite need for one session
/// (MCP market, R1 #4, R9). `None` at a call site = no market delivery: every
/// market row is dropped (fail closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMarketLaunch {
    /// Absolute path of the running `aioncore` binary (the launcher).
    pub launcher: String,
    /// Absolute AionUi data dir, passed as `--data-dir`.
    pub data_dir: String,
    /// The conversation's assistant (`preset_assistant_id`).
    pub assistant_id: Option<String>,
    /// Backend CLI label (`claude`, `codex`, `gemini`, ...).
    pub cli: String,
}

impl McpMarketLaunch {
    /// Build from the factory's backend binary + data dir; the data dir is
    /// made absolute so the launcher never depends on its cwd.
    pub fn new(launcher: &std::path::Path, data_dir: &std::path::Path, assistant_id: Option<&str>, cli: &str) -> Self {
        let data_dir = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
        Self {
            launcher: launcher.to_string_lossy().into_owned(),
            data_dir: data_dir.to_string_lossy().into_owned(),
            assistant_id: assistant_id.map(str::to_owned).filter(|a| !a.is_empty()),
            cli: cli.to_owned(),
        }
    }
}

/// Load a session's selected rows (or all enabled rows when there is no
/// snapshot) and run them through the ONE market gate
/// ([`IMcpServerRepository::market_gate`], R1 #5). Shared by every spawn path:
/// this module (claude/codex/agy), the ACP factory (gemini, ...) and aionrs.
pub async fn load_gated_rows(
    repo: &dyn IMcpServerRepository,
    user_id: &str,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
    market: Option<&McpMarketLaunch>,
) -> Result<Vec<GatedMcpRow>, DbError> {
    let mut rows = match selected_ids {
        Some(ids) => repo.list_by_ids_any(user_id, ids).await?,
        None => repo.list(user_id).await?,
    };
    // C27: market instances the conversation's kits grant join the candidates
    // (only when the market launcher is available; the gate decides again).
    if market.is_some() {
        let missing: Vec<String> = repo
            .kit_mcp_ids(user_id, conversation_id)
            .await?
            .into_iter()
            .filter(|id| !rows.iter().any(|r| &r.id == id))
            .collect();
        if !missing.is_empty() {
            rows.extend(repo.list_by_ids_any(user_id, &missing).await?);
        }
    }
    let scope = McpMarketScope {
        conversation_id: Some(conversation_id),
        assistant_id: market.and_then(|m| m.assistant_id.as_deref()),
        cli: market.map(|m| m.cli.as_str()),
    };
    repo.market_gate(user_id, &scope, rows).await
}

fn json_list(value: Option<&str>) -> Option<Vec<String>> {
    value.and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
}

/// The launcher entry for a gated market row (R9): never the row's own
/// transport, always `aioncore mcp exec <id> --project <pid> --assistant <aid>
/// --cli <cli> --data-dir <dir>` (or `--conversation <cid>` instead of
/// project/assistant for a kit grant, C27) with no env; the launcher decides again from
/// the DB. Tool filters ride along for backends that apply them natively.
pub fn market_session_server(
    row: &McpServerRow,
    delivery: &MarketDelivery,
    launch: &McpMarketLaunch,
) -> Option<SessionMcpServer> {
    let mut args = vec!["mcp".to_owned(), "exec".to_owned(), row.name.clone()];
    if let Some(conversation) = delivery.conversation_id.as_deref() {
        // C27 kit grant: the launcher recomputes the conversation's effective set.
        args.extend(["--conversation".to_owned(), conversation.to_owned()]);
    } else {
        let project = delivery.project_id.as_deref()?;
        let assistant = delivery.assistant_id.as_deref()?;
        args.extend(["--project".to_owned(), project.to_owned()]);
        args.extend(["--assistant".to_owned(), assistant.to_owned()]);
    }
    args.extend(["--cli".to_owned(), launch.cli.clone()]);
    args.extend(["--data-dir".to_owned(), launch.data_dir.clone()]);
    let enabled_tools = if delivery.tier == "write" {
        None
    } else {
        Some(json_list(delivery.tools_allow.as_deref()).unwrap_or_default())
    };
    Some(SessionMcpServer {
        id: row.id.clone(),
        name: row.name.clone(),
        transport: SessionMcpTransport::Stdio {
            command: launch.launcher.clone(),
            args,
            env: std::collections::HashMap::new(),
        },
        tool_filter: SessionMcpToolFilter {
            enabled_tools,
            disabled_tools: json_list(delivery.tools_block.as_deref()).unwrap_or_default(),
            // TASK-97: production writes need the owner's OK per call (D50 keeps
            // full tools, so the CLI must ask instead of the server refusing).
            require_approval: delivery.requires_owner_approval(),
        },
    })
}

/// Parse one `McpServerRow` into a neutral `SessionMcpServer`, resolving the stdio
/// launch command. Mirrors `factory::acp::row_to_sdk_mcp_server` but emits the
/// neutral type. Returns an error string when `transport_config` is malformed.
///
/// This is the SINGLE shared row→neutral conversion: `aionui-conversation` (team
/// snapshot refresh) and the session stack reuse it so the stdio command is
/// always normalized exactly once, the same way the direct claude/codex path
/// consumes it.
pub async fn row_to_session_mcp_server(row: &McpServerRow) -> Result<SessionMcpServer, String> {
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    let transport = match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut env: std::collections::HashMap<String, String> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();

            // Resolve the launch command (npx/bun → bundled path) + fold in the
            // runtime-provided args prefix + env, exactly like the legacy
            // `ensure_stdio_launch`. The resolved form is what the agent spawns.
            let resolved = ensure_runtime_command(command).await.map_err(|e| e.to_string())?;
            let mut final_args: Vec<String> = resolved
                .args_prefix
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            final_args.extend(args);
            for (k, v) in resolved.env {
                env.insert(k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned());
            }
            SessionMcpTransport::Stdio {
                command: resolved.program.to_string_lossy().into_owned(),
                args: final_args,
                env,
            }
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?
                .to_owned();
            SessionMcpTransport::StreamableHttp {
                url,
                headers: parse_headers(value.get("headers")),
            }
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?
                .to_owned();
            SessionMcpTransport::Sse {
                url,
                headers: parse_headers(value.get("headers")),
            }
        }
        other => return Err(format!("unknown transport type: {other}")),
    };

    Ok(SessionMcpServer {
        id: row.id.clone(),
        name: row.name.clone(),
        transport,
        tool_filter: Default::default(),
    })
}

/// Parse a JSON headers object into a `HashMap` (string values only).
fn parse_headers(value: Option<&serde_json::Value>) -> std::collections::HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::models::McpServerRow;

    const TEST_USER_ID: &str = "user-1";

    struct MockRepo {
        rows: Vec<McpServerRow>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self, user_id: &str) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                return Err(aionui_db::DbError::Init("boom".into()));
            }
            Ok(self.rows.iter().filter(|row| row.user_id == user_id).cloned().collect())
        }
        async fn find_by_id(&self, user_id: &str, id: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.user_id == user_id && row.id == id)
                .cloned())
        }
        async fn find_by_name(&self, user_id: &str, name: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.user_id == user_id && row.name == name)
                .cloned())
        }
        async fn create(
            &self,
            _params: aionui_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update(
            &self,
            _user_id: &str,
            _id: &str,
            _params: aionui_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn delete(&self, _user_id: &str, _id: &str) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn batch_upsert(
            &self,
            _user_id: &str,
            _servers: &[aionui_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update_status(
            &self,
            _user_id: &str,
            _id: &str,
            _status: &str,
            _last_connected: Option<aionui_common::TimestampMs>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update_tools(
            &self,
            _user_id: &str,
            _id: &str,
            _tools: Option<&str>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
    }

    fn make_row(name: &str, enabled: bool) -> McpServerRow {
        McpServerRow {
            id: format!("mcp_{name}"),
            user_id: TEST_USER_ID.to_owned(),
            name: name.to_owned(),
            description: None,
            enabled,
            transport_type: "http".into(),
            transport_config: r#"{"url":"http://127.0.0.1:9999/mcp"}"#.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin: false,
            deleted_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn test_broadcaster() -> Arc<dyn EventBroadcaster> {
        Arc::new(aionui_realtime::BroadcastEventBus::new(16))
    }

    #[tokio::test]
    async fn resolve_skips_reserved_team_mcp_name() {
        let repo = MockRepo {
            rows: vec![make_row("docs", true), make_row(TEAM_MCP_SERVER_NAME, true)],
            fail: false,
        };
        let servers = resolve_session_mcp_servers(&repo, TEST_USER_ID, None, "conv-1", None, test_broadcaster()).await;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
    }

    #[tokio::test]
    async fn resolve_skips_disabled_and_builtin_rows() {
        let repo = MockRepo {
            rows: vec![
                make_row("docs", true),
                make_row("off", false),
                McpServerRow {
                    id: "mcp_builtin".into(),
                    user_id: TEST_USER_ID.to_owned(),
                    name: "chrome-devtools".into(),
                    description: None,
                    enabled: true,
                    transport_type: "stdio".into(),
                    transport_config: r#"{"command":"/bin/true","args":[],"env":{}}"#.into(),
                    tools: None,
                    last_test_status: "disconnected".into(),
                    last_connected: None,
                    original_json: None,
                    builtin: true,
                    deleted_at: None,
                    created_at: 0,
                    updated_at: 0,
                },
            ],
            fail: false,
        };
        let servers = resolve_session_mcp_servers(&repo, TEST_USER_ID, None, "conv-1", None, test_broadcaster()).await;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
    }
}

/// Seeding helpers for the market-gate tests of every spawn path (R1 #5).
#[cfg(test)]
pub(crate) mod market_test_support {
    use aionui_db::{
        CreateAssignmentParams, CreateMcpServerParams, Database, IMcpMarketRepository, IMcpServerRepository,
        MarketInstanceParams, SqliteMcpMarketRepository, SqliteMcpServerRepository, init_database_memory,
    };

    pub const USER: &str = "system_default_user";
    pub const CONV: &str = "conv-market";

    pub struct MarketFx {
        pub db: Database,
        pub servers: SqliteMcpServerRepository,
        pub market: SqliteMcpMarketRepository,
    }

    pub async fn market_fx() -> MarketFx {
        let db = init_database_memory().await.unwrap();
        for sql in [
            "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)",
            "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id) VALUES ('conv-market', 'system_default_user', 'c', 'acp', 'pending', 1, 1, 'proj-a')",
        ] {
            sqlx::query(sql).execute(db.pool()).await.unwrap();
        }
        MarketFx {
            servers: SqliteMcpServerRepository::new(db.pool().clone()),
            market: SqliteMcpMarketRepository::new(db.pool().clone()),
            db,
        }
    }

    impl MarketFx {
        /// A market instance (read tier, allowlist `["echo"]`, block `["drop"]`).
        pub async fn instance(&self, name: &str, status: &str) -> String {
            self.instance_with(name, status, "local", "read").await
        }

        /// A market instance in `environment` at `tier` (write = no allowlist).
        pub async fn instance_with(&self, name: &str, status: &str, environment: &str, tier: &str) -> String {
            let config = format!(r#"{{"command":"aioncore","args":["mcp","exec","{name}"],"env":{{}}}}"#);
            self.market
                .create_instance(&MarketInstanceParams {
                    user_id: USER,
                    name,
                    description: None,
                    transport_type: "stdio",
                    transport_config: &config,
                    status,
                    template_id: "fake",
                    template_json: r#"{"id":"fake","name":"Fake","transport":"stdio","command":{"command":"/bin/true","args":[]},"tiers":["read"],"enforcement":"server","clis":["claude","codex","gemini"]}"#,
                    environment,
                    tier,
                    enforcement: "server",
                    clis_json: r#"["claude","codex","gemini"]"#,
                    plain_env_json: "{}",
                    secret_refs_json: "{}",
                    tools_allow_json: (tier == "read").then_some(r#"["echo"]"#),
                    tools_block_json: r#"["drop"]"#,
                    approval_ref: None,
                })
                .await
                .unwrap()
                .id
        }

        pub async fn assign(&self, id: &str, assistant: Option<&str>, cli: &str) {
            self.market
                .create_assignment(&CreateAssignmentParams {
                    user_id: USER,
                    instance_id: id,
                    project_id: Some("proj-a"),
                    assistant_id: assistant,
                    cli,
                })
                .await
                .unwrap();
        }

        pub async fn plain(&self, name: &str) -> String {
            self.servers
                .create(CreateMcpServerParams {
                    user_id: USER,
                    name,
                    description: None,
                    enabled: false,
                    transport_type: "stdio",
                    transport_config: r#"{"command":"/bin/sh","args":["-c","true"]}"#,
                    tools: None,
                    original_json: None,
                    builtin: false,
                })
                .await
                .unwrap()
                .id
        }
    }
}

#[cfg(test)]
mod market_gate_tests {
    use super::market_test_support::{CONV, USER, market_fx};
    use super::*;
    use aionui_realtime::BroadcastEventBus;

    fn bus() -> Arc<dyn EventBroadcaster> {
        Arc::new(BroadcastEventBus::new(8))
    }

    fn launch(assistant: Option<&str>, cli: &str) -> McpMarketLaunch {
        McpMarketLaunch::new(
            std::path::Path::new("/opt/aionui/aioncore"),
            std::path::Path::new("/data/aionui"),
            assistant,
            cli,
        )
    }

    /// C36: the two server-pool MCPs ride along only when the user has an
    /// enabled server and the launcher is known; the crucial one asks (TASK-97).
    #[tokio::test]
    async fn server_pool_mcps_are_injected_with_the_crucial_one_asking() {
        use aionui_db::{CreateServerParams, IServerPoolRepository, SqliteServerPoolRepository};
        let fx = market_fx().await;
        let none =
            resolve_session_mcp_servers(&fx.servers, USER, Some(&[]), CONV, Some(&launch(None, "claude")), bus()).await;
        assert!(none.is_empty(), "no server, no server MCPs");

        SqliteServerPoolRepository::new(fx.db.pool().clone())
            .create_server(&CreateServerParams {
                user_id: USER,
                name: "web-1",
                host: "10.0.0.5",
                port: 22,
                ssh_user: "deploy",
                host_key: "",
                labels: "[]",
                environment: "production",
                tier: "write",
                notes: "",
                enabled: true,
                key_encrypted: None,
            })
            .await
            .unwrap();
        // Without the launcher (no market delivery) nothing is injected.
        assert!(
            resolve_session_mcp_servers(&fx.servers, USER, Some(&[]), CONV, None, bus())
                .await
                .is_empty()
        );

        let servers =
            resolve_session_mcp_servers(&fx.servers, USER, Some(&[]), CONV, Some(&launch(None, "claude")), bus()).await;
        let names: Vec<(&str, bool)> = servers
            .iter()
            .map(|s| (s.name.as_str(), s.tool_filter.require_approval))
            .collect();
        assert_eq!(names, vec![(SERVER_POOL_STANDARD, false), (SERVER_POOL_CRUCIAL, true)]);
        match &servers[1].transport {
            SessionMcpTransport::Stdio { command, args, .. } => {
                assert_eq!(command, "/opt/aionui/aioncore");
                assert_eq!(
                    args,
                    &[
                        "mcp",
                        "servers",
                        "--channel",
                        "crucial",
                        "--conversation",
                        CONV,
                        "--user",
                        USER,
                        "--data-dir",
                        "/data/aionui"
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn assigned_market_rows_become_launcher_entries_and_others_are_dropped() {
        let fx = market_fx().await;
        let active = fx.instance("tb-active", "active").await;
        let dormant = fx.instance("tb-dormant", "dormant").await;
        let revoked = fx.instance("tb-revoked", "revoked").await;
        let other_asst = fx.instance("tb-other", "active").await;
        let project_only = fx.instance("tb-project", "active").await;
        let plain = fx.plain("plain").await;
        for id in [&active, &dormant, &revoked] {
            fx.assign(id, Some("asst-1"), "claude").await;
        }
        fx.assign(&other_asst, Some("asst-2"), "claude").await;
        fx.assign(&project_only, None, "claude").await;
        let selected = vec![active.clone(), dormant, revoked, other_asst, project_only, plain];

        let servers = resolve_session_mcp_servers(
            &fx.servers,
            USER,
            Some(&selected),
            CONV,
            Some(&launch(Some("asst-1"), "claude")),
            bus(),
        )
        .await;
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["tb-active", "plain"],
            "explicitly selected inactive/unassigned rows are dropped"
        );
        let market = &servers[0];
        match &market.transport {
            SessionMcpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "/opt/aionui/aioncore");
                assert_eq!(
                    args,
                    &[
                        "mcp",
                        "exec",
                        "tb-active",
                        "--project",
                        "proj-a",
                        "--assistant",
                        "asst-1",
                        "--cli",
                        "claude",
                        "--data-dir",
                        "/data/aionui"
                    ]
                );
                assert!(env.is_empty(), "no env on the launcher entry");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            market.tool_filter.enabled_tools.as_deref(),
            Some(&["echo".to_owned()][..])
        );
        assert_eq!(market.tool_filter.disabled_tools, vec!["drop"]);
        assert!(servers[1].tool_filter.is_empty());

        // Another CLI, no market context: the market row is gone, plain rows stay.
        for market_ctx in [Some(launch(Some("asst-1"), "codex")), None] {
            let servers =
                resolve_session_mcp_servers(&fx.servers, USER, Some(&selected), CONV, market_ctx.as_ref(), bus()).await;
            let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, vec!["plain"]);
        }
        let _ = active;
    }

    /// TASK-97: only a production WRITE instance asks before every call; a
    /// production read or a non-production write instance keeps the old shape.
    #[tokio::test]
    async fn production_write_instance_requires_approval() {
        let fx = market_fx().await;
        let cases = [
            ("tb-prod-write", "production", "write", true),
            ("tb-prod-read", "production", "read", false),
            ("tb-stage-write", "staging", "write", false),
        ];
        let mut selected = Vec::new();
        for (name, environment, tier, _) in cases {
            let id = fx.instance_with(name, "active", environment, tier).await;
            fx.assign(&id, Some("asst-1"), "codex").await;
            selected.push(id);
        }
        let servers = resolve_session_mcp_servers(
            &fx.servers,
            USER,
            Some(&selected),
            CONV,
            Some(&launch(Some("asst-1"), "codex")),
            bus(),
        )
        .await;
        assert_eq!(servers.len(), cases.len());
        for (name, _, _, approval) in cases {
            let server = servers.iter().find(|s| s.name == name).unwrap();
            assert_eq!(server.tool_filter.require_approval, approval, "{name}");
        }
        // The flag survives the snapshot round trip and is omitted when false.
        let prod = servers.iter().find(|s| s.name == "tb-prod-write").unwrap();
        let json = serde_json::to_value(prod).unwrap();
        assert_eq!(json["tool_filter"]["require_approval"], true);
        let back: SessionMcpServer = serde_json::from_value(json).unwrap();
        assert!(back.tool_filter.require_approval);
        let read = servers.iter().find(|s| s.name == "tb-prod-read").unwrap();
        assert!(
            serde_json::to_value(read).unwrap()["tool_filter"]
                .get("require_approval")
                .is_none()
        );
    }

    /// TASK-97 x C27: a production write instance granted by a conversation-level
    /// kit (no assignment) becomes a `--conversation` launcher entry that still
    /// requires approval, so every CLI asks before each call.
    #[tokio::test]
    async fn kit_granted_production_write_instance_requires_approval() {
        let fx = market_fx().await;
        let id = fx.instance_with("tb-kit-live", "active", "production", "write").await;
        sqlx::query(
            "INSERT INTO kit_attachments (id, user_id, level, target_id, item_kind, item_ref, op, created_at, updated_at) \
             VALUES ('att-1', ?, 'conversation', ?, 'mcp', ?, 'add', 1, 1)",
        )
        .bind(USER)
        .bind(CONV)
        .bind(&id)
        .execute(fx.db.pool())
        .await
        .unwrap();
        let servers = resolve_session_mcp_servers(
            &fx.servers,
            USER,
            Some(&[]),
            CONV,
            Some(&launch(Some("asst-1"), "codex")),
            bus(),
        )
        .await;
        assert_eq!(servers.len(), 1, "the kit grant delivers the instance");
        let server = &servers[0];
        assert_eq!(server.name, "tb-kit-live");
        match &server.transport {
            SessionMcpTransport::Stdio { args, .. } => {
                assert!(args.windows(2).any(|w| w == ["--conversation", CONV]), "{args:?}")
            }
            other => panic!("{other:?}"),
        }
        assert!(server.tool_filter.require_approval);
    }

    #[tokio::test]
    async fn deleted_market_row_is_never_injected() {
        let fx = market_fx().await;
        let id = fx.instance("tb-gone", "active").await;
        fx.assign(&id, Some("asst-1"), "claude").await;
        aionui_db::IMcpMarketRepository::delete_instance(&fx.market, USER, &id)
            .await
            .unwrap();
        let servers = resolve_session_mcp_servers(
            &fx.servers,
            USER,
            Some(std::slice::from_ref(&id)),
            CONV,
            Some(&launch(Some("asst-1"), "claude")),
            bus(),
        )
        .await;
        assert!(servers.is_empty());
        let _ = &fx.db;
    }
}
