use crate::error::DbError;
use crate::kit_resolve::KitScope;
use crate::models::{RuleRow, RuleSourceRow};

/// `DbError::Conflict` message of a rule name already taken by a live rule.
pub const RULE_NAME_TAKEN_MSG: &str = "rule name already in use";
/// `DbError::Conflict` message of a file that was already imported.
pub const RULE_SOURCE_EXISTS_MSG: &str = "file already imported";

/// Data access for rules (migration 912). Validation, caps, composition and
/// the file import live in `aionui-mcp`'s `rules` module; attachment lives in
/// `kit_attachments` (`item_kind = 'rule'`), resolved by [`crate::kit_resolve`].
#[async_trait::async_trait]
pub trait IRuleRepository: Send + Sync {
    /// Live rules of a user, by name.
    async fn list_rules(&self, user_id: &str) -> Result<Vec<RuleRow>, DbError>;

    async fn find_rule(&self, user_id: &str, id: &str) -> Result<Option<RuleRow>, DbError>;

    /// `Conflict` ([`RULE_NAME_TAKEN_MSG`]) on a taken name.
    async fn create_rule(&self, params: &CreateRuleParams<'_>) -> Result<RuleRow, DbError>;

    /// Column-level update in one statement (`None` keeps the value). With
    /// `expected_updated_at` the write happens only if the rule is unchanged
    /// since it was read (`Conflict` otherwise).
    async fn update_rule(&self, user_id: &str, id: &str, params: &UpdateRuleParams<'_>) -> Result<RuleRow, DbError>;

    /// Soft-delete a rule and delete every attachment and kit membership of
    /// it, in one transaction. Returns whether a live rule was deleted.
    async fn delete_rule(&self, user_id: &str, id: &str) -> Result<bool, DbError>;

    /// The enabled rules in effect for `scope`, each with the level that
    /// decided it, ordered global → project → assistant → conversation, then
    /// by name.
    async fn effective_rules(&self, user_id: &str, scope: &KitScope) -> Result<Vec<EffectiveRule>, DbError>;

    /// [`Self::effective_rules`] for a conversation (its project and
    /// assistant are read from it); empty when it does not exist.
    async fn conversation_rules(&self, user_id: &str, conversation_id: &str) -> Result<Vec<EffectiveRule>, DbError>;

    /// One transaction: record the source file, insert the rules (origin
    /// `import`) and attach each with `add` at (`level`, `target_id`).
    /// `Conflict` when the file was already imported or a name is taken.
    async fn import_rules(&self, params: &ImportRulesParams<'_>) -> Result<(RuleSourceRow, Vec<RuleRow>), DbError>;

    async fn list_rule_sources(&self, user_id: &str) -> Result<Vec<RuleSourceRow>, DbError>;

    /// Mark a source as managed by AionUi (the file is generated / a pointer).
    async fn mark_rule_source_managed(&self, user_id: &str, id: &str) -> Result<RuleSourceRow, DbError>;
}

/// A rule in effect and the level that decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRule {
    pub rule: RuleRow,
    /// `global` | `project` | `assistant` | `conversation`.
    pub level: &'static str,
}

#[derive(Debug, Clone)]
pub struct CreateRuleParams<'a> {
    pub user_id: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub body: &'a str,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateRuleParams<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub body: Option<&'a str>,
    pub enabled: Option<bool>,
    pub expected_updated_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ImportedRule<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub body: &'a str,
}

#[derive(Debug, Clone)]
pub struct ImportRulesParams<'a> {
    pub user_id: &'a str,
    /// `global` | `project`.
    pub scope: &'a str,
    /// Empty for global.
    pub project_id: &'a str,
    pub path: &'a str,
    pub original: &'a str,
    pub rules: &'a [ImportedRule<'a>],
}
