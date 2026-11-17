use aionui_common::TimestampMs;

/// An owner rule (`rules`, migration 912).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RuleRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub enabled: bool,
    /// `owner` | `import`.
    pub origin: String,
    /// `rule_sources.id` for an imported rule, else empty.
    pub origin_ref: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub deleted_at: Option<TimestampMs>,
}

/// A file rules were imported from (`rule_sources`, migration 912).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RuleSourceRow {
    pub id: String,
    pub user_id: String,
    /// `global` | `project`.
    pub scope: String,
    /// Empty for global.
    pub project_id: String,
    pub path: String,
    /// File content at import time.
    pub original: String,
    /// `imported` | `managed` (AionUi writes the file).
    pub status: String,
    pub imported_at: TimestampMs,
    pub managed_at: Option<TimestampMs>,
}
