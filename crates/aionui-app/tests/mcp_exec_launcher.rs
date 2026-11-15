//! `aioncore mcp exec` end to end (card C15, R9 + Review R1 #1–#3, #8): the
//! real binary against a real data dir, with a fake stdio MCP server (python3)
//! as the template command. Covers assignment refusal, dormant/revoked
//! refusal, a missing DB (never created), env isolation (no
//! AIONUI_RUNTIME_TOKEN, no parent-only vars, the instance's own secret
//! present), proxy filtering of `tools/list` / `tools/call`, and stopping a
//! running launcher on revoke.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aionui_api_types::{
    CreateMcpMarketAssignmentRequest, CreateMcpMarketInstanceRequest, UpdateMcpMarketInstanceRequest,
};
use aionui_db::{
    Database, IMcpMarketRepository, IMcpServerRepository, IUserRepository, SqliteMcpMarketRepository,
    SqliteMcpServerRepository, SqliteUserRepository, init_database,
};
use aionui_mcp::{McpMarketService, StaticTemplateCatalog};
use serde_json::{Value, json};

const USER: &str = "system_default_user";
const ENCRYPTION_SECRET: &str = "launcher-test-encryption-root";
const FIXTURE_SECRET: &str = "fake_key_LAUNCHERFIXTURE0123456789";
const RUNTIME_TOKEN: &str = "runtime-token-SENTINEL-must-not-leak";
const PYTHON: &str = "/usr/bin/python3";

const FAKE_SERVER: &str = r#"
import json, os, sys
TOOLS = [{"name": n, "description": "fake " + n, "inputSchema": {"type": "object"}} for n in ("echo", "env", "forbidden")]
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    m = json.loads(line)
    if "id" not in m:
        continue
    mid, method = m["id"], m.get("method")
    if method == "initialize":
        res = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "0"}}
    elif method == "tools/list":
        res = {"tools": TOOLS}
    elif method == "tools/call":
        name = m["params"]["name"]
        if name == "env":
            text = json.dumps(dict(os.environ))
        else:
            text = "called " + name
        res = {"content": [{"type": "text", "text": text}]}
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "nope"}}) + "\n")
        sys.stdout.flush()
        continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": res}) + "\n")
    sys.stdout.flush()
"#;

struct Fx {
    _dir: tempfile::TempDir,
    data_dir: PathBuf,
    db: Database,
    market: McpMarketService,
}

async fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("aionui-web");
    std::fs::create_dir_all(&data_dir).unwrap();
    let script = dir.path().join("fake_mcp.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();
    let db = init_database(&data_dir.join("aionui-backend.db")).await.unwrap();
    let users = SqliteUserRepository::new(db.pool().clone());
    users.update_encryption_secret(USER, ENCRYPTION_SECRET).await.unwrap();
    sqlx::query(
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let catalog = StaticTemplateCatalog::new(vec![json!({"id": "fakesrv", "name": "Fake", "market": {
        "transport": "stdio",
        "command": {"command": PYTHON, "args": ["-u", script.to_string_lossy()]},
        "env": {"FAKE_API_KEY": {"kind": "secret", "required": true}, "FAKE_MODE": {"kind": "plain"}},
        "tiers": ["read"], "enforcement": "server", "clis": ["claude", "codex"]
    }})]);
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let market = McpMarketService::new(
        repo,
        servers,
        Arc::new(catalog),
        aionui_app::derive_encryption_key(ENCRYPTION_SECRET),
    );
    Fx {
        _dir: dir,
        data_dir,
        db,
        market,
    }
}

impl Fx {
    async fn instance(&self, name: &str, active: bool) -> String {
        let inst = self
            .market
            .create_instance(
                USER,
                CreateMcpMarketInstanceRequest {
                    name: name.into(),
                    template_id: "fakesrv".into(),
                    description: None,
                    environment: "local".into(),
                    tier: "read".into(),
                    status: None,
                    clis: vec!["claude".into(), "codex".into()],
                    plain_env: BTreeMap::from([("FAKE_MODE".into(), "read-only".into())]),
                    tools_allow: Some(vec!["echo".into(), "env".into()]),
                    tools_block: vec![],
                },
            )
            .await
            .unwrap();
        self.market
            .set_secret(USER, &inst.id, "FAKE_API_KEY", FIXTURE_SECRET)
            .await
            .unwrap();
        if active {
            self.set_status(&inst.id, "active").await;
        }
        inst.id
    }

    async fn set_status(&self, id: &str, status: &str) {
        self.market
            .update_instance(
                USER,
                id,
                UpdateMcpMarketInstanceRequest {
                    status: Some(status.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    async fn assign(&self, id: &str, cli: &str) {
        self.market
            .create_assignments(
                USER,
                CreateMcpMarketAssignmentRequest {
                    instance_id: id.into(),
                    project_id: Some("proj-a".into()),
                    assistant_id: None,
                    clis: vec![cli.into()],
                },
            )
            .await
            .unwrap();
    }

    fn launcher(&self, instance: &str, project: &str, cli: &str) -> Command {
        launcher_cmd(&self.data_dir, instance, project, cli)
    }
}

fn launcher_cmd(data_dir: &Path, instance: &str, project: &str, cli: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    cmd.args([
        "mcp",
        "exec",
        instance,
        "--project",
        project,
        "--cli",
        cli,
        "--data-dir",
    ])
    .arg(data_dir)
    .env_remove("AIONUI_DATA_DIR")
    .env_remove("AIONUI_ENCRYPTION_SECRET")
    .env("AIONUI_RUNTIME_TOKEN", RUNTIME_TOKEN)
    .env("AIONUI_USER_ID", USER)
    .env("PARENT_ONLY_VAR", "parent-only")
    .env("GITHUB_TOKEN", "ghp_parentTokenThatMustNotPassThrough00")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    cmd
}

fn refused(mut cmd: Command) -> (i32, String) {
    let out = cmd.stdin(Stdio::null()).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(mut cmd: Command) -> Self {
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout }
    }

    fn call(&mut self, msg: Value) -> Value {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
    }
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_unassigned_dormant_revoked_and_missing_db() {
    let f = fx().await;
    let id = f.instance("tb-refuse", false).await;
    let (code, err) = refused(f.launcher("tb-refuse", "proj-a", "claude"));
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("dormant"), "{err}");
    f.set_status(&id, "active").await;
    let (code, err) = refused(f.launcher("tb-refuse", "proj-a", "claude"));
    assert_eq!(code, 3);
    assert!(err.contains("not assigned"), "{err}");
    f.assign(&id, "claude").await;
    let (code, err) = refused(f.launcher("tb-refuse", "proj-b", "claude"));
    assert_eq!(code, 3);
    assert!(err.contains("not assigned"), "{err}");
    let (code, _) = refused(f.launcher("tb-refuse", "proj-a", "codex"));
    assert_eq!(code, 3, "assigned for claude only");
    f.set_status(&id, "revoked").await;
    let (code, err) = refused(f.launcher("tb-refuse", "proj-a", "claude"));
    assert_eq!(code, 3);
    assert!(err.contains("revoked"), "{err}");
    let (code, _) = refused(f.launcher("no-such", "proj-a", "claude"));
    assert_eq!(code, 3);
    let (_, err) = refused(f.launcher("tb-refuse", "proj-a", "claude"));
    assert!(!err.contains("LAUNCHERFIXTURE"), "no secret on stderr");

    // Missing DB: refused, and the file is never created (read-only open).
    let empty = f._dir.path().join("empty-data-dir");
    std::fs::create_dir_all(&empty).unwrap();
    let (code, err) = refused(launcher_cmd(&empty, "tb-refuse", "proj-a", "claude"));
    assert_eq!(code, 3, "{err}");
    assert!(!empty.join("aionui-backend.db").exists());

    // Relative data dir: usage error.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    cmd.args([
        "mcp",
        "exec",
        "tb-refuse",
        "--project",
        "proj-a",
        "--cli",
        "claude",
        "--data-dir",
        "rel",
    ])
    .env_remove("AIONUI_DATA_DIR");
    let (code, _) = refused(cmd);
    assert_eq!(code, 2);
    let _ = &f.db;
}

#[tokio::test(flavor = "multi_thread")]
async fn proxies_filtered_tools_with_an_isolated_env_and_stops_on_revoke() {
    let f = fx().await;
    let id = f.instance("tb-run", true).await;
    f.assign(&id, "claude").await;

    let mut s = Session::start(f.launcher("tb-run", "proj-a", "claude"));
    let init = s.call(rpc(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    ));
    assert_eq!(init["result"]["serverInfo"]["name"], "fake");

    // tools/list: `forbidden` is not on the allowlist.
    let list = s.call(rpc(2, "tools/list", json!({})));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["echo", "env"]);

    // tools/call of a filtered tool: refused by the proxy.
    let blocked = s.call(rpc(3, "tools/call", json!({"name": "forbidden", "arguments": {}})));
    assert_eq!(blocked["error"]["code"], -32602);

    // The child's env: allowlist + plain env + this instance's secret only.
    let env_call = s.call(rpc(4, "tools/call", json!({"name": "env", "arguments": {}})));
    let env: serde_json::Map<String, Value> =
        serde_json::from_str(env_call["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        env["FAKE_API_KEY"], FIXTURE_SECRET,
        "decrypted secret reaches the child"
    );
    assert_eq!(env["FAKE_MODE"], "read-only");
    assert!(env.contains_key("PATH"));
    for leaked in [
        "AIONUI_RUNTIME_TOKEN",
        "AIONUI_USER_ID",
        "PARENT_ONLY_VAR",
        "GITHUB_TOKEN",
        "AIONUI_DATA_DIR",
    ] {
        assert!(!env.contains_key(leaked), "{leaked} leaked into the MCP child");
    }
    assert!(!env_call.to_string().contains("SENTINEL"));

    // Revoke while running: the next call (after the 2 s gate cache) is refused
    // and the launcher exits with code 3.
    f.set_status(&id, "revoked").await;
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let refused_call = s.call(rpc(5, "tools/call", json!({"name": "echo", "arguments": {}})));
    assert!(
        refused_call["error"]["message"].as_str().unwrap().contains("revoked"),
        "{refused_call}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = s.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "launcher did not stop after revoke");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(3));
    let mut stderr = String::new();
    std::io::Read::read_to_string(s.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(
        !stderr.contains("LAUNCHERFIXTURE") && !stderr.contains("SENTINEL"),
        "{stderr}"
    );
}
