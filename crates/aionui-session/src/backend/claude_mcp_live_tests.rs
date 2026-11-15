//! LIVE test (`#[ignore]`) for the claude MCP precedence `claude_conn` relies on:
//! WITHOUT `--strict-mcp-config`, the `--mcp-config` servers are ADDED to the
//! workspace `.mcp.json` ones, and on a name clash the `--mcp-config` entry wins
//! (`scope: "dynamic"`). Asserted against the real `claude` binary on PATH via the
//! stream-json `control_request{mcp_status}`, using the production builders
//! (`prepare_claude_mcp_config` + `build_claude_init_args`) and spawn path
//! (`ClaudeAdapter::start_turn` over a `RealSpawner`).
//!
//! In-crate (not under `tests/`) because those builders are crate-private. No
//! prompt is sent, so no tokens are spent; it needs `claude` and `node` on PATH.
//!
//! Last run green against claude 2.1.325.
//!
//! ```text
//! cargo test -p aionui-session --lib claude_mcp_live -- --ignored --nocapture
//! ```

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::claude_conn::{build_claude_init_args, prepare_claude_mcp_config};
use super::{McpServerSpec, McpTransport, SessionConfig, SessionInit};
use crate::adapter::{BackendAdapter, ClaudeAdapter, SessionSpec as LegacySessionSpec};

/// A minimal stdio MCP server whose single tool (and `serverInfo.name`) is
/// `argv[2]`, so the status reply shows WHICH definition of a name connected.
const PROBE_SERVER_JS: &str = r#"
const tool = process.argv[2] || 'x';
let buf = '';
process.stdin.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) {
  const line = buf.slice(0, i); buf = buf.slice(i + 1); if (!line.trim()) continue;
  let m; try { m = JSON.parse(line); } catch { continue; }
  if (m.id === undefined) continue;
  let result = {};
  if (m.method === 'initialize') result = { protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: tool, version: '1' } };
  else if (m.method === 'tools/list') result = { tools: [{ name: tool, description: 'probe', inputSchema: { type: 'object', properties: {} } }] };
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: m.id, result }) + '\n'); } });
"#;

fn stdio(script: &std::path::Path, tool: &str) -> serde_json::Value {
    serde_json::json!({ "command": "node", "args": [script, tool] })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "LIVE: spawns the real claude CLI (no prompt, no tokens)"]
async fn mcp_config_entry_wins_a_name_clash_and_project_servers_stay() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let script = tmp.path().join("probe-server.js");
    std::fs::write(&script, PROBE_SERVER_JS).unwrap();
    std::fs::write(
        project.join(".mcp.json"),
        serde_json::json!({ "mcpServers": {
            "dup": stdio(&script, "from_project"),
            "projonly": stdio(&script, "projonly_tool"),
        }})
        .to_string(),
    )
    .unwrap();

    let config = SessionConfig {
        cwd: Some(project.to_string_lossy().into_owned()),
        init: SessionInit {
            mcp_servers: vec![McpServerSpec {
                name: "dup".into(),
                transport: McpTransport::Stdio {
                    command: "node".into(),
                    args: vec![script.to_string_lossy().into_owned(), "from_flag".into()],
                    env: vec![],
                },
            }],
            ..Default::default()
        },
        runtime_dir: Some(tmp.path().join("view")),
        ..Default::default()
    };
    let (arg, _file) = prepare_claude_mcp_config(&config).expect("private file");
    let mut args = build_claude_init_args(&config, arg.as_deref());
    assert!(!args.iter().any(|a| a == "--strict-mcp-config"));
    args.push("--no-session-persistence".into());

    let spawner = aionui_process::RealSpawner::new(
        Arc::new(aionui_process::FileRegistryStore::new(tmp.path())),
        uuid::Uuid::now_v7(),
        "live-test",
    );
    let io = ClaudeAdapter::new()
        .start_turn(
            &spawner,
            &LegacySessionSpec::Fresh(uuid::Uuid::new_v4().to_string()),
            config.cwd.as_deref(),
            &args,
            &[],
            None,
        )
        .await
        .expect("claude spawns (is it on PATH?)");
    let (mut stdin, stdout) = io.take_stdio().await.expect("stdio");
    let mut lines = BufReader::new(stdout).lines();

    let mut send = async |id: &str, subtype: &str| {
        let frame = serde_json::json!({
            "type": "control_request", "request_id": id, "request": { "subtype": subtype }
        });
        stdin.write_all(format!("{frame}\n").as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    };
    send("live-init", "initialize").await;

    // Poll until no server is still pending (connections are asynchronous).
    let mut servers = Vec::new();
    for attempt in 0..30 {
        let id = format!("live-mcp-{attempt}");
        send(&id, "mcp_status").await;
        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await.expect("read stdout") {
                let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if frame["response"]["request_id"] == id.as_str() {
                    return frame["response"]["response"]["mcpServers"].clone();
                }
            }
            panic!("claude closed stdout before answering mcp_status");
        })
        .await
        .expect("mcp_status answered");
        servers = reply.as_array().cloned().unwrap_or_default();
        if !servers.is_empty() && servers.iter().all(|s| s["status"] != "pending") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    println!("mcp_status: {}", serde_json::Value::Array(servers.clone()));
    io.terminate().await;

    let by_name = |name: &str| servers.iter().find(|s| s["name"] == name).cloned();
    let dup = by_name("dup").expect("dup listed");
    assert_eq!(dup["scope"], "dynamic", "the --mcp-config entry owns the clashing name");
    assert_eq!(
        dup["serverInfo"]["name"], "from_flag",
        "the --mcp-config definition connected, not the project's"
    );
    let projonly = by_name("projonly").expect("project servers are kept without strict");
    assert_eq!(projonly["scope"], "project");
    assert_eq!(projonly["status"], "connected");
}
