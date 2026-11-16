use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::kit_resolve::{KitLayers, KitScope};
use crate::models::{KitAttachmentRow, KitItemRow, KitRow};

/// Data access for kits and their attachments (migration 907). Business rules
/// (validation, R2/R3, seed content) live in `aionui-mcp`'s market service;
/// resolution lives in [`crate::kit_resolve`].
#[async_trait::async_trait]
pub trait IKitRepository: Send + Sync {
    /// Live kits of a user, oldest first.
    async fn list_kits(&self, user_id: &str) -> Result<Vec<KitRow>, DbError>;

    /// A live kit by id.
    async fn find_kit(&self, user_id: &str, id: &str) -> Result<Option<KitRow>, DbError>;

    /// Members of the given kits (all live kits' members when `kit_ids` is `None`).
    async fn list_kit_items(&self, user_id: &str, kit_ids: Option<&[String]>) -> Result<Vec<KitItemRow>, DbError>;

    /// Insert a kit and its members in one transaction (`Conflict` when a
    /// live kit of the user already has this name).
    async fn create_kit(&self, params: &CreateKitParams<'_>) -> Result<KitRow, DbError>;

    /// Update name/description and replace the given member sets in one
    /// transaction. With `expected_updated_at` the write happens only if the
    /// kit is unchanged since it was read (`Conflict` otherwise).
    async fn update_kit(&self, user_id: &str, id: &str, params: &UpdateKitParams<'_>) -> Result<KitRow, DbError>;

    /// Soft-delete a kit, delete its members and every attachment of it, in
    /// one transaction. Returns whether a live kit was deleted.
    async fn delete_kit(&self, user_id: &str, id: &str) -> Result<bool, DbError>;

    /// Insert the seed kits a user does not have yet (`builtin_key` unique per
    /// user, soft-deleted ones included, so a deleted builtin never comes back).
    /// Returns how many kits were inserted.
    async fn seed_kits(&self, user_id: &str, seeds: &[SeedKit<'_>]) -> Result<usize, DbError>;

    /// Attachments of a user, optionally filtered by level and target.
    async fn list_attachments(
        &self,
        user_id: &str,
        level: Option<&str>,
        target_id: Option<&str>,
    ) -> Result<Vec<KitAttachmentRow>, DbError>;

    async fn find_attachment(&self, user_id: &str, id: &str) -> Result<Option<KitAttachmentRow>, DbError>;

    /// Insert, or flip the `op` of, the attachment (user, level, target, kind, ref).
    async fn upsert_attachment(&self, params: &AttachParams<'_>) -> Result<KitAttachmentRow, DbError>;

    async fn delete_attachment(&self, user_id: &str, id: &str) -> Result<bool, DbError>;

    /// The attachments of `scope` (all four levels) and the live kits they reference.
    async fn load_layers(&self, user_id: &str, scope: &KitScope) -> Result<KitLayers, DbError>;

    /// A conversation's scope: its project (`conversations.project_id`) and
    /// assistant (`extra.preset_assistant_id`). `None` when not found.
    async fn conversation_scope(&self, user_id: &str, conversation_id: &str) -> Result<Option<KitScope>, DbError>;

    /// A conversation's frozen skill snapshot (`extra.skills`) and the
    /// pre-kit base (`extra.kit_base_skills`, absent until kits changed it).
    /// `None` when the conversation does not exist.
    async fn conversation_skills(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ConversationSkills>, DbError>;
}

/// See [`IKitRepository::conversation_skills`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationSkills {
    pub skills: Vec<String>,
    pub kit_base_skills: Option<Vec<String>>,
}

/// `DbError::Conflict` message of a kit name already taken by a live kit.
pub const KIT_NAME_TAKEN_MSG: &str = "a kit with this name already exists";
/// `DbError::Conflict` message of an `expected_updated_at` mismatch.
pub const KIT_STALE_MSG: &str = "kit changed since it was read";

/// Members of a kit by kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct KitMembers<'a> {
    pub skills: &'a [String],
    pub mcp_instance_ids: &'a [String],
    pub mcp_templates: &'a [String],
}

#[derive(Debug, Clone, Copy)]
pub struct CreateKitParams<'a> {
    pub user_id: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub members: KitMembers<'a>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateKitParams<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub skills: Option<&'a [String]>,
    pub mcp_instance_ids: Option<&'a [String]>,
    pub mcp_templates: Option<&'a [String]>,
    pub expected_updated_at: Option<TimestampMs>,
}

/// One kit seeded from KITS.md.
#[derive(Debug, Clone, Copy)]
pub struct SeedKit<'a> {
    pub builtin_key: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub skills: &'a [&'a str],
    pub mcp_templates: &'a [&'a str],
}

#[derive(Debug, Clone, Copy)]
pub struct AttachParams<'a> {
    pub user_id: &'a str,
    pub level: &'a str,
    pub target_id: &'a str,
    pub item_kind: &'a str,
    pub item_ref: &'a str,
    pub op: &'a str,
}
