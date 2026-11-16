//! "Import keys from this project's .mcp.json" (card C28 §5) against a real
//! in-memory DB and the built-in catalogue: the preview never returns a value
//! or a command, imported instances run the template's pinned command (the
//! file's command/args are ignored), aliases map old env names, market rules
//! still apply, and only the project's own workspace file is read.

use std::sync::Arc;

use aionui_api_types::{KitErrorCode, KitImportMatch, KitImportRequest, KitImportServerRequest};
use aionui_db::{
    IKitRepository, IMcpMarketRepository, IMcpServerRepository, SqliteKitRepository, SqliteMcpMarketRepository,
    SqliteMcpServerRepository, init_database_memory,
};
use aionui_mcp::market::{MarketError, with_builtin};
use aionui_mcp::{McpMarketService, StaticTemplateCatalog};

const USER: &str = "system_default_user";
const KEY: [u8; 32] = [3u8; 32];
const TOKEN: &str = "pdl_sdbx_IMPORTFIXTURETOKEN0123456789";

fn file() -> String {
    format!(
        r#"{{"mcpServers": {{
        "tb-paddle-sandbox": {{"command": "node", "args": ["/old/paddle-mcp-0.0.1.js", "--evil"], "env": {{"PADDLE_API_KEY": "{TOKEN}"}}}},
        "pdl-nokey": {{"type": "http", "url": "https://sandbox-mcp.paddle.com/mcp"}},
        "github": {{"command": "docker", "args": ["run", "ghcr.io/github/github-mcp-server"], "env": {{"GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_ALIASFIXTURE"}}}},
        "tavily": {{"command": "npx", "args": ["-y", "tavily-mcp@0.1.0"], "env": {{"TAVILY_API_KEY": "tvly-SECRETFIXTURE"}}}},
        "tb-protocol": {{"command": "node", "args": ["/srv/tb/protocol.js"]}}
    }}}}"#
    )
}

struct Fx {
    db: aionui_db::Database,
    market: McpMarketService,
    repo: Arc<dyn IMcpMarketRepository>,
    kits: Arc<dyn IKitRepository>,
    _ws: tempfile::TempDir,
}

async fn fx() -> Fx {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let kits: Arc<dyn IKitRepository> = Arc::new(SqliteKitRepository::new(db.pool().clone()));
    let catalog = StaticTemplateCatalog::new(with_builtin(Vec::new()));
    let market = McpMarketService::new(repo.clone(), servers, Arc::new(catalog), KEY).with_kits(kits.clone());
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join(".mcp.json"), file()).unwrap();
    let uri = url::Url::from_directory_path(ws.path()).unwrap().to_string();
    for (sql, binds) in [
        (
            "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, 'standard', 1, 1)",
            vec!["proj-tb", USER, "TB-test"],
        ),
        (
            "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, 'standard', 1, 1)",
            vec!["proj-empty", USER, "empty"],
        ),
        (
            "INSERT INTO folders (folder_id, resource_uri, resource_canonical, created_at, updated_at) VALUES ('f-tb', ?, ?, 1, 1)",
            vec![&uri, &uri],
        ),
        (
            "INSERT INTO project_explorer (pe_id, project_id, owner_user_id, folder_id, role, created_at, updated_at) \
             VALUES ('pe-tb', 'proj-tb', ?, 'f-tb', 'workspace', 1, 1)",
            vec![USER],
        ),
    ] {
        let mut q = sqlx::query(sql);
        for b in binds {
            q = q.bind(b.to_owned());
        }
        q.execute(db.pool()).await.unwrap();
    }
    Fx {
        db,
        market,
        repo,
        kits,
        _ws: ws,
    }
}

fn server(name: &str, template: &str, instance: &str, env: &str, tier: &str) -> KitImportServerRequest {
    KitImportServerRequest {
        name: name.into(),
        template_id: template.into(),
        instance_name: instance.into(),
        environment: env.into(),
        tier: tier.into(),
    }
}

#[tokio::test]
async fn preview_classifies_servers_and_never_returns_values_or_commands() {
    let f = fx().await;
    let preview = f.market.kit_import_preview(USER, "proj-tb").await.unwrap();
    assert!(preview.file_found);
    let get = |n: &str| preview.servers.iter().find(|s| s.name == n).unwrap();

    let pdl = get("tb-paddle-sandbox");
    assert_eq!(
        (pdl.match_kind, pdl.template_id.as_deref()),
        (KitImportMatch::Catalogue, Some("paddle-sandbox"))
    );
    assert_eq!(pdl.secrets.len(), 1);
    assert!(pdl.secrets[0].found && pdl.secrets[0].field == "PADDLE_API_KEY");

    let nokey = get("pdl-nokey");
    assert_eq!(
        nokey.template_id.as_deref(),
        Some("paddle-sandbox"),
        "matched by URL host"
    );
    assert!(!nokey.secrets[0].found);
    assert!(nokey.oauth_only, "remote entry without any credential");

    // Alias mapping: the old github-mcp-server env name fills GITHUB_PAT.
    let gh = get("github");
    assert_eq!(gh.template_id.as_deref(), Some("github"));
    assert!(gh.secrets.iter().any(|s| s.field == "GITHUB_PAT" && s.found), "{gh:?}");

    assert_eq!(get("tavily").match_kind, KitImportMatch::None);
    assert_eq!(get("tb-protocol").match_kind, KitImportMatch::Custom);
    assert!(get("tb-protocol").secrets.is_empty());

    let wire = serde_json::to_string(&preview).unwrap();
    for leak in [
        TOKEN,
        "ghp_ALIASFIXTURE",
        "tvly-SECRETFIXTURE",
        "/old/paddle",
        "--evil",
        "docker",
        "protocol.js",
        "sandbox-mcp.paddle.com",
    ] {
        assert!(!wire.contains(leak), "preview leaked {leak}: {wire}");
    }
}

