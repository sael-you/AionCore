use crate::error::DbError;

/// One usage report of a finished turn (per-turn token split).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTurnParams<'a> {
    pub user_id: &'a str,
    pub conversation_id: &'a str,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub thought_tokens: i64,
    /// Cost of this turn in micro-USD; `None` when the backend reports none.
    pub cost_micro_usd: Option<i64>,
    /// Epoch ms.
    pub at: i64,
}

impl UsageTurnParams<'_> {
    /// Nothing to record: a report without any per-turn figure (e.g. an ACP
    /// occupancy-only update).
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
            && self.thought_tokens == 0
            && self.cost_micro_usd.is_none_or(|c| c == 0)
    }
}

/// Sums of one bucket (`cost_micro_usd` `None` = no row carried a cost).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct UsageSums {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub thought_tokens: i64,
    pub cost_micro_usd: Option<i64>,
    pub turns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UsageDayRow {
    /// `YYYY-MM-DD` (UTC).
    pub day: String,
    #[sqlx(flatten)]
    pub sums: UsageSums,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UsageModelRow {
    pub model: String,
    pub backend: String,
    #[sqlx(flatten)]
    pub sums: UsageSums,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UsageConversationRow {
    pub conversation_id: String,
    /// Empty when the conversation no longer exists.
    pub name: String,
    pub last_at: i64,
    #[sqlx(flatten)]
    pub sums: UsageSums,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UsageProjectRow {
    /// Empty = outside any project.
    pub project_id: String,
    /// Empty for no project or a deleted one.
    pub name: String,
    #[sqlx(flatten)]
    pub sums: UsageSums,
}

/// Counts for the project page's overview tiles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectCounts {
    pub conversations_active: i64,
    pub conversations_total: i64,
    pub open_cards: i64,
    pub kit_skills: i64,
    pub kit_mcps: i64,
    pub kit_rules: i64,
    pub kit_servers: i64,
}

/// Usage analytics (migration 913) and the project page's counts. Every
/// method is scoped to `user_id`; `project` `Some("")` = conversations outside
/// any project, `None` = all projects of the user.
#[async_trait::async_trait]
pub trait IUsageRepository: Send + Sync {
    /// Insert one `usage_turns` row and add it to `usage_daily`, in one
    /// transaction. Project, backend and model are read from the conversation
    /// and its session. `Ok(false)` when the conversation is not the user's
    /// (nothing written) or the report is empty.
    async fn record_usage_turn(&self, params: &UsageTurnParams<'_>) -> Result<bool, DbError>;

    /// Epoch ms of the first recorded report of the scope.
    async fn usage_since(&self, user_id: &str, project: Option<&str>) -> Result<Option<i64>, DbError>;

    /// Per-day sums for `from_day..=to_day` (`YYYY-MM-DD`), days without rows omitted.
    async fn usage_by_day(
        &self,
        user_id: &str,
        project: Option<&str>,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<UsageDayRow>, DbError>;

    /// Per (model, backend) sums of the range, largest `input + output` first.
    async fn usage_by_model(
        &self,
        user_id: &str,
        project: Option<&str>,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<UsageModelRow>, DbError>;

    /// Per-project sums of the range, largest first.
    async fn usage_by_project(
        &self,
        user_id: &str,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<UsageProjectRow>, DbError>;

    /// Top conversations of a project since `from_ms`, largest `input + output` first.
    async fn usage_by_conversation(
        &self,
        user_id: &str,
        project_id: &str,
        from_ms: i64,
        limit: i64,
    ) -> Result<Vec<UsageConversationRow>, DbError>;

    /// The overview counts of a project (kit counts = `included` decisions of
    /// the project's effective set).
    async fn project_counts(&self, user_id: &str, project_id: &str) -> Result<ProjectCounts, DbError>;
}
