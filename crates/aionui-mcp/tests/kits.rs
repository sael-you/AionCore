//! Kits + four attach levels (card C27) against a real in-memory DB: kit
//! validation and seed, R2/R3 on global attachments, the effective set with
//! provenance, the ONE market gate honouring kit grants and kit removes, and
//! the launcher's `--conversation` scope following attachments.

use std::collections::BTreeMap;
use std::sync::Arc;

use aionui_api_types::{
    AttachKitItemRequest, CreateKitRequest, CreateMcpMarketAssignmentRequest, CreateMcpMarketInstanceRequest,
    KitEffectiveQuery, KitErrorCode, KitItemKind, KitLevel, KitOp, UpdateKitRequest, UpdateMcpMarketInstanceRequest,
};
use aionui_db::{
    Database, IKitRepository, IMcpMarketRepository, IMcpServerRepository, McpMarketScope, SqliteKitRepository,
    SqliteMcpMarketRepository, SqliteMcpServerRepository, init_database_memory,
};
use aionui_mcp::market::{LaunchRequest, LaunchScope, MarketError, resolve_launch};
use aionui_mcp::{McpMarketService, StaticTemplateCatalog};
use serde_json::json;

const USER: &str = "system_default_user";
const KEY: [u8; 32] = [5u8; 32];
const FIXTURE_SECRET: &str = "pdl_live_KITFIXTURESECRET0123456789";

fn catalog() -> StaticTemplateCatalog {
    StaticTemplateCatalog::new(vec![
        json!({"id": "paddle", "name": "Paddle", "market": {
            "transport": "stdio",
            "command": {"command": "npx", "args": ["-y", "@paddle/paddle-mcp@0.1.0"]},
            "env": {
                "PADDLE_API_KEY": {"kind": "secret", "required": true},
                "PADDLE_ENVIRONMENT": {"kind": "plain", "required": true, "enum": ["production", "sandbox"]}
            },
            "tiers": ["read", "write"],
            "enforcement": "server",
            "clis": ["claude", "codex", "gemini"]
        }}),
        json!({"id": "context7", "name": "Context7", "market": {
            "transport": "http", "url": "https://mcp.context7.com/mcp",
            "tiers": ["read"], "enforcement": "server", "clis": ["claude", "codex", "gemini"]
        }}),
    ])
}

struct Fx {
    db: Database,
    market: McpMarketService,
    repo: Arc<dyn IMcpMarketRepository>,
    servers: Arc<dyn IMcpServerRepository>,
    kits: Arc<dyn IKitRepository>,
}

async fn fx() -> Fx {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let kits: Arc<dyn IKitRepository> = Arc::new(SqliteKitRepository::new(db.pool().clone()));
    let market = McpMarketService::new(repo.clone(), servers.clone(), Arc::new(catalog()), KEY).with_kits(kits.clone());
    for sql in [
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)",
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) \
         VALUES ('conv-a', 'system_default_user', 'c', 'acp', 'pending', 1, 1, 'proj-a', '{\"preset_assistant_id\":\"asst-1\",\"skills\":[\"cron\"]}')",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    Fx {
        db,
        market,
        repo,
        servers,
        kits,
    }
}

fn paddle(name: &str, tier: &str) -> CreateMcpMarketInstanceRequest {
    CreateMcpMarketInstanceRequest {
        name: name.to_owned(),
        template_id: "paddle".into(),
        description: None,
        environment: "production".into(),
        tier: tier.into(),
        status: None,
        clis: vec!["claude".into(), "codex".into()],
        plain_env: BTreeMap::from([("PADDLE_ENVIRONMENT".into(), "production".into())]),
        tools_allow: (tier == "read").then(|| vec!["list_products".into()]),
        tools_block: vec![],
    }
}

