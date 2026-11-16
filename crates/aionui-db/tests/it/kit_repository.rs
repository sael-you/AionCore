//! Migration 907 and the kit repository (card C27): kit CRUD with member
//! sets, the idempotent builtin seed, attachment upsert/flip, delete
//! cascades, and scope loading + resolution for a conversation.

use aionui_db::models::KitRow;
use aionui_db::{
    AttachParams, CreateKitParams, DbError, IKitRepository, KitMembers, KitScope, SeedKit, SqliteKitRepository,
    UpdateKitParams, init_database_memory, kit_resolve,
};

const USER: &str = "system_default_user";

async fn setup() -> (aionui_db::Database, SqliteKitRepository) {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteKitRepository::new(db.pool().clone());
    (db, repo)
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

async fn create(repo: &SqliteKitRepository, name: &str, skills: &[&str], mcps: &[&str]) -> aionui_db::models::KitRow {
    let skills = s(skills);
    let mcps = s(mcps);
    repo.create_kit(&CreateKitParams {
        user_id: USER,
        name,
        description: "",
        project_id: None,
        members: KitMembers {
            skills: &skills,
            mcp_instance_ids: &mcps,
            mcp_templates: &[],
        },
    })
    .await
    .unwrap()
}

fn attach<'a>(level: &'a str, target: &'a str, kind: &'a str, item: &'a str, op: &'a str) -> AttachParams<'a> {
    AttachParams {
        user_id: USER,
        level,
        target_id: target,
        item_kind: kind,
        item_ref: item,
        op,
    }
}

#[tokio::test]
async fn migration_907_creates_kit_tables() {
    let (db, _) = setup().await;
    for table in ["kits", "kit_items", "kit_attachments"] {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 1, "{table} missing");
    }
}

