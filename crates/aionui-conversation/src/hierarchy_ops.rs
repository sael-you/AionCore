//! Hierarchical conversations on `ConversationService`: parent validation for
//! create, reparent, and the delete cascade.
//!
//! Kept out of `service.rs` (already far past the file-size target). The pure
//! rules live in [`crate::hierarchy`]; this file only feeds them rows and
//! applies the outcome.

use std::sync::Arc;

use aionui_api_types::ConversationResponse;
use aionui_common::{ChildResultDelivery, ErrorChain};
use aionui_db::models::ConversationRow;
use tracing::{info, warn};

use crate::ConversationError;
use crate::hierarchy::{self, HierarchyViolation};
use crate::service::ConversationService;
use crate::session_mentions::team_id_from_extra_str;

impl ConversationService {
    /// Install the port that carries a finished child's result report to its
    /// parent (see [`ChildResultDelivery`]). Set once at startup.
    pub fn with_child_result_delivery(&self, delivery: Arc<dyn ChildResultDelivery>) {
        self.runtime_state().set_child_result_delivery(delivery);
    }

    /// Load `parent_id` as the prospective parent of a NEW child and return the
    /// child's depth. Enforces: parent exists, is owned by `user_id` (the
    /// lookup is user-scoped), is not a team member, `depth(parent)+1 <=
    /// MAX_DEPTH`, and the parent has fewer than `MAX_CHILDREN_PER_PARENT`
    /// children.
    pub(crate) async fn validate_new_parent(&self, user_id: &str, parent_id: &str) -> Result<i64, ConversationError> {
        let parent = self.load_parent(user_id, parent_id).await?;
        let depth = hierarchy::check_new_child_depth(parent.depth).map_err(|violation| {
            ConversationError::hierarchy(
                violation,
                format!(
                    "parent conversation {parent_id} is at depth {}; the maximum nesting depth is {}",
                    parent.depth,
                    hierarchy::MAX_DEPTH
                ),
            )
        })?;
        self.check_parent_fanout(user_id, parent_id).await?;
        Ok(depth)
    }

    async fn load_parent(&self, user_id: &str, parent_id: &str) -> Result<ConversationRow, ConversationError> {
        let parent = self.conversation_repo().get(user_id, parent_id).await?.ok_or_else(|| {
            ConversationError::hierarchy(
                HierarchyViolation::ParentNotFound,
                format!("parent conversation not found: {parent_id}"),
            )
        })?;
        if team_id_from_extra_str(&parent.extra).is_some() {
            return Err(ConversationError::hierarchy(
                HierarchyViolation::ParentNotFound,
                format!("parent conversation {parent_id} belongs to a team and cannot have children"),
            ));
        }
        Ok(parent)
    }

    async fn check_parent_fanout(&self, user_id: &str, parent_id: &str) -> Result<(), ConversationError> {
        let children = self.conversation_repo().list_children(user_id, parent_id).await?;
        hierarchy::check_fanout(children.len()).map_err(|violation| {
            ConversationError::hierarchy(
                violation,
                format!(
                    "parent conversation {parent_id} already has {} children; the maximum is {}",
                    children.len(),
                    hierarchy::MAX_CHILDREN_PER_PARENT
                ),
            )
        })
    }

    /// Move `id` (with its whole subtree) under `new_parent`, or detach it to a
    /// root when `new_parent` is `None`.
    ///
    /// Validates: the conversation exists and is not a team member; the new
    /// parent is not the conversation itself or one of its descendants (cycle);
    /// the new parent has room (unless it is already the parent); the moved
    /// subtree still fits `MAX_DEPTH`. Depths are rewritten in one transaction,
    /// then `conversation.listChanged(updated)` is broadcast.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn reparent(
        &self,
        user_id: &str,
        id: &str,
        new_parent: Option<&str>,
    ) -> Result<ConversationResponse, ConversationError> {
        let repo = self.conversation_repo();
        let moved = repo
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::NotFound { id: id.to_owned() })?;
        if team_id_from_extra_str(&moved.extra).is_some() {
            return Err(ConversationError::bad_request(
                "team member conversations cannot be reparented",
            ));
        }
        let new_parent = new_parent.map(str::trim).filter(|value| !value.is_empty());
        let descendants = repo.list_descendants(user_id, id).await?;

        let new_parent_depth = match new_parent {
            None => None,
            Some(parent_id) => {
                if parent_id == id || descendants.iter().any(|row| row.id == parent_id) {
                    return Err(ConversationError::hierarchy(
                        HierarchyViolation::ParentCycle,
                        format!("conversation {parent_id} is {id} or one of its descendants"),
                    ));
                }
                let parent = self.load_parent(user_id, parent_id).await?;
                if moved.parent_conversation_id.as_deref() != Some(parent_id) {
                    self.check_parent_fanout(user_id, parent_id).await?;
                }
                Some(parent.depth)
            }
        };

        let depth_updates = hierarchy::plan_reparent_depths(&moved, &descendants, new_parent_depth).map_err(|v| {
            ConversationError::hierarchy(
                v,
                format!(
                    "moving conversation {id} with its {} descendants would exceed the maximum nesting depth of {}",
                    descendants.len(),
                    hierarchy::MAX_DEPTH
                ),
            )
        })?;
        repo.reparent_subtree(user_id, id, new_parent, &depth_updates).await?;

        info!(
            conversation_id = %id,
            new_parent = new_parent.unwrap_or("<root>"),
            moved = depth_updates.len(),
            "Conversation reparented"
        );
        let source = moved
            .source
            .as_deref()
            .and_then(|s| crate::convert::string_to_enum::<aionui_common::ConversationSource>(s).ok());
        self.broadcast_list_changed(user_id, id, "updated", source.as_ref());
        self.get(user_id, id).await
    }

    /// Delete every descendant of `id`, deepest first, each through the normal
    /// single-conversation delete (hooks, runtime teardown, workspace guard).
    /// Best-effort per child: a failure is logged and the sweep continues.
    pub(crate) async fn delete_descendants_best_effort(&self, user_id: &str, id: &str) {
        let mut descendants = match self.conversation_repo().list_descendants(user_id, id).await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(conversation_id = %id, error = %ErrorChain(&error), "Failed to list descendants for cascade delete");
                return;
            }
        };
        if descendants.is_empty() {
            return;
        }
        descendants.sort_by(|a, b| {
            b.depth
                .cmp(&a.depth)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.id.cmp(&a.id))
        });
        for child in descendants {
            if let Err(error) = self.delete_single(user_id, &child.id).await {
                warn!(
                    conversation_id = %id,
                    child_conversation_id = %child.id,
                    error = %ErrorChain(&error),
                    "Cascade delete of a child conversation failed; continuing"
                );
            }
        }
    }
}