/// An active paddle instance; returns its id.
async fn active_paddle(f: &Fx, name: &str) -> String {
    let inst = f.market.create_instance(USER, paddle(name, "read")).await.unwrap();
    f.market
        .set_secret(USER, &inst.id, "PADDLE_API_KEY", FIXTURE_SECRET)
        .await
        .unwrap();
    f.market
        .update_instance(
            USER,
            &inst.id,
            UpdateMcpMarketInstanceRequest {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    inst.id
}

async fn kit(f: &Fx, name: &str, skills: &[&str], mcps: &[&str]) -> String {
    f.market
        .create_kit(
            USER,
            CreateKitRequest {
                name: name.into(),
                description: None,
                skills: skills.iter().map(|s| s.to_string()).collect(),
                mcp_instance_ids: mcps.iter().map(|s| s.to_string()).collect(),
                mcp_templates: vec![],
            },
        )
        .await
        .unwrap()
        .id
}

fn attach(level: KitLevel, target: &str, kind: KitItemKind, item: &str, op: KitOp) -> AttachKitItemRequest {
    AttachKitItemRequest {
        level,
        target_id: (!target.is_empty()).then(|| target.to_owned()),
        item_kind: kind,
        item_ref: item.to_owned(),
        op,
    }
}

/// The kit code of a refusal, asserting it.
fn coded(err: MarketError, want: KitErrorCode) -> String {
    match err {
        MarketError::Kit(code, msg) if code == want => msg,
        other => panic!("expected {}, got {other:?}", want.as_str()),
    }
}

#[tokio::test]
async fn list_seeds_the_twelve_default_kits_once_without_instances() {
    let f = fx().await;
    let kits = f.market.list_kits(USER).await.unwrap();
    assert_eq!(kits.len(), 12);
    assert_eq!(kits[0].builtin_key.as_deref(), Some("K1"));
    assert_eq!(kits[0].name, "Platform Dev");
    assert!(kits[0].skills.contains(&"pf-core".to_owned()));
    assert_eq!(kits[0].mcp_templates, vec!["context7", "deepwiki", "github"]);
    assert!(kits.iter().all(|k| k.mcps.is_empty()), "seeding creates no instance");
    assert_eq!(f.market.list_kits(USER).await.unwrap().len(), 12);
    assert!(f.repo.list_instances(USER).await.unwrap().is_empty());
}

#[tokio::test]
async fn kit_members_are_validated() {
    let f = fx().await;
    let bad = f
        .market
        .create_kit(
            USER,
            CreateKitRequest {
                name: "X".into(),
                skills: vec!["../etc".into()],
                mcp_instance_ids: vec!["mcp-missing".into()],
                mcp_templates: vec!["no-such-template".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    let msg = coded(bad, KitErrorCode::InvalidItem);
    assert!(msg.contains("not a valid skill name"), "{msg}");
    assert!(msg.contains("mcp-missing does not exist"), "{msg}");
    assert!(msg.contains("no-such-template is not in the MCP catalogue"), "{msg}");

    kit(&f, "Same", &[], &[]).await;
    let dup = f
        .market
        .create_kit(
            USER,
            CreateKitRequest {
                name: " Same ".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    coded(dup, KitErrorCode::NameTaken);
    let empty = f
        .market
        .create_kit(
            USER,
            CreateKitRequest {
                name: "  ".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(coded(empty, KitErrorCode::InvalidName).contains("kit name"));

    // A stale expected_updated_at is KIT_STALE, not a name clash.
    let id = kit(&f, "Versioned", &[], &[]).await;
    let first = f.market.get_kit(USER, &id).await.unwrap();
    f.market
        .update_kit(
            USER,
            &id,
            UpdateKitRequest {
                description: Some("v2".into()),
                expected_updated_at: Some(first.updated_at),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let stale = f
        .market
        .update_kit(
            USER,
            &id,
            UpdateKitRequest {
                description: Some("v3".into()),
                expected_updated_at: Some(first.updated_at),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    coded(stale, KitErrorCode::Stale);
    let renamed = f
        .market
        .update_kit(
            USER,
            &id,
            UpdateKitRequest {
                name: Some("Same".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    coded(renamed, KitErrorCode::NameTaken);
}

#[tokio::test]
async fn global_attachments_keep_r2_and_r3() {
    let f = fx().await;
    let pdl = active_paddle(&f, "tb-pdl-read").await;
    let ctx = f
        .market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                name: "context7".into(),
                template_id: "context7".into(),
                description: None,
                environment: "production".into(),
                tier: "read".into(),
                status: None,
                clis: vec!["claude".into()],
                plain_env: BTreeMap::new(),
                tools_allow: Some(vec!["resolve-library-id".into()]),
                tools_block: vec![],
            },
        )
        .await
        .unwrap();
    let k = kit(&f, "Payments", &["pricing-strategy"], &[&pdl]).await;

    let err = f
        .market
        .attach_kit_item(USER, attach(KitLevel::Global, "", KitItemKind::Kit, &k, KitOp::Add))
        .await
        .unwrap_err();
    assert!(coded(err, KitErrorCode::GlobalNotAllowed).contains("tb-pdl-read"));
    let err = f
        .market
        .attach_kit_item(USER, attach(KitLevel::Global, "", KitItemKind::Mcp, &pdl, KitOp::Add))
        .await
        .unwrap_err();
    assert!(coded(err, KitErrorCode::GlobalNotAllowed).contains("R2/R3"));
    // context7 may be global; a global target_id is refused.
    f.market
        .attach_kit_item(
            USER,
            attach(KitLevel::Global, "", KitItemKind::Mcp, &ctx.id, KitOp::Add),
        )
        .await
        .unwrap();
    let err = f
        .market
        .attach_kit_item(
            USER,
            attach(KitLevel::Global, "x", KitItemKind::Skill, "pdf", KitOp::Add),
        )
        .await
        .unwrap_err();
    assert!(coded(err, KitErrorCode::InvalidTarget).contains("no target_id"));

    // A globally attached kit cannot gain a non-global instance later.
    let ctx_kit = kit(&f, "Docs", &[], &[&ctx.id]).await;
    f.market
        .attach_kit_item(
            USER,
            attach(KitLevel::Global, "", KitItemKind::Kit, &ctx_kit, KitOp::Add),
        )
        .await
        .unwrap();
    let err = f
        .market
        .update_kit(
            USER,
            &ctx_kit,
            UpdateKitRequest {
                mcp_instance_ids: Some(vec![ctx.id.clone(), pdl.clone()]),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(coded(err, KitErrorCode::GlobalNotAllowed).contains("attached globally"));
}

#[tokio::test]
async fn attach_targets_must_exist() {
    let f = fx().await;
    let err = f
        .market
        .attach_kit_item(
            USER,
            attach(KitLevel::Project, "nope", KitItemKind::Skill, "pdf", KitOp::Add),
        )
        .await
        .unwrap_err();
    coded(err, KitErrorCode::UnknownTarget);
    let err = f
        .market
        .attach_kit_item(
            USER,
            attach(KitLevel::Conversation, "nope", KitItemKind::Skill, "pdf", KitOp::Add),
        )
        .await
        .unwrap_err();
    coded(err, KitErrorCode::UnknownTarget);
    let err = f
        .market
        .attach_kit_item(
            USER,
            attach(KitLevel::Assistant, "", KitItemKind::Skill, "pdf", KitOp::Add),
        )
        .await
        .unwrap_err();
    assert!(coded(err, KitErrorCode::InvalidTarget).contains("needs a target_id"));
    let err = f
        .market
        .attach_kit_item(
            USER,
            attach(KitLevel::Project, "proj-a", KitItemKind::Kit, "kit_x", KitOp::Add),
        )
        .await
        .unwrap_err();
    coded(err, KitErrorCode::NotFound);
    let err = f
        .market
        .attach_kit_item(
            USER,
            attach(KitLevel::Project, "proj-a", KitItemKind::Mcp, "mcp_x", KitOp::Add),
        )
        .await
        .unwrap_err();
    coded(err, KitErrorCode::InvalidItem);
    let err = f.market.detach_kit_item(USER, "kita_x").await.unwrap_err();
    coded(err, KitErrorCode::AttachmentNotFound);
}

#[tokio::test]
async fn effective_set_resolves_levels_with_provenance() {
    let f = fx().await;
    let pdl = active_paddle(&f, "tb-pdl-read").await;
    let k = kit(&f, "SEO", &["seo-audit", "ai-seo"], &[&pdl]).await;
    f.kits
        .update_kit(
            USER,
            &k,
            &aionui_db::UpdateKitParams {
                mcp_templates: Some(&["gsc".to_owned()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let m = &f.market;
    m.attach_kit_item(
        USER,
        attach(KitLevel::Project, "proj-a", KitItemKind::Kit, &k, KitOp::Add),
    )
    .await
    .unwrap();
    m.attach_kit_item(
        USER,
        attach(
            KitLevel::Assistant,
            "asst-1",
            KitItemKind::Skill,
            "ai-seo",
            KitOp::Remove,
        ),
    )
    .await
    .unwrap();
    let conv_remove = m
        .attach_kit_item(
            USER,
            attach(KitLevel::Conversation, "conv-a", KitItemKind::Mcp, &pdl, KitOp::Remove),
        )
        .await
        .unwrap();

    let set = m
        .kit_effective_set(
            USER,
            KitEffectiveQuery {
                conversation_id: Some("conv-a".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(set.project_id.as_deref(), Some("proj-a"));
    assert_eq!(set.assistant_id.as_deref(), Some("asst-1"));
    assert_eq!(set.conversation_skills, Some(vec!["cron".to_owned()]));
    let seo = set.skills.iter().find(|i| i.item_ref == "seo-audit").unwrap();
    assert!(seo.included);
    assert_eq!(seo.provenance.level, KitLevel::Project);
    assert_eq!(seo.provenance.kit_name.as_deref(), Some("SEO"));
    let ai = set.skills.iter().find(|i| i.item_ref == "ai-seo").unwrap();
    assert!(!ai.included);
    assert_eq!(
        (ai.provenance.level, ai.provenance.op),
        (KitLevel::Assistant, KitOp::Remove)
    );
    let mcp = &set.mcps[0];
    assert!(!mcp.included);
    let detail = mcp.mcp.as_ref().unwrap();
    assert_eq!(detail.instance_name, "tb-pdl-read");
    assert!(!detail.delivered);
    assert_eq!(set.missing_templates.len(), 1);
    assert_eq!(set.missing_templates[0].template_id, "gsc");

    // Detaching the conversation-level remove brings the instance back.
    m.detach_kit_item(USER, &conv_remove.id).await.unwrap();
    let set = m
        .kit_effective_set(
            USER,
            KitEffectiveQuery {
                conversation_id: Some("conv-a".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let detail = set.mcps[0].mcp.as_ref().unwrap();
    assert!(set.mcps[0].included && detail.delivered, "{set:?}");
    assert_eq!(detail.tier.as_deref(), Some("read"));

    // Preview without a conversation: project only, assistant level ignored.
    let preview = m
        .kit_effective_set(
            USER,
            KitEffectiveQuery {
                project_id: Some("proj-a".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(preview.skills.iter().all(|i| i.included));
    assert!(preview.conversation_skills.is_none());
}

fn scope<'a>(cli: &'a str) -> McpMarketScope<'a> {
    McpMarketScope {
        conversation_id: Some("conv-a"),
        assistant_id: Some("asst-1"),
        cli: Some(cli),
    }
}

#[tokio::test]
async fn market_gate_delivers_kit_grants_and_honours_kit_removes() {
    let f = fx().await;
    let pdl = active_paddle(&f, "tb-pdl-read").await;
    let m = &f.market;
    assert!(f.servers.kit_mcp_ids(USER, "conv-a").await.unwrap().is_empty());
    let rows = f
        .servers
        .list_by_ids_any(USER, std::slice::from_ref(&pdl))
        .await
        .unwrap();
    // Without kit or assignment: dropped.
    assert!(
        f.servers
            .market_gate(USER, &scope("claude"), rows.clone())
            .await
            .unwrap()
            .is_empty()
    );

    let k = kit(&f, "Payments", &[], &[&pdl]).await;
    m.attach_kit_item(
        USER,
        attach(KitLevel::Project, "proj-a", KitItemKind::Kit, &k, KitOp::Add),
    )
    .await
    .unwrap();
    assert_eq!(f.servers.kit_mcp_ids(USER, "conv-a").await.unwrap(), vec![pdl.clone()]);
    let gated = f
        .servers
        .market_gate(USER, &scope("claude"), rows.clone())
        .await
        .unwrap();
    assert_eq!(gated.len(), 1);
    let delivery = gated[0].market.as_ref().unwrap();
    assert_eq!(delivery.conversation_id.as_deref(), Some("conv-a"));
    assert_eq!(delivery.project_id, None);
    // A CLI the instance is not enabled for gets nothing.
    assert!(
        f.servers
            .market_gate(USER, &scope("gemini"), rows.clone())
            .await
            .unwrap()
            .is_empty()
    );

    // An assigned instance removed by a conversation-level kit remove is dropped.
    m.create_assignments(
        USER,
        CreateMcpMarketAssignmentRequest {
            instance_id: pdl.clone(),
            project_id: Some("proj-a".into()),
            assistant_id: Some("asst-1".into()),
            clis: vec!["claude".into()],
        },
    )
    .await
    .unwrap();
    m.attach_kit_item(
        USER,
        attach(KitLevel::Conversation, "conv-a", KitItemKind::Kit, &k, KitOp::Remove),
    )
    .await
    .unwrap();
    assert!(
        f.servers
            .market_gate(USER, &scope("claude"), rows.clone())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(f.servers.kit_mcp_ids(USER, "conv-a").await.unwrap().is_empty());
    // Snapshot time (no conversation): status check only, unchanged.
    let snapshot = f
        .servers
        .market_gate(USER, &McpMarketScope::default(), rows)
        .await
        .unwrap();
    assert_eq!(snapshot.len(), 1);
    let _ = &f.db;
}

/// TASK-97 x C27: a production WRITE instance delivered through a kit grant
/// carries its environment, so every CLI is made to ask before each call; a
/// kit remove still drops it.
#[tokio::test]
async fn kit_granted_production_write_instance_requires_owner_approval() {
    let f = fx().await;
    let m = &f.market;
    let inst = m.create_instance(USER, paddle("tb-pdl-live", "write")).await.unwrap();
    m.set_secret(USER, &inst.id, "PADDLE_API_KEY", FIXTURE_SECRET)
        .await
        .unwrap();
    m.update_instance(
        USER,
        &inst.id,
        UpdateMcpMarketInstanceRequest {
            status: Some("active".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let k = kit(&f, "Payments live", &[], &[&inst.id]).await;
    m.attach_kit_item(
        USER,
        attach(KitLevel::Project, "proj-a", KitItemKind::Kit, &k, KitOp::Add),
    )
    .await
    .unwrap();
    let rows = f
        .servers
        .list_by_ids_any(USER, std::slice::from_ref(&inst.id))
        .await
        .unwrap();
    for cli in ["claude", "codex"] {
        let gated = f.servers.market_gate(USER, &scope(cli), rows.clone()).await.unwrap();
        assert_eq!(gated.len(), 1, "{cli}: kit grant delivers the instance");
        let delivery = gated[0].market.as_ref().unwrap();
        assert_eq!(
            delivery.conversation_id.as_deref(),
            Some("conv-a"),
            "{cli}: via the kit"
        );
        assert_eq!(delivery.environment, "production");
        assert_eq!(delivery.tier, "write");
        assert!(delivery.requires_owner_approval(), "{cli}: ask before every call");
    }

    m.attach_kit_item(
        USER,
        attach(KitLevel::Conversation, "conv-a", KitItemKind::Kit, &k, KitOp::Remove),
    )
    .await
    .unwrap();
    assert!(
        f.servers
            .market_gate(USER, &scope("claude"), rows)
            .await
            .unwrap()
            .is_empty(),
        "a kit remove still drops the row"
    );
}

#[tokio::test]
async fn launcher_conversation_scope_follows_attachments() {
    let f = fx().await;
    let pdl = active_paddle(&f, "tb-pdl-read").await;
    let req = LaunchRequest {
        user_id: USER,
        instance: "tb-pdl-read",
        scope: LaunchScope::Conversation {
            conversation_id: "conv-a",
        },
        cli: "claude",
    };
    let err = resolve_launch(f.repo.as_ref(), &KEY, &req).await.unwrap_err();
    assert!(
        matches!(&err, MarketError::Forbidden(m) if m.contains("not granted")),
        "{err:?}"
    );

    let a = f
        .market
        .attach_kit_item(
            USER,
            attach(KitLevel::Conversation, "conv-a", KitItemKind::Mcp, &pdl, KitOp::Add),
        )
        .await
        .unwrap();
    let plan = resolve_launch(f.repo.as_ref(), &KEY, &req).await.unwrap();
    assert_eq!(plan.instance_name, "tb-pdl-read");
    // Wrong CLI still refused by the instance's own CLI list.
    let gemini = LaunchRequest { cli: "gemini", ..req };
    assert!(resolve_launch(f.repo.as_ref(), &KEY, &gemini).await.is_err());

    f.market.detach_kit_item(USER, &a.id).await.unwrap();
    let err = resolve_launch(f.repo.as_ref(), &KEY, &req).await.unwrap_err();
    assert!(
        matches!(&err, MarketError::Forbidden(m) if m.contains("not granted")),
        "{err:?}"
    );
}

#[tokio::test]
async fn deleting_a_kit_detaches_it_everywhere() {
    let f = fx().await;
    let k = kit(&f, "Ads", &["paid-ads"], &[]).await;
    f.market
        .attach_kit_item(
            USER,
            attach(KitLevel::Project, "proj-a", KitItemKind::Kit, &k, KitOp::Add),
        )
        .await
        .unwrap();
    f.market.delete_kit(USER, &k).await.unwrap();
    assert!(
        f.market
            .list_kit_attachments(USER, Default::default())
            .await
            .unwrap()
            .is_empty()
    );
    coded(f.market.delete_kit(USER, &k).await.unwrap_err(), KitErrorCode::NotFound);
    coded(f.market.get_kit(USER, &k).await.unwrap_err(), KitErrorCode::NotFound);
}
