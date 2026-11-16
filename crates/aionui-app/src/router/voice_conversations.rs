//! `ConversationPort` adapter for voice (card C9 S1b) over the existing
//! conversation service, sidebar store and worker task manager. Voice never
//! reaches into these directly.

use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    ConversationCreateRequest, ListConversationsQuery, ListMessagesQuery, SearchMessagesQuery, SendMessageRequest,
};
use aionui_common::{MessagePosition, MessageType};
use aionui_conversation::{ConversationError, ConversationService};
use aionui_db::ISidebarStore;
use aionui_voice::{
    ConversationInfo, ConversationPort, ConversationStatus, MessageHit, ProjectInfo, SendOutcome, SendProbe,
};

pub(crate) struct VoiceConversationAdapter {
    pub service: ConversationService,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub sidebar: Arc<dyn ISidebarStore>,
    /// Per draft key: one conversation create at a time.
    pub create_locks: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

const LIST_LIMIT: u32 = 200;

fn text_of(content: &serde_json::Value) -> Option<String> {
    content
        .get("content")
        .and_then(|v| v.as_str())
        .or_else(|| content.as_str())
        .map(str::to_owned)
}

impl VoiceConversationAdapter {
    async fn list(&self, user_id: &str) -> Result<Vec<aionui_api_types::ConversationResponse>, String> {
        self.service
            .list(
                user_id,
                ListConversationsQuery {
                    cursor: None,
                    limit: Some(LIST_LIMIT),
                    source: None,
                    cron_job_id: None,
                    pinned: None,
                },
            )
            .await
            .map(|page| page.items)
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl ConversationPort for VoiceConversationAdapter {
    async fn list_conversations(&self, user_id: &str) -> Result<Vec<ConversationInfo>, String> {
        Ok(self
            .list(user_id)
            .await?
            .into_iter()
            .map(|c| ConversationInfo {
                id: c.id,
                name: c.name,
                project_id: c.project_id,
                updated_at: c.modified_at,
            })
            .collect())
    }

    async fn list_projects(&self, user_id: &str) -> Result<Vec<ProjectInfo>, String> {
        Ok(self
            .sidebar
            .list_user_projects(user_id)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|p| ProjectInfo {
                id: p.project_id,
                name: p.name,
                root: p.workspace_canonical.or(p.workspace_uri),
            })
            .collect())
    }

    async fn status(&self, user_id: &str, conversation_id: &str) -> Result<Option<ConversationStatus>, String> {
        match self.service.get(user_id, conversation_id).await {
            Ok(conversation) => {
                let runtime = self.service.runtime_summary_for(conversation_id).await;
                let _ = conversation;
                Ok(Some(ConversationStatus {
                    is_processing: runtime.is_processing,
                    pending_confirmations: runtime.pending_confirmations,
                    active_turn_id: self.service.runtime_state().active_turn_id_for(conversation_id),
                    can_steer: runtime.supports_midturn_delivery,
                }))
            }
            Err(ConversationError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn last_reply(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, String> {
        let page = self
            .service
            .list_messages(
                user_id,
                conversation_id,
                ListMessagesQuery {
                    limit: Some(30),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(page
            .items
            .iter()
            .rev()
            .find(|m| m.r#type == MessageType::Text && m.position == Some(MessagePosition::Left))
            .and_then(|m| text_of(&m.content)))
    }

    async fn search_messages(&self, user_id: &str, keyword: &str, limit: usize) -> Result<Vec<MessageHit>, String> {
        let page = self
            .service
            .search_messages(
                user_id,
                SearchMessagesQuery {
                    keyword: keyword.to_owned(),
                    page: Some(1),
                    page_size: Some(limit.min(20) as u32),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(page
            .items
            .into_iter()
            .map(|item| MessageHit {
                conversation_id: item.conversation.id,
                conversation_name: item.conversation.name,
                snippet: item.preview_text,
            })
            .collect())
    }

    async fn send_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        text: &str,
        idempotency_key: &str,
    ) -> SendOutcome {
        let req: SendMessageRequest = match serde_json::from_value(serde_json::json!({ "content": text })) {
            Ok(req) => req,
            Err(e) => return SendOutcome::Rejected(e.to_string()),
        };
        match self
            .service
            .send_message_idempotent(user_id, conversation_id, req, &self.task_manager, idempotency_key)
            .await
        {
            Ok(response) => SendOutcome::Delivered {
                msg_id: response.msg_id,
                turn_id: response.turn_id,
                midturn: response.delivered_midturn,
            },
            Err(ConversationError::Busy { .. }) => SendOutcome::Busy,
            Err(ConversationError::NotFound { .. }) => SendOutcome::NotFound,
            Err(e) => SendOutcome::Rejected(e.to_string()),
        }
    }

    async fn probe_send(&self, user_id: &str, conversation_id: &str, msg_id: &str) -> Result<SendProbe, String> {
        let page = self
            .service
            .list_messages(
                user_id,
                conversation_id,
                ListMessagesQuery {
                    limit: Some(50),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        let sent_at = page
            .items
            .iter()
            .find(|m| m.msg_id.as_deref() == Some(msg_id) || m.id == msg_id)
            .map(|m| m.created_at);
        let after =
            |m: &&aionui_api_types::MessageResponse| sent_at.is_some_and(|t| m.created_at >= t) && m.id != msg_id;
        let newer: Vec<_> = page.items.iter().filter(after).collect();
        Ok(SendProbe {
            turn_active: self
                .service
                .runtime_state()
                .active_turn_id_for(conversation_id)
                .is_some(),
            replied: newer
                .iter()
                .any(|m| m.position == Some(MessagePosition::Left) && m.r#type != MessageType::Tips),
            failed: newer.iter().any(|m| {
                m.r#type == MessageType::Tips && m.content.get("type").and_then(|t| t.as_str()) == Some("error")
            }),
        })
    }

    async fn create_conversation(
        &self,
        user_id: &str,
        project_id: Option<&str>,
        title: &str,
        draft_key: &str,
    ) -> Result<String, String> {
        // One create per draft key at a time: a concurrent retry waits and
        // then finds the conversation by its key.
        let lock = self
            .create_locks
            .lock()
            .map_err(|_| "voice create lock poisoned".to_owned())?
            .entry(draft_key.to_owned())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        let conversations = self.list(user_id).await?;
        if let Some(existing) = conversations
            .iter()
            .find(|c| c.extra.get("voice_draft_key").and_then(|v| v.as_str()) == Some(draft_key))
        {
            return Ok(existing.id.clone());
        }
        // The new conversation inherits the setup (agent, workspace) of the
        // project's most recent conversation, like the agent-facing create.
        let source = conversations
            .iter()
            .filter(|c| project_id.is_none() || c.project_id.as_deref() == project_id)
            .max_by_key(|c| c.modified_at)
            .ok_or_else(|| "no conversation in that project to copy the setup from".to_owned())?;
        // The key is stored by the create itself: a crash can never leave a
        // conversation without it (a retry would then create a duplicate).
        let mut keys = serde_json::Map::new();
        keys.insert(
            "voice_draft_key".to_owned(),
            serde_json::Value::String(draft_key.to_owned()),
        );
        let created = self
            .service
            .create_for_conversation_helper_with_extra(
                user_id,
                &source.id,
                &ConversationCreateRequest {
                    name: title.to_owned(),
                    workspace: None,
                    assistant_id: None,
                    detached: true,
                    report_to_parent: false,
                },
                &keys,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(created.id)
    }
}
