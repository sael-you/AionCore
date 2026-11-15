//! Built-in MCP catalogue (D49/D50, C23): seeding, launch parameters (G1),
//! R11 picks (G2) and proxy-enforced read tiers (G3), against a real
//! in-memory DB and the catalogue shipped in the binary.

use std::collections::BTreeMap;
use std::sync::Arc;

use aionui_api_types::{
    CreateMcpMarketAssignmentRequest, CreateMcpMarketInstanceRequest, UpdateMcpMarketInstanceRequest,
};
use aionui_db::{
    IMcpMarketRepository, IMcpServerRepository, SqliteMcpMarketRepository, SqliteMcpServerRepository,
    init_database_memory,
};
use aionui_mcp::market::{
    LaunchRequest, LaunchScope, MarketError, builtin_entries, parse_template, resolve_launch, with_builtin,
};
use aionui_mcp::{McpMarketService, StaticTemplateCatalog};
use serde_json::json;

const USER: &str = "system_default_user";
const KEY: [u8; 32] = [9u8; 32];
const PAT: &str = "sbp_FIXTUREVALUE0123456789abcdef";
const REF: &str = "abcdefghijklmnopqrst";

/// The final D50 list.
const EXPECTED: &[&str] = &[
    "airtable",
    "brave-search",
    "brevo",
    "chrome-devtools",
    "cloudflare",
    "context7",
    "deepwiki",
    "exa",
    "firecrawl",
    "ga4",
    "github",
    "google-ads",
    "google-workspace",
    "gsc",
    "keyword-planner",
    "meta-ads",
    "minimax",
    "paddle",
    "paddle-sandbox",
    "perplexity",
    "posthog",
    "reddit",
    "resend",
    "sentry",
    "stripe",
    "supabase",
];

async fn market() -> (McpMarketService, Arc<dyn IMcpMarketRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let catalog = StaticTemplateCatalog::new(builtin_entries());
    sqlx::query(
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    (
        McpMarketService::new(repo.clone(), servers, Arc::new(catalog), KEY),
        repo,
        db,
    )
}

fn invalid(err: MarketError) -> String {
    match err {
        MarketError::Invalid(msg) => msg,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

fn instance(name: &str, template: &str, env: &str, tier: &str, clis: &[&str]) -> CreateMcpMarketInstanceRequest {
    CreateMcpMarketInstanceRequest {
        name: name.into(),
        template_id: template.into(),
        description: None,
        environment: env.into(),
        tier: tier.into(),
        status: None,
        clis: clis.iter().map(|c| c.to_string()).collect(),
        plain_env: BTreeMap::new(),
        tools_allow: None,
        tools_block: vec![],
    }
}

fn status(s: &str) -> UpdateMcpMarketInstanceRequest {
    UpdateMcpMarketInstanceRequest {
        status: Some(s.into()),
        ..Default::default()
    }
}

// -- seeding --------------------------------------------------------------------

#[tokio::test]
async fn builtin_catalogue_serves_every_template() {
    let entries = builtin_entries();
    assert_eq!(entries.len(), EXPECTED.len());
    for entry in &entries {
        let id = entry["id"].as_str().unwrap_or("?");
        let t = parse_template(entry).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert!(
            t.category.is_some() && t.vendor.is_some() && t.summary.is_some(),
            "{id}: browse metadata"
        );
    }
    let (market, _, _) = market().await;
    let ids: Vec<String> = market
        .list_templates(USER)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, EXPECTED);
    let ga4 = market
        .list_templates(USER)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == "ga4")
        .unwrap();
    assert!(ga4.optional);
    let stripe = market
        .list_templates(USER)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == "stripe")
        .unwrap();
    assert!(
        stripe.env["STRIPE_AGENT_KEY"]
            .description
            .as_deref()
            .unwrap()
            .contains("2026-11-30")
    );
}

#[test]
fn an_installed_catalogue_extension_overrides_builtin_ids_only() {
    let installed = vec![json!({"id": "context7", "name": "Context7 (pinned)", "market": {}})];
    let merged = with_builtin(installed);
    assert_eq!(merged.len(), EXPECTED.len());
    assert_eq!(merged[0]["name"], "Context7 (pinned)");
    assert_eq!(merged.iter().filter(|e| e["id"] == "context7").count(), 1);
}

// -- G1 launch parameters ---------------------------------------------------------

