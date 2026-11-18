//! Route-level tests for the project page (card C42): usage after recorded
//! turns, storage scan + cache + rescan on a real directory, the overview,
//! owner scoping and error codes. Real in-memory DB + tempdir workspace.

use std::sync::Arc;

use aionui_db::{
    Database, IProjectStore, IUsageRepository, SqliteProjectStore, SqliteUsageRepository, UsageTurnParams,
    init_database_memory,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

use super::{InsightsRouterState, insights_read_routes, insights_write_routes};
use crate::canonical::to_file_uri;
use crate::{MemoryService, ProjectInsights, ProjectService};

const USER: &str = "system_default_user";

struct Fx {
    router: Router,
    other_user: Router,
    project_id: String,
    workspace: TempDir,
    _memory_root: TempDir,
    db: Database,
    usage: SqliteUsageRepository,
}

fn user(id: &str) -> aionui_auth::CurrentUser {
    aionui_auth::CurrentUser {
        id: id.to_owned(),
        username: "admin".to_owned(),
        user_type: aionui_db::UserType::Local,
        status: aionui_db::UserStatus::Active,
    }
}

async fn fx() -> Fx {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));
    let workspace = tempfile::tempdir().unwrap();
    let created = service
        .create_standard(USER, to_file_uri(workspace.path()).unwrap())
        .await
        .unwrap();
    let project_id = created.project.project_id;
    let memory_root = tempfile::tempdir().unwrap();
    let memory = Arc::new(MemoryService::new(memory_root.path()));
    // Two memory files for the overview tile.
    let memory_dir = memory.dir_for(&std::fs::canonicalize(workspace.path()).unwrap());
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(memory_dir.join("MEMORY.md"), "index").unwrap();
    std::fs::write(memory_dir.join("a.md"), "one").unwrap();
    std::fs::write(memory_dir.join("notes.txt"), "not a memory").unwrap();

    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra) \
         VALUES ('conv-a', 'system_default_user', 'Alpha', 'acp', 'pending', 1, 1, ?, '{}')",
    )
    .bind(&project_id)
    .execute(db.pool())
    .await
    .unwrap();

    let usage = SqliteUsageRepository::new(db.pool().clone());
    let insights = Arc::new(ProjectInsights::new(service, Arc::new(usage.clone()), Some(memory)));
    let state = InsightsRouterState { insights };
    let routes = insights_read_routes(state.clone()).merge(insights_write_routes(state));
    Fx {
        router: routes.clone().layer(axum::Extension(user(USER))),
        other_user: routes.layer(axum::Extension(user("someone-else"))),
        project_id,
        workspace,
        _memory_root: memory_root,
        db,
        usage,
    }
}

async fn send(router: &Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri).body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn usage_shows_todays_turn_and_the_conversation() {
    let f = fx().await;
    let uri = format!("/api/projects/{}/usage?days=7", f.project_id);
    let (status, empty) = send(&f.router, "GET", &uri).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["data"]["days"].as_array().unwrap().len(), 7, "zero-filled range");
    assert!(empty["data"].get("since").is_none(), "nothing recorded yet");
    assert_eq!(empty["data"]["by_conversation"], serde_json::json!([]));

    f.usage
        .record_usage_turn(&UsageTurnParams {
            user_id: USER,
            conversation_id: "conv-a",
            input_tokens: 12,
            output_tokens: 34,
            cache_read_tokens: 900,
            cache_write_tokens: 30,
            thought_tokens: 7,
            cost_micro_usd: Some(2500),
            at: aionui_common::now_ms(),
        })
        .await
        .unwrap();

    let (_, body) = send(&f.router, "GET", &uri).await;
    let data = &body["data"];
    let today = &data["days"][6];
    assert_eq!(
        today["day"],
        chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string()
    );
    assert_eq!(
        (
            today["input"].as_i64(),
            today["output"].as_i64(),
            today["cache_read"].as_i64(),
            today["cache_write"].as_i64()
        ),
        (Some(12), Some(34), Some(900), Some(30))
    );
    assert_eq!(
        (
            today["thought"].as_i64(),
            today["cost_micro_usd"].as_i64(),
            today["turns"].as_i64()
        ),
        (Some(7), Some(2500), Some(1))
    );
    assert!(
        data["days"][5].get("cost_micro_usd").is_none(),
        "no cost on an empty day"
    );
    assert_eq!(data["totals"]["input"], 12);
    assert_eq!(data["range_days"], 7);
    assert!(data["since"].as_i64().is_some());
    assert_eq!(data["by_conversation"][0]["conversation_id"], "conv-a");
    assert_eq!(data["by_conversation"][0]["name"], "Alpha");
    assert_eq!(data["by_conversation"][0]["output"], 34);
    assert_eq!(data["by_model"][0]["turns"], 1);

    // The all-projects summary carries the same day and the project.
    let (status, summary) = send(&f.router, "GET", "/api/usage/summary").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summary["data"]["range_days"], 30);
    assert_eq!(summary["data"]["by_project"][0]["project_id"], f.project_id.as_str());
    assert_eq!(summary["data"]["by_project"][0]["input"], 12);

    // Another user sees neither the project nor the usage.
    let (status, resp) = send(&f.other_user, "GET", &uri).await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );
    let (_, theirs) = send(&f.other_user, "GET", "/api/usage/summary?days=7").await;
    assert_eq!(theirs["data"]["totals"]["turns"], 0);
    assert_eq!(theirs["data"]["by_project"], serde_json::json!([]));

    let (status, resp) = send(
        &f.router,
        "GET",
        &format!("/api/projects/{}/usage?days=14", f.project_id),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("usage_invalid_days"))
    );
}

