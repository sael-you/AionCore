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

        let row = match self.repo.get(&self.user_id, conversation_id).await {
            Ok(Some(row)) => row,
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
        };

        // A turn that ended because the user pressed stop is `cancelled`
        // whatever the relay's terminal said.
        let outcome = if self.persistence.lifecycle_for(conversation_id) == RuntimeLifecycleState::Cancelling {
            ChildResultStatus::Cancelled
        } else {
            outcome
        };
        let update = ConversationRowUpdate {
            status: Some("finished".to_owned()),
            extra: extra_with_last_outcome(&row.extra, outcome),
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
            "last_outcome": outcome.as_str(),
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
        tokio::spawn(report_child_result(
            self.repo.clone(),
            delivery,
            self.user_id.clone(),
            conversation_id.to_owned(),
            outcome,
        ));
    }
}

/// `extra` with `last_outcome` set to how the last turn ended
/// (`finished` / `error` / `cancelled`), so lists can show a failed
/// conversation without a status column change (the `conversations.status`
/// CHECK constraint only allows pending / running / finished). `None` (leave
/// `extra` untouched) when the stored value is not a JSON object.
pub(crate) fn extra_with_last_outcome(extra: &str, outcome: ChildResultStatus) -> Option<String> {
    let mut value: serde_json::Value = if extra.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(extra).ok()?
    };
    let object = value.as_object_mut()?;
    if object.get("last_outcome").and_then(|v| v.as_str()) == Some(outcome.as_str()) {
        return None;
    }
    object.insert("last_outcome".to_owned(), serde_json::Value::from(outcome.as_str()));
    serde_json::to_string(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_outcome_is_merged_into_extra() {
        let merged = extra_with_last_outcome(r#"{"workspace":"/w","backend":"claude"}"#, ChildResultStatus::Error)
            .expect("changed");
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["last_outcome"], "error");
        assert_eq!(value["workspace"], "/w", "other keys survive");
        assert_eq!(value["backend"], "claude");
    }

    #[test]
    fn last_outcome_unchanged_skips_the_write() {
        assert!(extra_with_last_outcome(r#"{"last_outcome":"finished"}"#, ChildResultStatus::Finished).is_none());
        let next = extra_with_last_outcome(r#"{"last_outcome":"error"}"#, ChildResultStatus::Finished).unwrap();
        assert!(next.contains(r#""last_outcome":"finished""#));
    }

    #[test]
    fn last_outcome_handles_empty_and_invalid_extra() {
        assert_eq!(
            extra_with_last_outcome("", ChildResultStatus::Cancelled).as_deref(),
            Some(r#"{"last_outcome":"cancelled"}"#)
        );
        assert!(extra_with_last_outcome("not json", ChildResultStatus::Error).is_none());
        assert!(extra_with_last_outcome("[1]", ChildResultStatus::Error).is_none());
    }
}
