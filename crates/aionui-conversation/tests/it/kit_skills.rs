//! Kits (card C27) on a new conversation's skill snapshot: the kit layers are
//! applied on top of the unchanged pre-kit snapshot, the base is kept in
//! `extra.kit_base_skills`, and a conversation-level re-apply starts from it.

use std::sync::Arc;

use aionui_ai_agent::{AgentError, IWorkerTaskManager};
use aionui_api_types::{CreateConversationRequest, WebSocketMessage};
use aionui_common::{AgentKillReason, TimestampMs};
use aionui_conversation::ConversationService;
use aionui_conversation::skill_resolver::SkillResolver;
use aionui_db::{
    AttachParams, IConversationRepository, IKitRepository, SqliteConversationRepository, SqliteKitRepository,
    init_database_memory,
};
use aionui_realtime::EventBroadcaster;
use serde_json::json;

const USER_ID: &str = "system_default_user";

struct NullBroadcaster;
impl EventBroadcaster for NullBroadcaster {
    fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
}

struct NoopTaskManager;

#[async_trait::async_trait]
impl IWorkerTaskManager for NoopTaskManager {
    fn get_task(&self, _: &str) -> Option<aionui_ai_agent::AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: aionui_ai_agent::types::BuildTaskOptions,
    ) -> Result<aionui_ai_agent::AgentInstance, AgentError> {
        Err(AgentError::internal("noop"))
    }
    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    async fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct EmptySkillResolver;

#[async_trait::async_trait]
impl SkillResolver for EmptySkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<aionui_extension::ResolvedAgentSkill> {
        Vec::new()
    }
}

async fn setup(
    with_kits: bool,
) -> (
    ConversationService,
    Arc<dyn IKitRepository>,
    Arc<dyn IConversationRepository>,
) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let kits: Arc<dyn IKitRepository> = Arc::new(SqliteKitRepository::new(db.pool().clone()));
    let svc = ConversationService::new(
        std::env::temp_dir(),
        Arc::new(NullBroadcaster),
        Arc::new(EmptySkillResolver),
        Arc::new(NoopTaskManager),
        repo.clone(),
        Arc::new(aionui_db::SqliteAgentMetadataRepository::new(db.pool().clone())),
        Arc::new(aionui_db::SqliteAcpSessionRepository::new(db.pool().clone())),
    );
    if with_kits {
        svc.with_kit_repo(kits.clone());
    }
    (svc, kits, repo)
}

fn create_req() -> CreateConversationRequest {
    let workspace = std::env::temp_dir().join("aionui-conversation-kit-skills-test");
    std::fs::create_dir_all(&workspace).unwrap();
    serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace.to_string_lossy(), "preset_enabled_skills": ["cron", "pdf"] }
    }))
    .unwrap()
}

fn attach<'a>(level: &'a str, target: &'a str, item: &'a str, op: &'a str) -> AttachParams<'a> {
    AttachParams {
        user_id: USER_ID,
        level,
        target_id: target,
        item_kind: "skill",
        item_ref: item,
        op,
    }
}

async fn stored_extra(repo: &Arc<dyn IConversationRepository>, id: &str) -> serde_json::Value {
    let row = repo.get(USER_ID, id).await.unwrap().unwrap();
    serde_json::from_str(&row.extra).unwrap()
}

#[tokio::test]
async fn create_applies_kit_layers_on_top_of_the_base_snapshot() {
    let (svc, kits, repo) = setup(true).await;
    kits.upsert_attachment(&attach("global", "", "kit-skill", "add"))
        .await
        .unwrap();
    kits.upsert_attachment(&attach("global", "", "pdf", "remove"))
        .await
        .unwrap();

    let resp = svc.create(USER_ID, create_req()).await.unwrap();
    assert_eq!(resp.extra["skills"], json!(["cron", "kit-skill"]));
    assert_eq!(resp.extra["kit_base_skills"], json!(["cron", "pdf"]));
    let stored = stored_extra(&repo, &resp.id).await;
    assert_eq!(stored["skills"], json!(["cron", "kit-skill"]));
    assert_eq!(stored["kit_base_skills"], json!(["cron", "pdf"]));
}

#[tokio::test]
async fn without_attachments_or_kit_repo_the_snapshot_is_unchanged() {
    for with_kits in [true, false] {
        let (svc, _kits, repo) = setup(with_kits).await;
        let resp = svc.create(USER_ID, create_req()).await.unwrap();
        assert_eq!(resp.extra["skills"], json!(["cron", "pdf"]));
        assert!(resp.extra.get("kit_base_skills").is_none());
        assert!(stored_extra(&repo, &resp.id).await.get("kit_base_skills").is_none());
    }
}

#[tokio::test]
async fn conversation_level_reapply_starts_from_the_base() {
    let (svc, kits, repo) = setup(true).await;
    kits.upsert_attachment(&attach("global", "", "kit-skill", "add"))
        .await
        .unwrap();
    let resp = svc.create(USER_ID, create_req()).await.unwrap();
    assert_eq!(resp.extra["skills"], json!(["cron", "pdf", "kit-skill"]));

    let a = kits
        .upsert_attachment(&attach("conversation", &resp.id, "late", "add"))
        .await
        .unwrap();
    kits.upsert_attachment(&attach("conversation", &resp.id, "kit-skill", "remove"))
        .await
        .unwrap();
    let next = svc.reapply_kit_skills(USER_ID, &resp.id).await.unwrap();
    assert_eq!(next, Some(vec!["cron".to_owned(), "pdf".to_owned(), "late".to_owned()]));
    let stored = stored_extra(&repo, &resp.id).await;
    assert_eq!(stored["skills"], json!(["cron", "pdf", "late"]));
    assert_eq!(stored["kit_base_skills"], json!(["cron", "pdf"]));

    // Detaching the conversation-level add drops it again (not stuck in the snapshot).
    kits.delete_attachment(USER_ID, &a.id).await.unwrap();
    svc.reapply_kit_skills(USER_ID, &resp.id).await.unwrap();
    assert_eq!(stored_extra(&repo, &resp.id).await["skills"], json!(["cron", "pdf"]));
    // Nothing to change: no write.
    assert_eq!(svc.reapply_kit_skills(USER_ID, &resp.id).await.unwrap(), None);
    // Unknown conversation: no-op.
    assert_eq!(svc.reapply_kit_skills(USER_ID, "missing").await.unwrap(), None);
}
