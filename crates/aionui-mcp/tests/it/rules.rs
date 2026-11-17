//! Rules system (card C37, TASK-131) against a real in-memory DB and real
//! files: CRUD + caps, `rule` attachments through the kit levels, the block a
//! session gets, the confirmation-gated import, and the files AionUi writes
//! only after the owner hands them over.

use std::path::Path;
use std::sync::Arc;

use aionui_api_types::{
    AttachKitItemRequest, CreateRuleRequest, KitErrorCode, KitItemKind, KitLevel, KitOp, RULE_BODY_MAX_BYTES,
    RuleEffectiveQuery, RuleImportQuery, RuleImportRequest, RuleManageFilesRequest, UpdateRuleRequest,
};
use aionui_db::{
    IKitRepository, IMcpMarketRepository, IMcpServerRepository, IRuleRepository, SqliteKitRepository,
    SqliteMcpMarketRepository, SqliteMcpServerRepository, SqliteRuleRepository, init_database_memory,
};
use aionui_mcp::market::MarketError;
use aionui_mcp::rules::import::{GENERATED_MARKER, POINTER_LINE};
use aionui_mcp::rules::{RuleError, RuleService};
use aionui_mcp::{McpMarketService, StaticTemplateCatalog};

const USER: &str = "system_default_user";

struct Fx {
    _db: aionui_db::Database,
    rules: RuleService,
    market: McpMarketService,
    dir: tempfile::TempDir,
}

/// Project `proj-a` has its workspace at `<tmp>/repo`; the global file is `<tmp>/global/AGENTS.md`.
async fn fx() -> Fx {
    let db = init_database_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("repo")).unwrap();
    std::fs::create_dir_all(dir.path().join("global")).unwrap();
    let repo_uri = url::Url::from_file_path(dir.path().join("repo")).unwrap().to_string();
    for sql in [
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-a', 'system_default_user', 'TB', 'standard', 1, 1)".to_owned(),
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-b', 'system_default_user', 'Other', 'standard', 1, 1)".to_owned(),
        format!("INSERT INTO folders (folder_id, resource_uri, resource_canonical, created_at, updated_at) VALUES ('fld-a', '{repo_uri}', '{repo_uri}', 1, 1)"),
        "INSERT INTO project_explorer (pe_id, project_id, folder_id, role, owner_user_id, created_at, updated_at) VALUES ('pe-a', 'proj-a', 'fld-a', 'workspace', 'system_default_user', 1, 1)".to_owned(),
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) VALUES ('conv-a', 'system_default_user', 'c', 'acp', 'pending', 1, 1, 'proj-a', '{\"preset_assistant_id\":\"asst-1\"}')".to_owned(),
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) VALUES ('conv-b', 'system_default_user', 'c', 'acp', 'pending', 1, 1, 'proj-b', '{}')".to_owned(),
    ] {
        sqlx::query(&sql).execute(db.pool()).await.unwrap();
    }
    let rule_repo: Arc<dyn IRuleRepository> = Arc::new(SqliteRuleRepository::new(db.pool().clone()));
    let kits: Arc<dyn IKitRepository> = Arc::new(SqliteKitRepository::new(db.pool().clone()));
    let rules = RuleService::new(
        rule_repo,
        kits.clone(),
        Some(dir.path().join("global").join("AGENTS.md")),
    );
    let market_repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let mcp: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let market = McpMarketService::new(
        market_repo,
        mcp,
        Arc::new(StaticTemplateCatalog::new(vec![])),
        [3u8; 32],
    )
    .with_kits(kits)
    .with_rules(rules.clone());
    Fx {
        _db: db,
        rules,
        market,
        dir,
    }
}

fn new_rule(name: &str, body: &str) -> CreateRuleRequest {
    CreateRuleRequest {
        name: name.into(),
        description: None,
        body: body.into(),
        enabled: None,
    }
}

fn attach(level: KitLevel, target: &str, id: &str, op: KitOp) -> AttachKitItemRequest {
    AttachKitItemRequest {
        level,
        target_id: (!target.is_empty()).then(|| target.to_owned()),
        item_kind: KitItemKind::Rule,
        item_ref: id.to_owned(),
        op,
    }
}