#[tokio::test]
async fn kit_crud_replaces_member_sets_and_rejects_duplicates_and_stale_writes() {
    let (_db, repo) = setup().await;
    let kit = create(&repo, "Platform", &["pf-core", "cs-build"], &["mcp-1"]).await;
    let items = repo
        .list_kit_items(USER, Some(std::slice::from_ref(&kit.id)))
        .await
        .unwrap();
    assert_eq!(items.len(), 3);

    let dup = repo
        .create_kit(&CreateKitParams {
            user_id: USER,
            name: "Platform",
            description: "",
            project_id: None,
            members: KitMembers::default(),
        })
        .await;
    assert!(matches!(dup, Err(DbError::Conflict(_))), "{dup:?}");

    let skills = s(&["pf-ui"]);
    let updated = repo
        .update_kit(
            USER,
            &kit.id,
            &UpdateKitParams {
                description: Some("ui"),
                skills: Some(&skills),
                expected_updated_at: Some(kit.updated_at),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.description, "ui");
    assert!(updated.updated_at > kit.updated_at);
    let items = repo
        .list_kit_items(USER, Some(std::slice::from_ref(&kit.id)))
        .await
        .unwrap();
    let refs: Vec<(&str, &str)> = items.iter().map(|i| (i.kind.as_str(), i.item_ref.as_str())).collect();
    // skills replaced, the mcp member untouched (its list was not sent)
    assert_eq!(refs, vec![("mcp", "mcp-1"), ("skill", "pf-ui")]);

    let stale = repo
        .update_kit(
            USER,
            &kit.id,
            &UpdateKitParams {
                name: Some("X"),
                expected_updated_at: Some(kit.updated_at),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(stale, Err(DbError::Conflict(_))), "{stale:?}");
    let missing = repo.update_kit(USER, "nope", &UpdateKitParams::default()).await;
    assert!(matches!(missing, Err(DbError::NotFound(_))), "{missing:?}");

    // other users see nothing
    assert!(repo.list_kits("someone-else").await.unwrap().is_empty());
    assert!(repo.find_kit("someone-else", &kit.id).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_kit_removes_its_attachments_and_frees_the_name() {
    let (_db, repo) = setup().await;
    let kit = create(&repo, "Ads", &["paid-ads"], &[]).await;
    repo.upsert_attachment(&attach("project", "p1", "kit", &kit.id, "add"))
        .await
        .unwrap();
    repo.upsert_attachment(&attach("project", "p1", "skill", "pdf", "add"))
        .await
        .unwrap();
    assert!(repo.delete_kit(USER, &kit.id).await.unwrap());
    assert!(!repo.delete_kit(USER, &kit.id).await.unwrap());
    let left = repo.list_attachments(USER, None, None).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].item_ref, "pdf");
    // the name is free again
    create(&repo, "Ads", &[], &[]).await;
}

#[tokio::test]
async fn seed_is_idempotent_and_never_resurrects_a_deleted_builtin() {
    let (_db, repo) = setup().await;
    let seeds = [
        SeedKit {
            builtin_key: "K1",
            name: "Platform Dev",
            description: "build",
            skills: &["pf-core"],
            mcp_templates: &["context7", "github"],
            attach_globally: false,
        },
        SeedKit {
            builtin_key: "K2",
            name: "Ship & Ops",
            description: "ship",
            skills: &["pf-staging"],
            mcp_templates: &["sentry"],
            attach_globally: false,
        },
    ];
    assert_eq!(repo.seed_kits(USER, &seeds).await.unwrap(), 2);
    assert_eq!(repo.seed_kits(USER, &seeds).await.unwrap(), 0);
    let kits = repo.list_kits(USER).await.unwrap();
    assert_eq!(
        kits.iter().map(|k| k.builtin_key.as_deref()).collect::<Vec<_>>(),
        vec![Some("K1"), Some("K2")]
    );
    let items = repo.list_kit_items(USER, Some(&[kits[0].id.clone()])).await.unwrap();
    assert_eq!(items.iter().filter(|i| i.kind == "template").count(), 2);

    assert!(repo.delete_kit(USER, &kits[0].id).await.unwrap());
    assert_eq!(repo.seed_kits(USER, &seeds).await.unwrap(), 0);
    assert_eq!(repo.list_kits(USER).await.unwrap().len(), 1);
}

#[tokio::test]
async fn attaching_again_flips_the_op_instead_of_duplicating() {
    let (_db, repo) = setup().await;
    let a = repo
        .upsert_attachment(&attach("assistant", "a1", "skill", "pdf", "add"))
        .await
        .unwrap();
    let b = repo
        .upsert_attachment(&attach("assistant", "a1", "skill", "pdf", "remove"))
        .await
        .unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(b.op, "remove");
    assert_eq!(
        repo.list_attachments(USER, Some("assistant"), Some("a1"))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        repo.list_attachments(USER, Some("project"), None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(repo.delete_attachment(USER, &a.id).await.unwrap());
    assert!(!repo.delete_attachment(USER, &a.id).await.unwrap());
}

#[tokio::test]
async fn conversation_scope_and_layers_resolve_all_four_levels() {
    let (db, repo) = setup().await;
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) \
         VALUES ('conv-a', ?, 'c', 'acp', 'pending', 1, 1, 'proj-a', '{\"preset_assistant_id\":\"asst-1\"}')",
    )
    .bind(USER)
    .execute(db.pool())
    .await
    .unwrap();
    let scope = repo.conversation_scope(USER, "conv-a").await.unwrap().unwrap();
    assert_eq!(
        scope,
        KitScope {
            project_id: Some("proj-a".into()),
            assistant_id: Some("asst-1".into()),
            conversation_id: Some("conv-a".into()),
        }
    );
    assert!(repo.conversation_scope(USER, "missing").await.unwrap().is_none());

    let k6 = create(&repo, "SEO", &["seo-audit", "ai-seo"], &["gsc-1"]).await;
    repo.upsert_attachment(&attach("global", "", "skill", "context-skill", "add"))
        .await
        .unwrap();
    repo.upsert_attachment(&attach("project", "proj-a", "kit", &k6.id, "add"))
        .await
        .unwrap();
    repo.upsert_attachment(&attach("project", "proj-b", "skill", "other-project", "add"))
        .await
        .unwrap();
    repo.upsert_attachment(&attach("assistant", "asst-1", "skill", "ai-seo", "remove"))
        .await
        .unwrap();
    repo.upsert_attachment(&attach("conversation", "conv-a", "mcp", "gsc-1", "remove"))
        .await
        .unwrap();

    let layers = repo.load_layers(USER, &scope).await.unwrap();
    assert_eq!(layers.attachments.len(), 4, "proj-b's attachment is not in scope");
    let r = kit_resolve::resolve(&scope, &layers);
    assert!(r.skills["context-skill"].included);
    assert!(r.skills["seo-audit"].included);
    assert!(!r.skills["ai-seo"].included);
    assert!(r.mcp_removed("gsc-1"));
    assert_eq!(
        r.apply_skills(&s(&["cron"])),
        vec!["cron", "context-skill", "seo-audit"]
    );
}

// -- C28 project kits ---------------------------------------------------------

async fn project_kit(
    repo: &SqliteKitRepository,
    name: &str,
    project: &str,
    skills: &[&str],
) -> Result<KitRow, DbError> {
    let skills = s(skills);
    repo.create_kit(&CreateKitParams {
        user_id: USER,
        name,
        description: "",
        project_id: Some(project),
        members: KitMembers {
            skills: &skills,
            mcp_instance_ids: &[],
            mcp_templates: &[],
        },
    })
    .await
}

#[tokio::test]
async fn one_live_project_kit_per_project_and_it_is_implicitly_attached() {
    let (db, repo) = setup().await;
    let pk = project_kit(&repo, "TB-test", "proj-a", &["tb-protocol"]).await.unwrap();
    assert_eq!(pk.project_id.as_deref(), Some("proj-a"));
    let second = project_kit(&repo, "TB-test 2", "proj-a", &[]).await;
    assert!(
        matches!(&second, Err(DbError::Conflict(m)) if m == aionui_db::KIT_PROJECT_HAS_KIT_MSG),
        "{second:?}"
    );
    // A name clash is still the name conflict, not the project one.
    let same_name = project_kit(&repo, "TB-test", "proj-b", &[]).await;
    assert!(
        matches!(&same_name, Err(DbError::Conflict(m)) if m == aionui_db::KIT_NAME_TAKEN_MSG),
        "{same_name:?}"
    );
    assert_eq!(repo.find_project_kit(USER, "proj-a").await.unwrap().unwrap().id, pk.id);
    assert!(repo.find_project_kit(USER, "proj-b").await.unwrap().is_none());

    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) \
         VALUES ('conv-a', ?, 'c', 'acp', 'pending', 1, 1, 'proj-a', '{\"workspace\":\"/ws/a\"}'), \
                ('conv-b', ?, 'c', 'acp', 'pending', 1, 1, 'proj-b', '{}')",
    )
    .bind(USER)
    .bind(USER)
    .execute(db.pool())
    .await
    .unwrap();
    assert_eq!(
        repo.conversation_workspace(USER, "conv-a").await.unwrap().as_deref(),
        Some("/ws/a")
    );
    assert!(repo.conversation_workspace(USER, "conv-b").await.unwrap().is_none());

    let scope_a = repo.conversation_scope(USER, "conv-a").await.unwrap().unwrap();
    let layers = repo.load_layers(USER, &scope_a).await.unwrap();
    assert_eq!(layers.attachments.len(), 1, "implicit project attachment, no row");
    assert!(repo.list_attachments(USER, None, None).await.unwrap().is_empty());
    assert!(kit_resolve::resolve(&scope_a, &layers).skills["tb-protocol"].included);

    let scope_b = repo.conversation_scope(USER, "conv-b").await.unwrap().unwrap();
    let layers = repo.load_layers(USER, &scope_b).await.unwrap();
    assert!(kit_resolve::resolve(&scope_b, &layers).skills.is_empty());

    // Deleted: the project may get a new project kit.
    assert!(repo.delete_kit(USER, &pk.id).await.unwrap());
    project_kit(&repo, "TB-test", "proj-a", &[]).await.unwrap();
}

