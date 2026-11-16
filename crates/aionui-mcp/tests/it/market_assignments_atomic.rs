//! TASK-73 (train-2 review C2 #7): creating assignments is all or nothing —
//! a CLI listed twice is refused before anything is written, a duplicate of an
//! existing row inserts none of the batch, and the assistant is bound only
//! after the rows are committed (rows undone if the binding fails).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aionui_api_types::{CreateMcpMarketAssignmentRequest, CreateMcpMarketInstanceRequest};
use aionui_db::{
    IMcpMarketRepository, IMcpServerRepository, SqliteMcpMarketRepository, SqliteMcpServerRepository,
    init_database_memory,
};
use aionui_mcp::market::MarketError;
use aionui_mcp::{IMcpAssistantBinder, McpMarketService, StaticTemplateCatalog};
use serde_json::json;

const USER: &str = "system_default_user";

#[derive(Default)]
struct RecordingBinder {
    binds: Mutex<Vec<String>>,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl IMcpAssistantBinder for RecordingBinder {
    async fn bind(&self, _user_id: &str, assistant_id: &str, _mcp_server_id: &str) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("assistant service unavailable".into());
        }
        self.binds.lock().unwrap().push(assistant_id.to_owned());
        Ok(())
    }
    async fn unbind(&self, _user_id: &str, _assistant_id: &str, _mcp_server_id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn assign(instance_id: &str, assistant: &str, clis: &[&str]) -> CreateMcpMarketAssignmentRequest {
    CreateMcpMarketAssignmentRequest {
        instance_id: instance_id.to_owned(),
        project_id: Some("proj-a".into()),
        assistant_id: Some(assistant.to_owned()),
        clis: clis.iter().map(|c| (*c).to_owned()).collect(),
    }
}

#[tokio::test]
async fn assignment_batches_are_atomic_and_bind_after_commit() {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) \
         VALUES ('proj-a', 'system_default_user', 'A', 'standard', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let catalog = StaticTemplateCatalog::new(vec![json!({"id": "duck", "name": "Duck", "market": {
        "transport": "stdio", "command": {"command": "npx", "args": ["-y", "duck-mcp@1.0.0"]},
        "tiers": ["read"], "enforcement": "tools-only", "clis": ["claude", "codex", "gemini"]
    }})]);
    let binder = Arc::new(RecordingBinder::default());
    let market = McpMarketService::new(repo.clone(), servers, Arc::new(catalog), [4u8; 32])
        .with_assistant_binder(binder.clone());
    let inst = market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                name: "duck-assign".into(),
                template_id: "duck".into(),
                description: None,
                environment: "sandbox".into(),
                tier: "read".into(),
                status: None,
                clis: vec!["claude".into(), "codex".into()],
                plain_env: BTreeMap::new(),
                tools_allow: Some(vec!["search".into()]),
                tools_block: vec![],
            },
        )
        .await
        .unwrap();
    let rows = || async { repo.list_assignments(USER, Some(&inst.id)).await.unwrap().len() };

    // A CLI listed twice: refused before any write or binding.
    let err = market
        .create_assignments(USER, assign(&inst.id, "asst-1", &["claude", "claude"]))
        .await
        .unwrap_err();
    assert!(matches!(err, MarketError::Invalid(_)), "{err:?}");
    assert_eq!(rows().await, 0);
    assert!(binder.binds.lock().unwrap().is_empty());

    // One existing row; a batch that repeats it inserts nothing else.
    market
        .create_assignments(USER, assign(&inst.id, "asst-1", &["claude"]))
        .await
        .unwrap();
    assert_eq!(rows().await, 1);
    assert_eq!(binder.binds.lock().unwrap().len(), 1);
    let err = market
        .create_assignments(USER, assign(&inst.id, "asst-1", &["codex", "claude"]))
        .await
        .unwrap_err();
    assert!(matches!(err, MarketError::Conflict(_)), "{err:?}");
    assert_eq!(rows().await, 1, "the codex row of the failed batch was kept");
    assert_eq!(binder.binds.lock().unwrap().len(), 1, "bound before the insert failed");

    // The binding fails after the commit: the new rows are undone.
    binder.fail.store(true, Ordering::SeqCst);
    let err = market
        .create_assignments(USER, assign(&inst.id, "asst-2", &["codex"]))
        .await
        .unwrap_err();
    assert!(matches!(err, MarketError::Invalid(_)), "{err:?}");
    assert_eq!(rows().await, 1, "rows of a batch whose binding failed were kept");
}
