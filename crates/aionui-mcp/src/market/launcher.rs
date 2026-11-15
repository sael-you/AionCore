//! The ONE place that decides which binary runs `aioncore mcp exec` for this
//! process, and the rewrite of the market placeholder transport to it. Every
//! path that resolves, spawns or health-checks a market row uses these helpers
//! (session spawn rewrite, conversation MCP status, test-connection, agent
//! config sync), so a market row never looks "failed" just because
//! `aioncore` is not on PATH.

use std::borrow::Cow;
use std::path::PathBuf;

use aionui_db::models::McpServerRow;

use super::stored::PLACEHOLDER_COMMAND;
use crate::types::McpServerTransport;

/// Optional override: absolute path of the launcher to use (e.g. the stable
/// `/root/.clauterm/studio/bin/aioncore` link). Ignored unless it is an
/// absolute path to an existing file.
pub const LAUNCHER_ENV: &str = "AIONUI_MCP_LAUNCHER";

/// The launcher binary: [`LAUNCHER_ENV`] when configured, else the running
/// executable (`std::env::current_exe`), else the bare placeholder name.
pub fn launcher_program() -> PathBuf {
    if let Some(configured) = std::env::var_os(LAUNCHER_ENV).map(PathBuf::from)
        && configured.is_absolute()
        && configured.is_file()
    {
        return configured;
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from(PLACEHOLDER_COMMAND))
}

/// Whether a stdio command/args pair is the market placeholder
/// (`aioncore mcp exec <id> ...`).
pub fn is_placeholder(command: &str, args: &[String]) -> bool {
    command == PLACEHOLDER_COMMAND && args.len() >= 2 && args[0] == "mcp" && args[1] == "exec"
}

/// Rewrite the placeholder command of a domain transport in place.
pub fn resolve_placeholder_transport(transport: &mut McpServerTransport) {
    if let McpServerTransport::Stdio { command, args, .. } = transport
        && is_placeholder(command, args)
    {
        *command = launcher_program().to_string_lossy().into_owned();
    }
}

/// A row whose `transport_config` is the market placeholder, with the command
/// rewritten to [`launcher_program`]; any other row is returned unchanged.
pub fn resolve_placeholder_row(row: &McpServerRow) -> Cow<'_, McpServerRow> {
    if row.transport_type != "stdio" {
        return Cow::Borrowed(row);
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&row.transport_config) else {
        return Cow::Borrowed(row);
    };
    let command = value.get("command").and_then(|c| c.as_str()).unwrap_or_default();
    let args: Vec<String> = value
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    if !is_placeholder(command, &args) {
        return Cow::Borrowed(row);
    }
    value["command"] = serde_json::Value::String(launcher_program().to_string_lossy().into_owned());
    let mut owned = row.clone();
    owned.transport_config = value.to_string();
    Cow::Owned(owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::stored::placeholder_transport_config;

    fn row(transport_type: &str, config: &str) -> McpServerRow {
        McpServerRow {
            id: "mcp_1".into(),
            user_id: "u".into(),
            name: "tb-x".into(),
            description: None,
            enabled: false,
            transport_type: transport_type.into(),
            transport_config: config.into(),
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

    #[test]
    fn placeholder_rows_point_at_an_existing_launcher() {
        let r = row("stdio", &placeholder_transport_config("tb-x"));
        let resolved = resolve_placeholder_row(&r);
        let v: serde_json::Value = serde_json::from_str(&resolved.transport_config).unwrap();
        let cmd = v["command"].as_str().unwrap();
        assert_ne!(cmd, PLACEHOLDER_COMMAND);
        assert!(std::path::Path::new(cmd).is_file(), "{cmd} must exist");
        assert_eq!(v["args"][2], "tb-x", "args are kept");

        let mut t = McpServerTransport::from_db("stdio", &r.transport_config).unwrap();
        resolve_placeholder_transport(&mut t);
        assert!(matches!(t, McpServerTransport::Stdio { ref command, .. } if command == cmd));
    }

    #[test]
    fn other_rows_are_untouched() {
        for r in [
            row("stdio", r#"{"command":"aioncore","args":["mcp-team-stdio"]}"#),
            row("stdio", r#"{"command":"node","args":["mcp","exec"]}"#),
            row("http", r#"{"url":"https://x"}"#),
        ] {
            assert!(matches!(resolve_placeholder_row(&r), Cow::Borrowed(_)));
        }
    }
}