#[tokio::test]
async fn seed_attaches_k0_globally_once_and_a_detach_is_not_undone() {
    let (_db, repo) = setup().await;
    let seeds = [SeedKit {
        builtin_key: "K0",
        name: "Global",
        description: "",
        skills: &["cs-verify"],
        mcp_templates: &[],
        attach_globally: true,
    }];
    assert_eq!(repo.seed_kits(USER, &seeds).await.unwrap(), 1);
    let global = repo.list_attachments(USER, Some("global"), Some("")).await.unwrap();
    assert_eq!(global.len(), 1);
    assert!(repo.delete_attachment(USER, &global[0].id).await.unwrap());
    assert_eq!(repo.seed_kits(USER, &seeds).await.unwrap(), 0);
    assert!(
        repo.list_attachments(USER, Some("global"), None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn global_kit_step_moves_builtins_off_global_exactly_once() {
    let (db, repo) = setup().await;
    for (pid, name) in [
        ("p-aionui", "aionui"),
        ("p-studio", "clauterm-studio"),
        ("p-tb", "TB-test"),
    ] {
        sqlx::query(
            "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, 'standard', 1, 1)",
        )
        .bind(pid)
        .bind(USER)
        .bind(name)
        .execute(db.pool())
        .await
        .unwrap();
    }
    let seeds = [
        SeedKit {
            builtin_key: "K0",
            name: "Global",
            description: "",
            skills: &[],
            mcp_templates: &[],
            attach_globally: true,
        },
        SeedKit {
            builtin_key: "K1",
            name: "Platform Dev",
            description: "",
            skills: &[],
            mcp_templates: &[],
            attach_globally: false,
        },
        SeedKit {
            builtin_key: "K2",
            name: "Ship & Ops",
            description: "",
            skills: &[],
            mcp_templates: &[],
            attach_globally: false,
        },
    ];
    repo.seed_kits(USER, &seeds).await.unwrap();
    let kits = repo.list_kits(USER).await.unwrap();
    let id = |key: &str| {
        kits.iter()
            .find(|k| k.builtin_key.as_deref() == Some(key))
            .unwrap()
            .id
            .clone()
    };
    let (k0, k1, k2) = (id("K0"), id("K1"), id("K2"));
    // Today's state: Platform Dev attached globally (plus an owner single skill).
    repo.upsert_attachment(&attach("global", "", "kit", &k1, "add"))
        .await
        .unwrap();
    repo.upsert_attachment(&attach("global", "", "skill", "pdf", "add"))
        .await
        .unwrap();

    let off: Vec<String> = (1..=12).map(|n| format!("K{n}")).collect();
    let off: Vec<&str> = off.iter().map(String::as_str).collect();
    let step = repo
        .run_global_kit_step_once(
            USER,
            "kits.c28",
            &off,
            &["K1", "K2"],
            &["aionui", "clauterm-studio", "missing"],
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(step.removed_global, 1);
    assert_eq!(step.attached_project, 4);
    let global: Vec<String> = repo
        .list_attachments(USER, Some("global"), None)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.item_ref)
        .collect();
    assert_eq!(global.len(), 2);
    assert!(global.contains(&k0) && global.contains(&"pdf".to_owned()));
    for project in ["p-aionui", "p-studio"] {
        let refs: Vec<String> = repo
            .list_attachments(USER, Some("project"), Some(project))
            .await
            .unwrap()
            .into_iter()
            .map(|a| a.item_ref)
            .collect();
        assert!(refs.contains(&k1) && refs.contains(&k2), "{project}: {refs:?}");
    }
    assert!(
        repo.list_attachments(USER, Some("project"), Some("p-tb"))
            .await
            .unwrap()
            .is_empty()
    );

    // Re-run is a no-op, even after the owner re-attaches K1 globally.
    repo.upsert_attachment(&attach("global", "", "kit", &k1, "add"))
        .await
        .unwrap();
    assert!(
        repo.run_global_kit_step_once(USER, "kits.c28", &off, &["K1", "K2"], &["aionui"])
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.list_attachments(USER, Some("global"), None).await.unwrap().len(),
        3
    );
}