#[tokio::test]
async fn import_creates_template_instances_with_file_secrets_and_ignores_the_old_command() {
    let f = fx().await;
    let result = f
        .market
        .kit_import(
            USER,
            "proj-tb",
            KitImportRequest {
                servers: vec![
                    server("tb-paddle-sandbox", "paddle-sandbox", "tb-pdl-sbx", "sandbox", "write"),
                    server("pdl-nokey", "paddle-sandbox", "tb-pdl-sbx-two", "sandbox", "write"),
                    // R1: instance name = template id; tier unknown.
                    server(
                        "tb-paddle-sandbox",
                        "paddle-sandbox",
                        "paddle-sandbox",
                        "sandbox",
                        "write",
                    ),
                    server("tb-paddle-sandbox", "paddle-sandbox", "tb-pdl-bad", "sandbox", "admin"),
                    server("not-there", "paddle-sandbox", "tb-x", "sandbox", "write"),
                    server("tb-protocol", "tb-protocol", "tb-protocol-i", "local", "write"),
                ],
            },
        )
        .await
        .unwrap();
    let r = &result.servers;
    assert_eq!(r.len(), 6);
    let ok = &r[0];
    assert!(ok.error_code.is_none(), "{ok:?}");
    assert!(ok.missing_secrets.is_empty());
    let inst_id = ok.instance_id.clone().unwrap();
    assert_eq!(
        r[1].missing_secrets,
        vec!["PADDLE_API_KEY"],
        "created needs_secret: {:?}",
        r[1]
    );
    assert!(r[1].instance_id.is_some());
    assert_eq!(r[2].error_code.as_deref(), Some("MCP_MARKET_INVALID"));
    assert_eq!(r[3].error_code.as_deref(), Some("MCP_MARKET_INVALID"));
    assert_eq!(r[4].error_code.as_deref(), Some("KIT_IMPORT_NOT_IN_FILE"));
    assert_eq!(r[5].error_code.as_deref(), Some("KIT_IMPORT_UNKNOWN_TEMPLATE"));

    // The secret was copied server-side (status only), never echoed.
    let status = f.market.get_instance(USER, &inst_id).await.unwrap();
    assert_eq!(status.status, "dormant");
    let wire = serde_json::to_string(&result).unwrap() + &serde_json::to_string(&status).unwrap();
    assert!(!wire.contains(TOKEN), "{wire}");
    let secrets = f.repo.list_instance_secrets(USER, &inst_id).await.unwrap();
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0].instance_id, inst_id);

    // Instance launch data = the template's pinned bridge, not the file's command.
    let row = f.repo.find_instance(USER, &inst_id).await.unwrap().unwrap();
    let snapshot = row.market_template.unwrap();
    assert!(snapshot.contains("sandbox-mcp.paddle.com"), "{snapshot}");
    assert!(!snapshot.contains("/old/paddle") && !snapshot.contains("--evil"));
    let template = f
        .market
        .list_templates(USER)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == "paddle-sandbox")
        .unwrap();
    let stored: aionui_api_types::McpMarketTemplate = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(stored.bridge, template.bridge);
    assert_eq!(stored.command, template.command);

    // Both created instances joined the (new) project kit.
    let kit = f.market.get_project_kit(USER, "proj-tb").await.unwrap();
    assert_eq!(kit.id, result.kit_id);
    assert_eq!(kit.mcps.len(), 2);
    assert!(kit.mcps.iter().any(|m| m.instance_id == inst_id));
    // Re-import into the existing kit reuses it.
    let again = f
        .market
        .kit_import(
            USER,
            "proj-tb",
            KitImportRequest {
                servers: vec![server(
                    "tb-paddle-sandbox",
                    "paddle-sandbox",
                    "tb-pdl-sbx-3",
                    "sandbox",
                    "write",
                )],
            },
        )
        .await
        .unwrap();
    assert_eq!(again.kit_id, result.kit_id);
    assert_eq!(
        f.kits
            .list_kit_items(USER, Some(std::slice::from_ref(&kit.id)))
            .await
            .unwrap()
            .len(),
        3
    );
    let _ = &f.db;
}

#[tokio::test]
async fn only_the_projects_own_workspace_file_is_read() {
    let f = fx().await;
    // A project without a workspace: no file, even though the process cwd or
    // other projects have one.
    let preview = f.market.kit_import_preview(USER, "proj-empty").await.unwrap();
    assert!(!preview.file_found && preview.servers.is_empty());
    let err = f.market.kit_import_preview(USER, "nope").await.unwrap_err();
    assert!(
        matches!(err, MarketError::Kit(KitErrorCode::UnknownTarget, _)),
        "{err:?}"
    );
    // Another user's project is not visible.
    let err = f
        .market
        .kit_import_preview("someone-else", "proj-tb")
        .await
        .unwrap_err();
    assert!(
        matches!(err, MarketError::Kit(KitErrorCode::UnknownTarget, _)),
        "{err:?}"
    );
    // A malformed file reads as "no file".
    let dir = f._ws.path().to_path_buf();
    std::fs::write(dir.join(".mcp.json"), "{not json").unwrap();
    assert!(!f.market.kit_import_preview(USER, "proj-tb").await.unwrap().file_found);
}
