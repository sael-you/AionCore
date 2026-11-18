//! Project page (card C42, TASK-169): usage analytics, disk usage and the
//! overview tiles of one project. All routes are owner-scoped reads behind
//! the ordinary auth middleware, except the storage rescan (a write: Origin
//! guard like every other state-changing project route).
//!
//! Token counters are per-turn sums (never context occupancy). `cost_micro_usd`
//! is micro-USD (1 USD = 1_000_000) and absent when no backend of the bucket
//! reported a cost. Days are UTC calendar days, `YYYY-MM-DD`; a response
//! holds every day of the range, oldest first, empty days zero-filled.
//! Times are epoch milliseconds.
//!
//! Errors (`ApiError` code, HTTP status):
//! | code | status | when |
//! |---|---|---|
//! | `project_not_found` | 404 | no such project for this user |
//! | `usage_invalid_days` | 400 | `days` is not 7, 30 or 90 |

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Query of `GET /api/projects/{id}/usage` and `GET /api/usage/summary`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageQuery {
    /// 7, 30 (default) or 90.
    #[serde(default)]
    pub days: Option<u32>,
}

/// Token and cost sums of one bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Reasoning tokens (already counted in `output` by the backends that report them).
    pub thought: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micro_usd: Option<i64>,
    /// Usage reports recorded (about one per turn).
    pub turns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageDay {
    /// `YYYY-MM-DD` (UTC).
    pub day: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageByConversation {
    pub conversation_id: String,
    /// Empty when the conversation no longer exists.
    pub name: String,
    pub last_at: TimestampMs,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageByModel {
    /// Model id as the backend reported it; empty when unknown.
    pub model: String,
    /// Backend label (`claude`, `codex`, `gemini`, ...); empty when unknown.
    pub backend: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageByProject {
    /// `None` = conversations outside any project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Empty for `project_id = None` or a deleted project.
    pub name: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

/// `GET /api/projects/{id}/usage?days=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectUsageResponse {
    /// First recorded turn of this project (recording started with this
    /// release; older history is not back-filled). `None` = nothing recorded yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<TimestampMs>,
    pub range_days: u32,
    pub totals: UsageTotals,
    pub days: Vec<UsageDay>,
    /// Top 10 by `input + output`, largest first.
    pub by_conversation: Vec<UsageByConversation>,
    /// Largest first.
    pub by_model: Vec<UsageByModel>,
}

/// `GET /api/usage/summary?days=`: all projects of the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummaryResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<TimestampMs>,
    pub range_days: u32,
    pub totals: UsageTotals,
    pub days: Vec<UsageDay>,
    /// Largest first.
    pub by_project: Vec<UsageByProject>,
    pub by_model: Vec<UsageByModel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageDisk {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageChild {
    pub name: String,
    /// For a `dir`: everything below it (symlinks not followed).
    pub bytes: u64,
    /// `dir` | `file` | `symlink` | `other`.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageFile {
    /// Path relative to the root, `/`-separated.
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageRoot {
    /// `workspace` | `attached` | `memory`.
    pub label: String,
    /// Absolute path.
    pub path: String,
    /// `false` = the directory does not exist (bytes 0, no children).
    pub exists: bool,
    /// Apparent size of the files found (not disk blocks).
    pub bytes: u64,
    pub entries: u64,
    /// The walk stopped at 200 000 entries or 5 s: `bytes` is a lower bound.
    pub truncated: bool,
    /// Top-level entries, largest first, at most 20.
    pub children: Vec<StorageChild>,
    /// Largest first, at most 10.
    pub largest_files: Vec<StorageFile>,
}

/// `GET /api/projects/{id}/storage` (cached up to 10 minutes; the first call
/// scans) and `POST /api/projects/{id}/storage/rescan` (scans now, returns
/// the new result with 200).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStorageResponse {
    pub scanned_at: TimestampMs,
    /// Sum of `roots[].bytes`.
    pub total_bytes: u64,
    /// The file system holding the workspace; absent when it cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<StorageDisk>,
    pub roots: Vec<StorageRoot>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewConversations {
    /// Not archived.
    pub active: i64,
    pub total: i64,
}

/// What conversations of this project get from attachments and kits (the
/// `included` decisions of `GET /api/kits/effective?project_id=`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewKit {
    pub skills: i64,
    pub mcps: i64,
    pub rules: i64,
    pub servers: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverviewStorage {
    pub total_bytes: u64,
    pub scanned_at: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<StorageDisk>,
}

/// `GET /api/projects/{id}/overview`: the tiles in one call. Never scans the
/// disk: `storage` comes from the cached scan and is absent until one ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectOverviewResponse {
    pub project_id: String,
    pub name: String,
    pub conversations: OverviewConversations,
    /// Board cards outside a `done` column (0 when the project has no board).
    pub open_cards: i64,
    pub kit: OverviewKit,
    /// Files in the project's Claude memory dir; absent when it has no workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_files: Option<i64>,
    /// Today (UTC).
    pub tokens_today: UsageTotals,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<OverviewStorage>,
}