fn conv(id: &str) -> RuleEffectiveQuery {
    RuleEffectiveQuery {
        conversation_id: Some(id.into()),
        ..Default::default()
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn crud_validates_and_detects_stale_writes() {
    let f = fx().await;
    let r = f
        .rules
        .create(USER, new_rule("Be brief", "Answer in 5 lines."))
        .await
        .unwrap();
    assert_eq!(r.origin, "owner");
    assert!(matches!(
        f.rules.create(USER, new_rule("Be brief", "x")).await,
        Err(RuleError::NameTaken(_))
    ));
    assert!(matches!(
        f.rules.create(USER, new_rule("Empty", "  ")).await,
        Err(RuleError::Invalid(_))
    ));
    let big = "x".repeat(RULE_BODY_MAX_BYTES + 1);
    assert!(
        matches!(f.rules.create(USER, new_rule("Big", &big)).await, Err(RuleError::Invalid(m)) if m.contains("8192"))
    );

    let stale = f
        .rules
        .update(
            USER,
            &r.id,
            UpdateRuleRequest {
                body: Some("New".into()),
                expected_updated_at: Some(r.updated_at - 1),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(stale, Err(RuleError::Stale(_))));
    let ok = f
        .rules
        .update(
            USER,
            &r.id,
            UpdateRuleRequest {
                enabled: Some(false),
                expected_updated_at: Some(r.updated_at),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!ok.enabled);
    assert_eq!(ok.body, "Answer in 5 lines.", "a partial update keeps the body");
    assert!(f.rules.list("other-user").await.unwrap().is_empty());
    assert!(matches!(
        f.rules.get("other-user", &r.id).await,
        Err(RuleError::NotFound(_))
    ));
    f.rules.delete(USER, &r.id).await.unwrap();
    assert!(matches!(f.rules.delete(USER, &r.id).await, Err(RuleError::NotFound(_))));
}

#[tokio::test]
async fn levels_decide_the_block_a_session_gets() {
    let f = fx().await;
    let g = f
        .rules
        .create(USER, new_rule("Global rule", "Never push."))
        .await
        .unwrap();
    let p = f
        .rules
        .create(USER, new_rule("Project rule", "Use pnpm."))
        .await
        .unwrap();
    let c = f
        .rules
        .create(USER, new_rule("Conversation rule", "Reply in French."))
        .await
        .unwrap();
    assert_eq!(
        f.rules.conversation_block(USER, "conv-a").await,
        "",
        "unattached rules do nothing"
    );

    for req in [
        attach(KitLevel::Global, "", &g.id, KitOp::Add),
        attach(KitLevel::Project, "proj-a", &p.id, KitOp::Add),
        attach(KitLevel::Conversation, "conv-a", &c.id, KitOp::Add),
    ] {
        let a = f.market.attach_kit_item(USER, req).await.unwrap();
        assert_eq!(a.item_kind, KitItemKind::Rule);
        assert!(a.item_name.ends_with("rule"));
    }
    let block = f.rules.conversation_block(USER, "conv-a").await;
    assert!(block.starts_with("[Owner Rules]"));
    let pos = |needle: &str| {
        block
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing in {block}"))
    };
    assert!(pos("## Global rule (global)\nNever push.") < pos("## Project rule (project)\nUse pnpm."));
    assert!(pos("## Project rule (project)") < pos("## Conversation rule (conversation)\nReply in French."));

    // Another project's conversation only gets the global rule.
    let other = f.rules.effective(USER, conv("conv-b")).await.unwrap();
    assert_eq!(
        other.rules.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["Global rule"]
    );
    assert_eq!(other.block, f.rules.conversation_block(USER, "conv-b").await);

    // A lower level removes an inherited rule; a disabled rule drops out.
    f.market
        .attach_kit_item(USER, attach(KitLevel::Conversation, "conv-a", &g.id, KitOp::Remove))
        .await
        .unwrap();
    f.rules
        .update(
            USER,
            &p.id,
            UpdateRuleRequest {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let now = f.rules.effective(USER, conv("conv-a")).await.unwrap();
    assert_eq!(
        now.rules.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["Conversation rule"]
    );
    assert!(!now.block.contains("Never push."));

    // Deleting a rule deletes its attachments.
    f.rules.delete(USER, &c.id).await.unwrap();
    assert_eq!(f.rules.conversation_block(USER, "conv-a").await, "");
    assert!(matches!(
        f.rules.effective(USER, conv("nope")).await,
        Err(RuleError::NotFound(_))
    ));
}

#[tokio::test]
async fn attach_refuses_unknown_rules_and_a_scope_over_the_cap() {
    let f = fx().await;
    let err = f
        .market
        .attach_kit_item(USER, attach(KitLevel::Global, "", "rule_nope", KitOp::Add))
        .await
        .unwrap_err();
    assert!(matches!(err, MarketError::Kit(KitErrorCode::InvalidItem, _)));

    let body = "y".repeat(RULE_BODY_MAX_BYTES);
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(
            f.rules
                .create(USER, new_rule(&format!("R{i}"), &body))
                .await
                .unwrap()
                .id,
        );
    }
    for id in &ids[..3] {
        f.market
            .attach_kit_item(USER, attach(KitLevel::Global, "", id, KitOp::Add))
            .await
            .unwrap();
    }
    let err = f
        .market
        .attach_kit_item(USER, attach(KitLevel::Project, "proj-a", &ids[3], KitOp::Add))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, MarketError::Kit(KitErrorCode::InvalidItem, m) if m.contains("24576")),
        "{err:?}"
    );
}

#[tokio::test]
async fn project_import_needs_confirmation_and_never_touches_the_repo_files() {
    let f = fx().await;
    let repo = f.dir.path().join("repo");
    let agents = "# TB\n\nIntro.\n\n## Hard rules\n- never push\n\n## Style\nTabs.\n";
    std::fs::write(repo.join("AGENTS.md"), agents).unwrap();
    std::fs::write(repo.join("CLAUDE.md"), "@AGENTS.md\n").unwrap();

    let query = || RuleImportQuery {
        scope: "project".into(),
        project_id: Some("proj-a".into()),
    };
    let preview = f.rules.import_preview(USER, query()).await.unwrap();
    let statuses: Vec<(&str, &str)> = preview
        .files
        .iter()
        .map(|file| {
            (
                Path::new(&file.path).file_name().unwrap().to_str().unwrap(),
                file.status.as_str(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        vec![("AGENTS.md", "new"), ("CLAUDE.md", "pointer"), ("GEMINI.md", "missing")]
    );
    let names: Vec<&str> = preview.files[0].rules.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["TB: Overview", "TB: Hard rules", "TB: Style"]);
    assert!(
        f.rules.list(USER).await.unwrap().is_empty(),
        "the preview writes nothing"
    );

    let unconfirmed = f
        .rules
        .import(
            USER,
            RuleImportRequest {
                scope: "project".into(),
                project_id: Some("proj-a".into()),
                confirm: false,
            },
        )
        .await;
    assert!(matches!(unconfirmed, Err(RuleError::ConfirmRequired(_))));
    assert!(f.rules.list(USER).await.unwrap().is_empty());

    let done = f
        .rules
        .import(
            USER,
            RuleImportRequest {
                scope: "project".into(),
                project_id: Some("proj-a".into()),
                confirm: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(done.rules.len(), 3);
    assert_eq!(done.rules[0].origin, "import");
    assert!(done.rules[0].source_path.as_deref().unwrap().ends_with("AGENTS.md"));
    assert_eq!(
        read(&repo.join("AGENTS.md")),
        agents,
        "import alone leaves the file as it is"
    );

    // Imported rules are attached at the project: conv-a gets them, conv-b does not.
    assert!(
        f.rules
            .conversation_block(USER, "conv-a")
            .await
            .contains("- never push")
    );
    assert_eq!(f.rules.conversation_block(USER, "conv-b").await, "");

    // Importing again finds nothing new.
    let again = f
        .rules
        .import(
            USER,
            RuleImportRequest {
                scope: "project".into(),
                project_id: Some("proj-a".into()),
                confirm: true,
            },
        )
        .await;
    assert!(matches!(again, Err(RuleError::Invalid(m)) if m.contains("nothing to import")));
    assert_eq!(
        f.rules.import_preview(USER, query()).await.unwrap().files[0].status,
        "imported"
    );

    // The pointer is written only on the owner's confirmation, and only to imported files.
    let manage = |confirm| RuleManageFilesRequest {
        scope: "project".into(),
        project_id: Some("proj-a".into()),
        confirm,
    };
    assert!(matches!(
        f.rules.manage_files(USER, manage(false)).await,
        Err(RuleError::ConfirmRequired(_))
    ));
    assert_eq!(read(&repo.join("AGENTS.md")), agents);
    let written = f.rules.manage_files(USER, manage(true)).await.unwrap();
    assert_eq!(written.written.len(), 1);
    assert_eq!(written.written[0].status, "managed");
    assert_eq!(read(&repo.join("AGENTS.md")), format!("{POINTER_LINE}\n"));
    assert_eq!(
        read(&repo.join("CLAUDE.md")),
        "@AGENTS.md\n",
        "a file that was not imported is never rewritten"
    );
    assert!(!repo.join(".git").exists(), "AionUi does not create or touch git state");

    // A project without imported files cannot be handed over; unknown project → 404.
    let other = RuleManageFilesRequest {
        scope: "project".into(),
        project_id: Some("proj-b".into()),
        confirm: true,
    };
    assert!(matches!(
        f.rules.manage_files(USER, other).await,
        Err(RuleError::Invalid(_))
    ));
    let unknown = RuleImportQuery {
        scope: "project".into(),
        project_id: Some("nope".into()),
    };
    assert!(matches!(
        f.rules.import_preview(USER, unknown).await,
        Err(RuleError::NotFound(_))
    ));
}

#[tokio::test]
async fn global_file_is_generated_only_after_hand_over_and_follows_changes() {
    let f = fx().await;
    let file = f.dir.path().join("global").join("AGENTS.md");
    let original = "# Global\n\n## Hard rules\nNever print secrets.\n\n## Working style\nSmall steps.\n";
    std::fs::write(&file, original).unwrap();

    let import = RuleImportRequest {
        scope: "global".into(),
        project_id: None,
        confirm: true,
    };
    let done = f.rules.import(USER, import).await.unwrap();
    assert_eq!(done.rules.len(), 2);
    assert_eq!(read(&file), original);

    // Before the hand-over a rule change does not write the file.
    let hard = done.rules.iter().find(|r| r.name == "Global: Hard rules").unwrap();
    f.rules
        .update(
            USER,
            &hard.id,
            UpdateRuleRequest {
                body: Some("Never print secrets. Ever.".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(read(&file), original);

    f.rules
        .manage_files(
            USER,
            RuleManageFilesRequest {
                scope: "global".into(),
                project_id: None,
                confirm: true,
            },
        )
        .await
        .unwrap();
    let generated = read(&file);
    assert!(generated.starts_with(GENERATED_MARKER));
    assert!(generated.contains("## Global: Hard rules\n\nNever print secrets. Ever."));
    assert!(generated.contains("## Global: Working style\n\nSmall steps."));

    // From now on the file follows the Global rules: edits, new global attachments, deletes.
    let extra = f
        .rules
        .create(USER, new_rule("Language", "Write English."))
        .await
        .unwrap();
    assert!(!read(&file).contains("Write English."), "not global yet");
    f.market
        .attach_kit_item(USER, attach(KitLevel::Global, "", &extra.id, KitOp::Add))
        .await
        .unwrap();
    assert!(read(&file).contains("## Language\n\nWrite English."));
    f.rules.delete(USER, &hard.id).await.unwrap();
    assert!(!read(&file).contains("Hard rules"));

    // The generated file is never offered for import again.
    let preview = f
        .rules
        .import_preview(
            USER,
            RuleImportQuery {
                scope: "global".into(),
                project_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(preview.files[0].status, "managed");
    assert!(preview.files[0].rules.is_empty());
}
