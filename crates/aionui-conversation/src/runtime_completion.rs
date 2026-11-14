use std::sync::Arc;

use aionui_api_types::{ConversationRuntimeSummary, WebSocketMessage};
use aionui_common::{ErrorChain, now_ms};
use aionui_db::{ConversationRowUpdate, IConversationRepository};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tracing::{debug, error};

use crate::child_report::report_child_result;
use crate::hierarchy::ChildResultStatus;
use crate::runtime_persistence::{RuntimePersistenceCoordinator, RuntimeWriteKind};
use crate::runtime_state::RuntimeLifecycleState;

#[derive(Clone)]
pub struct RuntimeCompletionPublisher {
    user_id: String,
    repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    persistence: RuntimePersistenceCoordinator,
}

impl RuntimeCompletionPublisher {
    pub fn new(
        user_id: String,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        persistence: RuntimePersistenceCoordinator,
    ) -> Self {
        Self {
            user_id,
            repo,
            broadcaster,
            persistence,
        }
    }

    pub async fn publish(&self, conversation_id: &str, turn_id: &str, runtime: Option<ConversationRuntimeSummary>) {
        self.publish_with_outcome(conversation_id, turn_id, runtime, ChildResultStatus::Finished)
            .await;
    }

    /// Same as [`Self::publish`], but records how the turn ended so a child
    /// conversation's result report (hierarchical conversations) carries the
    /// right status. The persisted / broadcast status stays `finished` — only
    /// the report header distinguishes `error` / `cancelled`.
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id, turn_id = %turn_id))]
    pub async fn publish_with_outcome(
        &self,
        conversation_id: &str,
        turn_id: &str,
        runtime: Option<ConversationRuntimeSummary>,
        outcome: ChildResultStatus,
    ) {
        if !self
            .persistence
            .allows(conversation_id, RuntimeWriteKind::ConversationFinished)
        {
            debug!(
                conversation_id,
                turn_id, "turn completion skipped by runtime persistence policy"
            );
            return;
        }

        match self.repo.get(&self.user_id, conversation_id).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                debug!(
                    conversation_id,
                    turn_id, "turn completion skipped because conversation row is missing"
                );
                return;
            }
            Err(error) => {
                error!(
                    conversation_id,
                    turn_id,
                    error = %ErrorChain(&error),
                    "turn completion skipped because conversation row lookup failed"
                );
                return;
            }
        }

        let update = ConversationRowUpdate {
            status: Some("finished".to_owned()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(error) = self.repo.update(&self.user_id, conversation_id, &update).await {
            error!(
                conversation_id,
                turn_id,
                error = %ErrorChain(&error),
                "Failed to update conversation status"
            );
            return;
        }

        let payload = json!({
            "user_id": self.user_id,
            "conversation_id": conversation_id,
            "session_id": conversation_id,
            "turn_id": turn_id,
            "status": "finished",
            "canSendMessage": true,
            "runtime": runtime,
        });
        self.broadcaster
            .broadcast(WebSocketMessage::new("turn.completed", payload));

        debug!(conversation_id, turn_id, status = "finished", "Turn completed");

        self.spawn_child_result_report(conversation_id, outcome);
    }

    /// Fire-and-forget: the report must never delay or fail turn completion.
    fn spawn_child_result_report(&self, conversation_id: &str, outcome: ChildResultStatus) {
        let Some(delivery) = self.persistence.child_result_delivery() else {
            return;
        };
        // A turn that ended because the user pressed stop is `cancelled`
        // whatever the relay's terminal said.
        let status = if self.persistence.lifecycle_for(conversation_id) == RuntimeLifecycleState::Cancelling {
            ChildResultStatus::Cancelled
        } else {
            outcome
        };
        tokio::spawn(report_child_result(
            self.repo.clone(),
            delivery,
            self.user_id.clone(),
            conversation_id.to_owned(),
            status,
        ));
    }
}
