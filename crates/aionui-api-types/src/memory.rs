//! Project memory review (card C29, TASK-109): the Claude Code auto-memory of a
//! project's workspace (`~/.claude/projects/<slug>/memory/`), read and edited
//! where it lives. Routes under `/api/projects/{project_id}/memory`.

use serde::{Deserialize, Serialize};

/// `GET /api/projects/{project_id}/memory`: every memory file, no bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryListResponse {
    /// `false`: the project has no memory yet (`items` and `lint` empty).
    pub dir_exists: bool,
    /// Lines of `MEMORY.md` (0 when it is missing).
    pub index_lines: u32,
    /// Sorted by `modified`, newest first.
    pub items: Vec<MemoryItem>,
    pub lint: Vec<MemoryLint>,
}

/// One memory file. Frontmatter fields are `None` when absent or malformed
/// (the file is still listed; its raw frontmatter is on the detail route).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryItem {
    /// File name inside the memory dir, e.g. `feedback_tests.md`.
    pub file: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// `metadata.type`: `user` | `feedback` | `project` | `reference` (as written).
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// File modification time, unix milliseconds (the `expected_modified` of a PUT).
    pub modified: i64,
    /// `metadata.modified` as written in the frontmatter (ISO 8601), if any.
    pub frontmatter_modified: Option<String>,
    /// Bytes.
    pub size: u64,
    /// `[[slug]]` links in the body, in order, deduplicated.
    pub links: Vec<String>,
    /// `MEMORY.md` has a line pointing to this file.
    pub in_index: bool,
    /// Links whose slug matches no memory (`name` or file stem).
    pub broken_links: Vec<String>,
    /// A `project` memory not modified for more than 30 days.
    pub stale: bool,
}

/// A lint finding computed on the list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryLint {
    /// `duplicate` | `not_in_index` | `missing_file` | `broken_link` | `stale`.
    pub kind: String,
    /// The memory file concerned; for `missing_file` the file the index names.
    pub file: Option<String>,
    /// Human-readable detail (English), e.g. the duplicate's other file.
    pub detail: String,
}

/// `GET /api/projects/{project_id}/memory/{file}` (and the result of PUT / restore).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryFileResponse {
    pub file: String,
    /// The frontmatter between the `---` lines, verbatim (empty when none).
    pub frontmatter_raw: String,
    pub body: String,
    /// File modification time, unix milliseconds.
    pub modified: i64,
}

/// `PUT /api/projects/{project_id}/memory/{file}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMemoryFileRequest {
    pub body: String,
    /// Written back between `---` lines; empty → no frontmatter.
    pub frontmatter_raw: String,
    /// The `modified` the client read; another value now → 409 `MEMORY_CHANGED`.
    pub expected_modified: i64,
}

/// `DELETE /api/projects/{project_id}/memory/{file}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryDeleteResponse {
    pub file: String,
    /// The trash copy (`.trash/<file>.<unix ms>`) `restore` brings back.
    pub trash_file: String,
    /// `MEMORY.md` lines removed (lines linking to this file).
    pub index_lines_removed: u32,
}
