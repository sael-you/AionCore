//! Kits (card C27, D49/D50) on the skill snapshot of a conversation.
//!
//! The pre-kit snapshot (`compute_initial_skills`, unchanged) is the BASE; the
//! conversation's kit decisions (global → project → assistant → conversation,
//! `aionui_db::kit_resolve`) are applied on top. The first time kits change the
//! list, the base is kept in `extra.kit_base_skills` so a later re-apply (a
//! conversation-level attach/detach) starts from it again instead of from an
//! already-modified list. Writes are one `json_set` (`set_extra_keys`), and the
//! skill view (layer 1 argv/protocol + layer 2 injection) is rebuilt.
//!
//! Kept out of `service.rs` (already far past the file-size target).

use aionui_common::now_ms;
use aionui_db::kit_resolve;
use tracing::info;

use crate::ConversationError;
use crate::service::ConversationService;

impl ConversationService {
    /// Re-apply the conversation's kit skill decisions to its frozen snapshot
    /// and skill view. No-op without a kit repository, for an unknown
    /// conversation, or when the list would not change. Returns the new list
    /// when it changed.
    pub async fn reapply_kit_skills(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<Vec<String>>, ConversationError> {
        self.apply_kit_skills(user_id, conversation_id, None).await
    }

    /// `assistant_hint`: the create-time assistant when `extra` does not carry
    /// `preset_assistant_id` (an assistant passed as `req.assistant`).
    pub(crate) async fn apply_kit_skills(
        &self,
        user_id: &str,
        conversation_id: &str,
        assistant_hint: Option<&str>,
    ) -> Result<Option<Vec<String>>, ConversationError> {
        let Some(kits) = self.kit_repo() else {
            return Ok(None);
        };
        let Some(mut scope) = kits.conversation_scope(user_id, conversation_id).await? else {
            return Ok(None);
        };
        if scope.assistant_id.is_none() {
            scope.assistant_id = assistant_hint.filter(|a| !a.is_empty()).map(str::to_owned);
        }
        let Some(current) = kits.conversation_skills(user_id, conversation_id).await? else {
            return Ok(None);
        };
        let layers = kits.load_layers(user_id, &scope).await?;
        let resolution = kit_resolve::resolve(&scope, &layers);
        let base = current
            .kit_base_skills
            .clone()
            .unwrap_or_else(|| current.skills.clone());
        let next = resolution.apply_skills(&base);
        if next == current.skills {
            return Ok(None);
        }
        let mut entries = vec![("skills", serde_json::json!(next))];
        if current.kit_base_skills.is_none() {
            entries.push(("kit_base_skills", serde_json::json!(base)));
        }
        self.conversation_repo()
            .set_extra_keys(user_id, conversation_id, &entries, None, Some(now_ms()))
            .await?;
        let resolver = self.skill_resolver();
        let resolved = if next.is_empty() {
            Vec::new()
        } else {
            resolver.resolve_skills_for_user(user_id, &next).await
        };
        if resolved.is_empty() {
            resolver.remove_skill_view(user_id, conversation_id).await;
        } else {
            resolver.sync_skill_view(user_id, conversation_id, &resolved).await;
        }
        info!(
            conversation_id = %conversation_id,
            before = current.skills.len(),
            after = next.len(),
            "kits: conversation skill snapshot updated"
        );
        Ok(Some(next))
    }
}