#[tokio::test]
async fn storage_scans_caches_and_rescans() {
    let f = fx().await;
    std::fs::create_dir_all(f.workspace.path().join("src")).unwrap();
    std::fs::write(f.workspace.path().join("src/a.rs"), vec![b'x'; 4000]).unwrap();
    std::fs::write(f.workspace.path().join("README.md"), vec![b'x'; 100]).unwrap();

    // The overview never scans: no storage before the first scan.
    let overview_uri = format!("/api/projects/{}/overview", f.project_id);
    let (_, before) = send(&f.router, "GET", &overview_uri).await;
    assert!(before["data"].get("storage").is_none());

    let uri = format!("/api/projects/{}/storage", f.project_id);
    let (status, first) = send(&f.router, "GET", &uri).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let roots = first["data"]["roots"].as_array().unwrap();
    assert_eq!(
        roots.iter().map(|r| r["label"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["workspace", "memory"]
    );
    assert_eq!(roots[0]["bytes"], 4100);
    assert_eq!(roots[0]["truncated"], false);
    assert_eq!(
        roots[0]["children"][0],
        serde_json::json!({"name": "src", "bytes": 4000, "kind": "dir"})
    );
    assert_eq!(
        roots[0]["largest_files"][0],
        serde_json::json!({"path": "src/a.rs", "bytes": 4000})
    );
    assert_eq!(roots[1]["exists"], true);
    assert_eq!(
        first["data"]["total_bytes"].as_u64().unwrap(),
        4100 + roots[1]["bytes"].as_u64().unwrap()
    );
    assert!(first["data"]["disk"]["total_bytes"].as_u64().unwrap() > 0);

    // A new file is not seen until a rescan: the GET is served from the cache.
    std::fs::write(f.workspace.path().join("big.bin"), vec![b'x'; 10_000]).unwrap();
    let (_, cached) = send(&f.router, "GET", &uri).await;
    assert_eq!(cached["data"]["scanned_at"], first["data"]["scanned_at"]);
    assert_eq!(cached["data"]["roots"][0]["bytes"], 4100);
    let (status, fresh) = send(&f.router, "POST", &format!("{uri}/rescan")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fresh["data"]["roots"][0]["bytes"], 14_100);
    let (_, after) = send(&f.router, "GET", &uri).await;
    assert_eq!(
        after["data"]["roots"][0]["bytes"], 14_100,
        "the rescan replaced the cache"
    );

    // The overview now carries the cached scan.
    let (_, overview) = send(&f.router, "GET", &overview_uri).await;
    assert_eq!(overview["data"]["storage"]["total_bytes"], fresh["data"]["total_bytes"]);

    // Not the user's project: 404 on read and on rescan, nothing scanned.
    let (status, resp) = send(&f.other_user, "GET", &uri).await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );
    let (status, _) = send(&f.other_user, "POST", &format!("{uri}/rescan")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn overview_counts_the_tiles() {
    let f = fx().await;
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, project_id, extra, archived_at) \
         VALUES ('conv-old', 'system_default_user', 'Old', 'acp', 'pending', 1, 1, ?, '{}', 5)",
    )
    .bind(&f.project_id)
    .execute(f.db.pool())
    .await
    .unwrap();
    f.usage
        .record_usage_turn(&UsageTurnParams {
            user_id: USER,
            conversation_id: "conv-a",
            input_tokens: 5,
            output_tokens: 6,
            at: aionui_common::now_ms(),
            ..Default::default()
        })
        .await
        .unwrap();

    let (status, body) = send(&f.router, "GET", &format!("/api/projects/{}/overview", f.project_id)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data = &body["data"];
    assert_eq!(data["project_id"], f.project_id.as_str());
    assert!(!data["name"].as_str().unwrap().is_empty());
    assert_eq!(data["conversations"], serde_json::json!({"active": 1, "total": 2}));
    assert_eq!(data["open_cards"], 0);
    assert_eq!(
        data["kit"],
        serde_json::json!({"skills": 0, "mcps": 0, "rules": 0, "servers": 0})
    );
    assert_eq!(data["memory_files"], 2, "only *.md files");
    assert_eq!(
        (
            data["tokens_today"]["input"].as_i64(),
            data["tokens_today"]["output"].as_i64()
        ),
        (Some(5), Some(6))
    );

    let (status, resp) = send(
        &f.other_user,
        "GET",
        &format!("/api/projects/{}/overview", f.project_id),
    )
    .await;
    assert_eq!(
        (status, resp["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );
}
