use aionui_common::TimestampMs;

/// A kit (`kits`, migration 907). Members live in [`KitItemRow`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct KitRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub description: String,
    /// `K1`..`K12` for the kits seeded from KITS.md; `None` for owner kits.
    pub builtin_key: Option<String>,
    /// C28 (migration 908): `Some` = the project kit of this project.
    pub project_id: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub deleted_at: Option<TimestampMs>,
}

/// One member of a kit: `kind` = `skill` | `mcp` | `template`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct KitItemRow {
    pub kit_id: String,
    pub user_id: String,
    pub kind: String,
    #[sqlx(rename = "ref")]
    pub item_ref: String,
}

/// One attachment (`kit_attachments`, migration 907).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct KitAttachmentRow {
    pub id: String,
    pub user_id: String,
    /// `global` | `project` | `assistant` | `conversation`.
    pub level: String,
    /// `''` for global.
    pub target_id: String,
    /// `kit` | `skill` | `mcp`.
    pub item_kind: String,
    pub item_ref: String,
    /// `add` | `remove`.
    pub op: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