#[tokio::test]
async fn g1_project_ref_is_validated_and_supabase_write_stays_dormant_without_it() {
    let (market, repo, _db) = market().await;
    // A bad ref is refused at write time.
    let mut bad = instance("tb-sb-preprod", "supabase", "staging", "write", &["claude"]);
    bad.plain_env
        .insert("SUPABASE_PROJECT_REF".into(), "abc&read_only=false".into());
    assert!(invalid(market.create_instance(USER, bad).await.unwrap_err()).contains("SUPABASE_PROJECT_REF"));

    // Without the ref the write instance cannot be activated, even with its token.
    let inst = market
        .create_instance(
            USER,
            instance("tb-sb-preprod", "supabase", "staging", "write", &["claude"]),
        )
        .await
        .unwrap();
    assert_eq!(inst.status, "dormant");
    market
        .set_secret(USER, &inst.id, "SUPABASE_ACCESS_TOKEN", PAT)
        .await
        .unwrap();
    let err = invalid(
        market
            .update_instance(USER, &inst.id, status("active"))
            .await
            .unwrap_err(),
    );
    assert!(err.contains("SUPABASE_PROJECT_REF"), "{err}");

    // With a valid ref it activates, and the launcher pins it into the bridge URL.
    market
        .update_instance(
            USER,
            &inst.id,
            UpdateMcpMarketInstanceRequest {
                status: Some("active".into()),
                plain_env: Some(BTreeMap::from([("SUPABASE_PROJECT_REF".into(), REF.into())])),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    market
        .create_assignments(
            USER,
            CreateMcpMarketAssignmentRequest {
                instance_id: inst.id.clone(),
                project_id: Some("proj-a".into()),
                assistant_id: None,
                clis: vec!["claude".into()],
            },
        )
        .await
        .unwrap();
    let req = LaunchRequest {
        user_id: USER,
        instance: "tb-sb-preprod",
        scope: LaunchScope::Project {
            project_id: "proj-a",
            assistant_id: None,
        },
        cli: "claude",
    };
    let plan = resolve_launch(repo.as_ref(), &KEY, &req).await.unwrap();
    let url = format!("https://mcp.supabase.com/mcp?project_ref={REF}&read_only=false&");
    assert!(plan.args.iter().any(|a| a.starts_with(&url)), "{:?}", plan.args);
    assert!(!plan.args.iter().any(|a| a.contains("${SUPABASE_PROJECT_REF}")));
    assert!(
        plan.env
            .iter()
            .any(|(k, v)| k == "MCP_SERVER_URL" && v.starts_with(&url))
    );

    // The read tier forces read_only=true into the same URL.
    let read = market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                plain_env: BTreeMap::from([("SUPABASE_PROJECT_REF".into(), REF.into())]),
                tools_allow: Some(vec!["list_tables".into()]),
                ..instance("tb-sb-prod", "supabase", "production", "read", &["claude", "codex"])
            },
        )
        .await
        .unwrap();
    assert_eq!(read.plain_env["SUPABASE_READ_ONLY"], "true");
}

// -- G2 R11 ------------------------------------------------------------------------

#[tokio::test]
async fn g2_github_and_chrome_devtools_are_in_the_market() {
    let (market, _, _db) = market().await;
    let ids: Vec<String> = market
        .list_templates(USER)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert!(ids.contains(&"github".to_owned()) && ids.contains(&"chrome-devtools".to_owned()));
    market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                tools_block: vec!["delete_file".into()],
                ..instance("github-arai", "github", "production", "write", &["claude"])
            },
        )
        .await
        .unwrap();
}

// -- G3 proxy enforcement ------------------------------------------------------------

#[tokio::test]
async fn g3_proxy_enforcement_gives_a_production_read_tier_limited_to_the_pinned_allowlist() {
    let (market, repo, _db) = market().await;
    // Production read is accepted, and starts with the template's pinned allowlist.
    let inst = market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                status: Some("active".into()),
                ..instance(
                    "lighthouse-prod",
                    "chrome-devtools",
                    "production",
                    "read",
                    &["claude", "codex", "gemini"],
                )
            },
        )
        .await
        .unwrap();
    assert_eq!(inst.enforcement, "proxy");
    let allow = inst.tools_allow.clone().unwrap();
    assert!(allow.contains(&"lighthouse_audit".to_owned()) && !allow.contains(&"evaluate_script".to_owned()));

    // An allowlist may not reach beyond the pinned read tools.
    let mut wider = instance("lighthouse-wide", "chrome-devtools", "production", "read", &["claude"]);
    wider.tools_allow = Some(vec!["lighthouse_audit".into(), "evaluate_script".into()]);
    assert!(invalid(market.create_instance(USER, wider).await.unwrap_err()).contains("evaluate_script"));

    // The proxy policy blocks every non-allowlisted tool.
    market
        .create_assignments(
            USER,
            CreateMcpMarketAssignmentRequest {
                instance_id: inst.id.clone(),
                project_id: Some("proj-a".into()),
                assistant_id: None,
                clis: vec!["codex".into()],
            },
        )
        .await
        .unwrap();
    let req = LaunchRequest {
        user_id: USER,
        instance: "lighthouse-prod",
        scope: LaunchScope::Project {
            project_id: "proj-a",
            assistant_id: None,
        },
        cli: "codex",
    };
    let plan = resolve_launch(repo.as_ref(), &KEY, &req).await.unwrap();
    assert!(plan.policy.permits("lighthouse_audit"));
    for blocked in ["evaluate_script", "click", "fill_form", "unknown_tool"] {
        assert!(!plan.policy.permits(blocked), "{blocked}");
    }

    // A tools-only template without a pinned allowlist still has no production read tier.
    let brevo = CreateMcpMarketInstanceRequest {
        tools_allow: Some(vec!["get_contacts".into()]),
        ..instance("brevo-prod", "brevo", "production", "read", &["claude"])
    };
    assert!(invalid(market.create_instance(USER, brevo).await.unwrap_err()).contains("R6"));
}
