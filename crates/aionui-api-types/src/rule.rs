//! Rules system (card C37, TASK-131): owner-written rule documents that are
//! always applied to the conversations they are attached to through the kit
//! levels (`item_kind = "rule"`).
//!
//! Request types are `deny_unknown_fields`. Errors (`ApiError` code, status):
//! | code | status | when |
//! |---|---|---|
//! | `RULE_INVALID` | 400 | name/description/body fail validation, body over 8 KB, a scope's rules over 24 KB, bad import request |
//! | `RULE_NAME_TAKEN` | 409 | another live rule of the user has this name |
//! | `RULE_STALE` | 409 | `expected_updated_at` no longer matches |
//! | `RULE_ALREADY_IMPORTED` | 409 | the file was imported before |
//! | `RULE_CONFIRM_REQUIRED` | 400 | an import or a file rewrite without `confirm: true` |
//! | `RULE_NOT_FOUND` | 404 | no such rule / project / source |
//! | `MCP_MARKET_SESSION_REQUIRED` | 403 | a write from an agent (runtime token) or `--local` caller |

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Largest rule body (bytes).
pub const RULE_BODY_MAX_BYTES: usize = 8 * 1024;
/// Largest total of the rule bodies in effect for one scope (bytes).
pub const RULES_EFFECTIVE_MAX_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleResponse {
    pub id: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub enabled: bool,
    /// `owner` | `import`.
    pub origin: String,
    /// The file an imported rule came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `POST /api/rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRuleRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub body: String,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `PUT /api/rules/{id}`: PATCH semantics; `expected_updated_at` → 409
/// `RULE_STALE` when the rule changed meanwhile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRuleRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub expected_updated_at: Option<TimestampMs>,
}

/// Query of `GET /api/rules/effective`: a conversation, or a project/assistant
/// pair (preview for a new conversation).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleEffectiveQuery {
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub assistant_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEffectiveItem {
    pub id: String,
    pub name: String,
    /// `global` | `project` | `assistant` | `conversation`.
    pub level: String,
    pub bytes: usize,
    /// `false` = left out of the block because the 24 KB cap was reached.
    pub delivered: bool,
}

/// What a session of this scope gets. Running conversations pick a change up
/// at their next session start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEffectiveResponse {
    pub rules: Vec<RuleEffectiveItem>,
    /// The exact text prepended to the session's preset context; empty when
    /// no rule applies.
    pub block: String,
    pub bytes: usize,
    pub max_bytes: usize,
}

/// Query of `GET /api/rules/import/preview`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleImportQuery {
    /// `global` | `project`.
    pub scope: String,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleImportProposal {
    pub name: String,
    pub description: String,
    pub body: String,
    pub bytes: usize,
    /// A live rule already has this name (the import would fail).
    pub name_taken: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleImportFile {
    pub path: String,
    /// `new` | `imported` | `managed` | `pointer` (only refers to another
    /// file, nothing to import) | `missing`.
    pub status: String,
    pub bytes: usize,
    pub rules: Vec<RuleImportProposal>,
}

/// Nothing is written by the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleImportPreviewResponse {
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub files: Vec<RuleImportFile>,
}

/// `POST /api/rules/import`: import the `new` files of a scope as rules
/// attached at that scope. Files on disk are not touched.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleImportRequest {
    pub scope: String,
    #[serde(default)]
    pub project_id: Option<String>,
    /// Must be `true`: the owner saw the preview.
    pub confirm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSourceResponse {
    pub id: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub path: String,
    /// `imported` | `managed`.
    pub status: String,
    pub imported_at: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleImportResponse {
    pub sources: Vec<RuleSourceResponse>,
    pub rules: Vec<RuleResponse>,
}

/// `POST /api/rules/files/manage`: hand the imported files of a scope over to
/// AionUi. Global: the file is generated from the Global rules (and again
/// after every change). Project: each imported repo file becomes a one-line
/// pointer. AionUi never commits or pushes these files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleManageFilesRequest {
    pub scope: String,
    #[serde(default)]
    pub project_id: Option<String>,
    /// Must be `true`: the owner confirmed the rewrite for this scope.
    pub confirm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleManageFilesResponse {
    pub written: Vec<RuleSourceResponse>,
}
