//! TASK-71 (train-2 review C2 #4): an instance update must not write back the
//! status (or any column) it loaded when the row changed in between — e.g. a
//! revoke landing while the owner saves an edit. The repository wrapper below
//! performs that revoke right before the service's write.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aionui_api_types::{CreateMcpMarketInstanceRequest, UpdateMcpMarketInstanceRequest};
use aionui_db::models::{McpAssignmentRow, McpMarketRow, McpSecretRow};
use aionui_db::{
    CreateAssignmentParams, DbError, IMcpMarketRepository, IMcpServerRepository, MarketInstanceParams,
    SqliteMcpMarketRepository, SqliteMcpServerRepository, UpdateMarketInstanceParams, init_database_memory,
};
use aionui_mcp::market::MarketError;
use aionui_mcp::{McpMarketService, StaticTemplateCatalog};
use serde_json::json;

const USER: &str = "system_default_user";

/// Delegates everything; the first `update_instance` call that carries an
/// `expected_updated_at` (a service read-decide-write) is preceded by a
/// concurrent revoke.
struct RevokeBeforeWrite {
    inner: SqliteMcpMarketRepository,
    armed: AtomicBool,
}

#[async_trait::async_trait]
impl IMcpMarketRepository for RevokeBeforeWrite {
    async fn list_instances(&self, user_id: &str) -> Result<Vec<McpMarketRow>, DbError> {
        self.inner.list_instances(user_id).await
    }
    async fn find_instance(&self, user_id: &str, id: &str) -> Result<Option<McpMarketRow>, DbError> {
        self.inner.find_instance(user_id, id).await
    }
    async fn find_by_name_any(&self, user_id: &str, name: &str) -> Result<Option<McpMarketRow>, DbError> {
        self.inner.find_by_name_any(user_id, name).await
    }
    async fn create_instance(&self, params: &MarketInstanceParams<'_>) -> Result<McpMarketRow, DbError> {
        self.inner.create_instance(params).await
    }
    async fn revive_instance(
        &self,
        user_id: &str,
        id: &str,
        params: &MarketInstanceParams<'_>,
    ) -> Result<McpMarketRow, DbError> {
        self.inner.revive_instance(user_id, id, params).await
    }
    async fn update_instance(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateMarketInstanceParams<'_>,
    ) -> Result<McpMarketRow, DbError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.inner
                .update_instance(
                    user_id,
                    id,
                    &UpdateMarketInstanceParams {
                        status: Some("revoked"),
                        ..Default::default()
                    },
                )
                .await?;
        }
        self.inner.update_instance(user_id, id, params).await
    }
    async fn delete_instance(&self, user_id: &str, id: &str) -> Result<(), DbError> {
        self.inner.delete_instance(user_id, id).await
    }
    async fn list_secrets(&self, user_id: &str) -> Result<Vec<McpSecretRow>, DbError> {
        self.inner.list_secrets(user_id).await
    }
    async fn list_instance_secrets(&self, user_id: &str, instance_id: &str) -> Result<Vec<McpSecretRow>, DbError> {
        self.inner.list_instance_secrets(user_id, instance_id).await
    }
    async fn upsert_secret(
        &self,
        user_id: &str,
        instance_id: &str,
        name: &str,
        value_encrypted: &str,
    ) -> Result<McpSecretRow, DbError> {
        self.inner
            .upsert_secret(user_id, instance_id, name, value_encrypted)
            .await
    }
    async fn delete_secret(&self, user_id: &str, instance_id: &str, name: &str) -> Result<bool, DbError> {
        self.inner.delete_secret(user_id, instance_id, name).await
    }
    async fn list_assignments(
        &self,
        user_id: &str,
        instance_id: Option<&str>,
    ) -> Result<Vec<McpAssignmentRow>, DbError> {
        self.inner.list_assignments(user_id, instance_id).await
    }
    async fn find_assignment(&self, user_id: &str, id: &str) -> Result<Option<McpAssignmentRow>, DbError> {
        self.inner.find_assignment(user_id, id).await
    }
    async fn create_assignment(&self, params: &CreateAssignmentParams<'_>) -> Result<McpAssignmentRow, DbError> {
        self.inner.create_assignment(params).await
    }
    async fn create_assignments(
        &self,
        params: &[CreateAssignmentParams<'_>],
    ) -> Result<Vec<McpAssignmentRow>, DbError> {
        self.inner.create_assignments(params).await
    }
    async fn delete_assignment(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        self.inner.delete_assignment(user_id, id).await
    }
    async fn delete_instance_assignments(
        &self,
        user_id: &str,
        instance_id: &str,
    ) -> Result<Vec<McpAssignmentRow>, DbError> {
        self.inner.delete_instance_assignments(user_id, instance_id).await
    }
    async fn has_assignment(
        &self,
        user_id: &str,
        instance_id: &str,
        project_id: Option<&str>,
        assistant_id: Option<&str>,
        cli: &str,
    ) -> Result<bool, DbError> {
        self.inner
            .has_assignment(user_id, instance_id, project_id, assistant_id, cli)
            .await
    }
    async fn conversation_project_id(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, DbError> {
        self.inner.conversation_project_id(user_id, conversation_id).await
    }
    async fn project_exists(&self, project_id: &str) -> Result<bool, DbError> {
        self.inner.project_exists(project_id).await
    }
}

#[tokio::test]
async fn an_edit_never_resurrects_a_concurrently_revoked_instance() {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(RevokeBeforeWrite {
        inner: SqliteMcpMarketRepository::new(db.pool().clone()),
        armed: AtomicBool::new(false),
    });
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let catalog = StaticTemplateCatalog::new(vec![json!({"id": "duck", "name": "Duck", "market": {
        "transport": "stdio", "command": {"command": "npx", "args": ["-y", "duck-mcp@1.0.0"]},
        "tiers": ["read"], "enforcement": "tools-only", "clis": ["claude"]
    }})]);
    let market = McpMarketService::new(repo.clone(), servers, Arc::new(catalog), [9u8; 32]);
    let created = market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                name: "duck-edit".into(),
                template_id: "duck".into(),
                description: None,
                environment: "sandbox".into(),
                tier: "read".into(),
                status: Some("active".into()),
                clis: vec!["claude".into()],
                plain_env: BTreeMap::new(),
                tools_allow: Some(vec!["search".into()]),
                tools_block: vec![],
            },
        )
        .await
        .unwrap();
    assert_eq!(created.status, "active");

    // The owner saves a description while a revoke lands in between.
    repo.armed.store(true, Ordering::SeqCst);
    let result = market
        .update_instance(
            USER,
            &created.id,
            UpdateMcpMarketInstanceRequest {
                description: Some("edited".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(MarketError::Conflict(_))), "{result:?}");
    let row = repo.inner.find_instance(USER, &created.id).await.unwrap().unwrap();
    assert_eq!(
        row.market_status.as_deref(),
        Some("revoked"),
        "the revoke was overwritten"
    );

    // Reloaded, the same edit goes through and leaves the status alone.
    let edited = market
        .update_instance(
            USER,
            &created.id,
            UpdateMcpMarketInstanceRequest {
                description: Some("edited".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.status, "revoked");
    assert_eq!(edited.description.as_deref(), Some("edited"));
}
