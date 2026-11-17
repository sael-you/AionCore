//! Migration 910 and the server-pool repository (card C36): CRUD, the
//! write-only key, delete cascades, the audit trail, and resolution of a
//! `server` attachment through the kit levels.

use aionui_db::{
    AttachParams, CreateServerParams, DbError, IKitRepository, IServerPoolRepository, KitScope, SERVER_NAME_TAKEN_MSG,
    ServerAuditParams, SqliteKitRepository, SqliteServerPoolRepository, UpdateServerParams, init_database_memory,
    kit_resolve,
};

const USER: &str = "system_default_user";

async fn setup() -> (aionui_db::Database, SqliteServerPoolRepository) {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteServerPoolRepository::new(db.pool().clone());
    (db, repo)
}

fn params<'a>(name: &'a str, key: Option<&'a str>) -> CreateServerParams<'a> {
    CreateServerParams {
        user_id: USER,
        name,
        host: "10.0.0.5",
        port: 22,
        ssh_user: "deploy",
        host_key: "",
        labels: "[\"web\"]",
        environment: "production",
        tier: "write",
        notes: "",
        enabled: true,
        key_encrypted: key,
    }
}

#[tokio::test]
async fn create_list_update_and_name_conflict() {
    let (_db, repo) = setup().await;
    assert!(!repo.any_enabled_server(USER).await.unwrap());

    let a = repo.create_server(&params("web-1", Some("cipher"))).await.unwrap();
    assert!(a.has_key);
    assert_eq!(a.environment, "production");
    let b = repo.create_server(&params("db-1", None)).await.unwrap();
    assert!(!b.has_key);
    assert!(repo.any_enabled_server(USER).await.unwrap());

    let names: Vec<String> = repo
        .list_servers(USER)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["db-1", "web-1"]);

    let err = repo.create_server(&params("web-1", None)).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(msg) if msg == SERVER_NAME_TAKEN_MSG));

    // A partial update keeps every other column.
    let updated = repo
        .update_server(
            USER,
            &a.id,
            &UpdateServerParams {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!updated.enabled);
    assert_eq!(updated.host, "10.0.0.5");
    assert_eq!(updated.tier, "write");
    assert!(updated.has_key);

    let err = repo
        .update_server(
            USER,
            &b.id,
            &UpdateServerParams {
                name: Some("web-1"),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)));

    // Another user sees nothing and cannot touch it.
    assert!(repo.list_servers("other").await.unwrap().is_empty());
    assert!(repo.find_server("other", &a.id).await.unwrap().is_none());
    assert!(matches!(
        repo.set_server_key("other", &a.id, "x").await,
        Err(DbError::NotFound(_))
    ));
}

#[tokio::test]
async fn key_is_replaced_and_delete_cascades() {
    let (db, repo) = setup().await;
    let kits = SqliteKitRepository::new(db.pool().clone());
    let srv = repo.create_server(&params("web-1", None)).await.unwrap();
    assert_eq!(repo.server_key(USER, &srv.id).await.unwrap(), None);

    repo.set_server_key(USER, &srv.id, "one").await.unwrap();
    repo.set_server_key(USER, &srv.id, "two").await.unwrap();
    assert_eq!(repo.server_key(USER, &srv.id).await.unwrap().as_deref(), Some("two"));

    kits.upsert_attachment(&AttachParams {
        user_id: USER,
        level: "global",
        target_id: "",
        item_kind: "server",
        item_ref: &srv.id,
        op: "add",
    })
    .await
    .unwrap();

    assert!(repo.delete_server(USER, &srv.id).await.unwrap());
    assert!(!repo.delete_server(USER, &srv.id).await.unwrap());
    assert!(repo.find_server(USER, &srv.id).await.unwrap().is_none());
    assert_eq!(repo.server_key(USER, &srv.id).await.unwrap(), None);
    assert!(kits.list_attachments(USER, None, None).await.unwrap().is_empty());
    // The name is free again.
    repo.create_server(&params("web-1", None)).await.unwrap();
}

#[tokio::test]
async fn server_attachment_resolves_through_the_levels() {
    let (db, repo) = setup().await;
    let kits = SqliteKitRepository::new(db.pool().clone());
    let srv = repo.create_server(&params("web-1", None)).await.unwrap();
    let attach = |level: &'static str, target: &'static str, op: &'static str| {
        let kits = kits.clone();
        let id = srv.id.clone();
        async move {
            kits.upsert_attachment(&AttachParams {
                user_id: USER,
                level,
                target_id: target,
                item_kind: "server",
                item_ref: &id,
                op,
            })
            .await
            .unwrap();
        }
    };
    let scope = |project: &str, conversation: &str| KitScope {
        project_id: Some(project.to_owned()),
        assistant_id: None,
        conversation_id: Some(conversation.to_owned()),
    };

    // Not attached anywhere: no decision, not granted.
    let layers = kits.load_layers(USER, &scope("p1", "c1")).await.unwrap();
    assert!(!kit_resolve::resolve(&scope("p1", "c1"), &layers).server_granted(&srv.id));

    attach("project", "p1", "add").await;
    let layers = kits.load_layers(USER, &scope("p1", "c1")).await.unwrap();
    assert!(kit_resolve::resolve(&scope("p1", "c1"), &layers).server_granted(&srv.id));
    // Another project does not get it.
    let layers = kits.load_layers(USER, &scope("p2", "c9")).await.unwrap();
    assert!(!kit_resolve::resolve(&scope("p2", "c9"), &layers).server_granted(&srv.id));

    // A conversation-level remove wins over the project add.
    attach("conversation", "c1", "remove").await;
    let layers = kits.load_layers(USER, &scope("p1", "c1")).await.unwrap();
    let resolution = kit_resolve::resolve(&scope("p1", "c1"), &layers);
    assert!(!resolution.server_granted(&srv.id));
    assert_eq!(resolution.servers[&srv.id].level, "conversation");
}

#[tokio::test]
async fn audit_rows_are_listed_newest_first_and_filtered() {
    let (_db, repo) = setup().await;
    for (server, decision) in [("s1", "ran"), ("s2", "refused"), ("s1", "needs_approval")] {
        repo.insert_server_audit(&ServerAuditParams {
            user_id: USER,
            conversation_id: "c1",
            server_id: server,
            server_name: server,
            channel: "standard",
            decision,
            reason: "",
            command: "uptime",
            exit_code: Some(0),
            duration_ms: Some(12),
        })
        .await
        .unwrap();
    }
    let all = repo.list_server_audit(USER, None, 10).await.unwrap();
    assert_eq!(
        all.iter().map(|r| r.decision.as_str()).collect::<Vec<_>>(),
        vec!["needs_approval", "refused", "ran"]
    );
    let s1 = repo.list_server_audit(USER, Some("s1"), 10).await.unwrap();
    assert_eq!(s1.len(), 2);
    assert_eq!(repo.list_server_audit(USER, None, 1).await.unwrap().len(), 1);
    assert!(repo.list_server_audit("other", None, 10).await.unwrap().is_empty());
}
