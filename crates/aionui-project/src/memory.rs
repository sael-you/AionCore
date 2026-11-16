//! Project memory review (card C29, TASK-109): the Claude Code auto-memory of a
//! project's workspace, read and edited where it lives —
//! `<claude projects root>/<slug>/memory/` (production root `~/.claude/projects`),
//! one markdown file per memory plus the `MEMORY.md` index. No copy, no table.
//!
//! Safety: file names are `[A-Za-z0-9._-]+\.md` (no leading dot), resolved inside
//! the memory dir with symlinks refused; at most 256 KB; every write or delete
//! first copies the old file to `.trash/<file>.<unix ms>` (kept 30 days), and
//! `restore` brings the newest copy back.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aionui_api_types::{MemoryDeleteResponse, MemoryFileResponse, MemoryItem, MemoryLint, MemoryListResponse};

/// The index Claude Code keeps next to the memories.
pub const INDEX_FILE: &str = "MEMORY.md";
pub const TRASH_DIR: &str = ".trash";
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
const TRASH_KEEP: Duration = Duration::from_secs(30 * 24 * 3600);
const STALE_AFTER_MS: i64 = 30 * 24 * 3600 * 1000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryError {
    #[error("invalid memory file name")]
    InvalidName,
    #[error("the project has no memory yet")]
    NoMemory,
    #[error("memory file not found")]
    NotFound,
    #[error("the memory file changed meanwhile")]
    Changed { current_modified: i64 },
    #[error("memory file larger than 256 KB")]
    TooLarge,
    #[error("the index cannot be deleted")]
    IndexNotDeletable,
    #[error("no trash copy to restore")]
    NothingToRestore,
    #[error("memory io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for MemoryError {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::NotFound {
            Self::NotFound
        } else {
            Self::Io(err.kind().to_string())
        }
    }
}

/// Claude Code's per-workspace folder name: every character outside
/// `[A-Za-z0-9-]` becomes `-` (verified against `~/.claude/projects`:
/// `/root/.clauterm/scratch` → `-root--clauterm-scratch`,
/// `system_default_user` → `system-default-user`).
pub fn slug(workspace: &Path) -> String {
    workspace
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

/// `[A-Za-z0-9._-]+\.md`, no leading dot (no hidden files, no `..`).
pub fn valid_file_name(file: &str) -> bool {
    file.len() > 3
        && file.len() <= 200
        && file.ends_with(".md")
        && !file.starts_with('.')
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Memory files of one root (`~/.claude/projects` in production, a temp dir in tests).
#[derive(Debug, Clone)]
pub struct MemoryService {
    root: PathBuf,
}

/// Parsed frontmatter fields (all optional: malformed → `None`).
#[derive(Debug, Default, PartialEq)]
struct Front {
    name: Option<String>,
    description: Option<String>,
    kind: Option<String>,
    modified: Option<String>,
}

fn now_ms() -> i64 {
    aionui_common::now_ms()
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// `(frontmatter_raw, body)`: the text between a leading `---` line and the
/// next `---` line, and everything after it. No frontmatter → `("", text)`.
pub fn split_frontmatter(text: &str) -> (String, String) {
    let rest = match text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) {
        Some(rest) => rest,
        None => return (String::new(), text.to_owned()),
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let raw = rest[..offset].trim_end_matches(['\n', '\r']).to_owned();
            let body = rest[offset + line.len()..].trim_start_matches(['\n', '\r']).to_owned();
            return (raw, body);
        }
        offset += line.len();
    }
    // No closing line: not frontmatter, show it all as the body.
    (String::new(), text.to_owned())
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        value[1..value.len() - 1].replace("''", "'")
    } else {
        value.to_owned()
    }
}

/// The fields the list shows: top-level `name`, `description`, and
/// `metadata.type` / `metadata.modified` (a top-level `type` / `modified` too).
fn parse_front(raw: &str) -> Front {
    let mut front = Front::default();
    let mut in_metadata = false;
    for line in raw.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), unquote(value));
        if !indented {
            in_metadata = key == "metadata" && value.is_empty();
        }
        let set = |slot: &mut Option<String>| {
            if !value.is_empty() && slot.is_none() {
                *slot = Some(value.clone());
            }
        };
        match (indented, in_metadata, key) {
            (false, _, "name") => set(&mut front.name),
            (false, _, "description") => set(&mut front.description),
            (false, _, "type") | (true, true, "type") => set(&mut front.kind),
            (false, _, "modified") | (true, true, "modified") => set(&mut front.modified),
            _ => {}
        }
    }
    front
}

/// `[[slug]]` links of a body, in order, deduplicated.
pub fn links(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let slug = after[..end].trim();
        if !slug.is_empty() && !slug.contains('\n') && !out.iter().any(|s| s == slug) {
            out.push(slug.to_owned());
        }
        rest = &after[end + 2..];
    }
    out
}

/// Markdown link targets of the index that name a file in the dir
/// (`[Title](file.md)`; URLs and paths with `/` are not memory files).
fn index_targets(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let Some(end) = after.find(')') else { break };
        let target = after[..end].trim();
        if target.ends_with(".md") && !target.contains('/') && !target.contains(':') {
            out.push(target.to_owned());
        }
        rest = &after[end + 1..];
    }
    out
}

fn normalized_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn iso_to_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|t| t.timestamp_millis())
}

impl MemoryService {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Production root: `$HOME/.claude/projects`.
    pub fn from_home() -> Option<Self> {
        std::env::var_os("HOME").map(|home| Self::new(PathBuf::from(home).join(".claude").join("projects")))
    }

    /// The memory dir of a workspace (it may not exist).
    pub fn dir_for(&self, workspace: &Path) -> PathBuf {
        self.root.join(slug(workspace)).join("memory")
    }

    /// The memory dir, refusing a symlinked dir that leads outside the root.
    async fn existing_dir(&self, workspace: &Path) -> Result<PathBuf, MemoryError> {
        let dir = self.dir_for(workspace);
        let meta = match tokio::fs::symlink_metadata(&dir).await {
            Ok(meta) => meta,
            Err(_) => return Err(MemoryError::NoMemory),
        };
        if !meta.is_dir() {
            return Err(MemoryError::NoMemory);
        }
        let root = tokio::fs::canonicalize(&self.root).await?;
        let canonical = tokio::fs::canonicalize(&dir).await?;
        if !canonical.starts_with(&root) {
            return Err(MemoryError::NoMemory);
        }
        Ok(canonical)
    }

    /// A regular file directly inside `dir` (no symlink, size-checked).
    async fn file_path(dir: &Path, file: &str) -> Result<(PathBuf, std::fs::Metadata), MemoryError> {
        if !valid_file_name(file) {
            return Err(MemoryError::InvalidName);
        }
        let path = dir.join(file);
        let meta = tokio::fs::symlink_metadata(&path).await?;
        if !meta.is_file() {
            // A symlink or a directory is never read or written through.
            return Err(MemoryError::NotFound);
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(MemoryError::TooLarge);
        }
        Ok((path, meta))
    }

    pub async fn list(&self, workspace: &Path) -> Result<MemoryListResponse, MemoryError> {
        let dir = match self.existing_dir(workspace).await {
            Ok(dir) => dir,
            Err(MemoryError::NoMemory) => {
                return Ok(MemoryListResponse {
                    dir_exists: false,
                    index_lines: 0,
                    items: Vec::new(),
                    lint: Vec::new(),
                });
            }
            Err(err) => return Err(err),
        };
        let index = tokio::fs::read_to_string(dir.join(INDEX_FILE))
            .await
            .unwrap_or_default();
        let indexed: HashSet<String> = index.lines().flat_map(index_targets).collect();

        let mut files = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file = entry.file_name().to_string_lossy().into_owned();
            if file == INDEX_FILE || !valid_file_name(&file) {
                continue;
            }
            let Ok(meta) = tokio::fs::symlink_metadata(entry.path()).await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let (front, body) = if meta.len() <= MAX_FILE_BYTES {
                let text = tokio::fs::read_to_string(entry.path()).await.unwrap_or_default();
                let (raw, body) = split_frontmatter(&text);
                (parse_front(&raw), body)
            } else {
                (Front::default(), String::new())
            };
            files.push((file, meta, front, body));
        }

        // Link targets: a memory's `name`, or its file stem.
        let known: HashSet<String> = files
            .iter()
            .flat_map(|(file, _, front, _)| [Some(file.trim_end_matches(".md").to_owned()), front.name.clone()])
            .flatten()
            .collect();
        let now = now_ms();
        let mut items: Vec<MemoryItem> = files
            .iter()
            .map(|(file, meta, front, body)| {
                let links = links(body);
                let broken_links = links.iter().filter(|l| !known.contains(*l)).cloned().collect();
                let modified = mtime_ms(meta);
                let written = front.modified.as_deref().and_then(iso_to_ms).unwrap_or(modified);
                MemoryItem {
                    file: file.clone(),
                    name: front.name.clone(),
                    description: front.description.clone(),
                    kind: front.kind.clone(),
                    modified,
                    frontmatter_modified: front.modified.clone(),
                    size: meta.len(),
                    links,
                    in_index: indexed.contains(file),
                    broken_links,
                    stale: front.kind.as_deref() == Some("project") && now - written > STALE_AFTER_MS,
                }
            })
            .collect();
        items.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.file.cmp(&b.file)));

        let mut lint = Vec::new();
        let mut by_name: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for item in &items {
            if let Some(name) = &item.name {
                by_name.entry(normalized_name(name)).or_default().push(&item.file);
            }
        }
        for files in by_name.values_mut().filter(|f| f.len() > 1) {
            files.sort();
            for file in files.iter() {
                let others: Vec<&str> = files.iter().filter(|f| *f != file).copied().collect();
                lint.push(MemoryLint {
                    kind: "duplicate".into(),
                    file: Some((*file).to_owned()),
                    detail: format!("same name as {}", others.join(", ")),
                });
            }
        }
        let present: HashSet<&str> = items.iter().map(|i| i.file.as_str()).collect();
        for item in &items {
            if !item.in_index {
                lint.push(MemoryLint {
                    kind: "not_in_index".into(),
                    file: Some(item.file.clone()),
                    detail: format!("{INDEX_FILE} has no line for it"),
                });
            }
            for link in &item.broken_links {
                lint.push(MemoryLint {
                    kind: "broken_link".into(),
                    file: Some(item.file.clone()),
                    detail: format!("[[{link}]] matches no memory"),
                });
            }
            if item.stale {
                lint.push(MemoryLint {
                    kind: "stale".into(),
                    file: Some(item.file.clone()),
                    detail: "project memory not modified for more than 30 days".into(),
                });
            }
        }
        let mut missing: Vec<&String> = indexed.iter().filter(|t| !present.contains(t.as_str())).collect();
        missing.sort();
        for target in missing {
            lint.push(MemoryLint {
                kind: "missing_file".into(),
                file: Some(target.clone()),
                detail: format!("{INDEX_FILE} links a file that does not exist"),
            });
        }
        Ok(MemoryListResponse {
            dir_exists: true,
            index_lines: index.lines().count() as u32,
            items,
            lint,
        })
    }

    pub async fn read(&self, workspace: &Path, file: &str) -> Result<MemoryFileResponse, MemoryError> {
        let dir = self.existing_dir(workspace).await?;
        let (path, meta) = Self::file_path(&dir, file).await?;
        let text = tokio::fs::read_to_string(&path).await?;
        let (frontmatter_raw, body) = split_frontmatter(&text);
        Ok(MemoryFileResponse {
            file: file.to_owned(),
            frontmatter_raw,
            body,
            modified: mtime_ms(&meta),
        })
    }

    /// Replace a file's content when its mtime is still `expected_modified`.
    pub async fn update(
        &self,
        workspace: &Path,
        file: &str,
        frontmatter_raw: &str,
        body: &str,
        expected_modified: i64,
    ) -> Result<MemoryFileResponse, MemoryError> {
        let dir = self.existing_dir(workspace).await?;
        let (path, meta) = Self::file_path(&dir, file).await?;
        let current = mtime_ms(&meta);
        if current != expected_modified {
            return Err(MemoryError::Changed {
                current_modified: current,
            });
        }
        let text = compose(frontmatter_raw, body);
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(MemoryError::TooLarge);
        }
        Self::trash_copy(&dir, file).await?;
        write_atomic(&dir, &path, &text).await?;
        tracing::info!(file, bytes = text.len(), "memory file updated");
        self.read(workspace, file).await
    }

    /// Delete a file (trash copy first) and the index lines linking it.
    pub async fn delete(&self, workspace: &Path, file: &str) -> Result<MemoryDeleteResponse, MemoryError> {
        if file == INDEX_FILE {
            return Err(MemoryError::IndexNotDeletable);
        }
        let dir = self.existing_dir(workspace).await?;
        let (path, _) = Self::file_path(&dir, file).await?;
        let trash_file = Self::trash_copy(&dir, file).await?;
        tokio::fs::remove_file(&path).await?;
        let mut removed = 0;
        let index_path = dir.join(INDEX_FILE);
        if let Ok(index) = tokio::fs::read_to_string(&index_path).await {
            let kept: Vec<&str> = index
                .lines()
                .filter(|line| !index_targets(line).iter().any(|t| t == file))
                .collect();
            removed = index.lines().count() - kept.len();
            if removed > 0 {
                Self::trash_copy(&dir, INDEX_FILE).await?;
                let mut text = kept.join("\n");
                if index.ends_with('\n') {
                    text.push('\n');
                }
                write_atomic(&dir, &index_path, &text).await?;
            }
        }
        tracing::info!(
            file,
            index_lines_removed = removed,
            "memory file deleted (trash copy kept)"
        );
        Ok(MemoryDeleteResponse {
            file: file.to_owned(),
            trash_file: format!("{TRASH_DIR}/{trash_file}"),
            index_lines_removed: removed as u32,
        })
    }

    /// Bring back the newest trash copy of `file` (the current file, if any,
    /// goes to the trash first) and re-add its index lines when they are gone.
    pub async fn restore(&self, workspace: &Path, file: &str) -> Result<MemoryFileResponse, MemoryError> {
        if !valid_file_name(file) {
            return Err(MemoryError::InvalidName);
        }
        let dir = self.existing_dir(workspace).await?;
        let copies = trash_copies(&dir, file).await;
        let Some((_, newest)) = copies.last() else {
            return Err(MemoryError::NothingToRestore);
        };
        let text = tokio::fs::read_to_string(dir.join(TRASH_DIR).join(newest)).await?;
        let path = dir.join(file);
        if let Ok(meta) = tokio::fs::symlink_metadata(&path).await {
            if !meta.is_file() {
                return Err(MemoryError::InvalidName);
            }
            Self::trash_copy(&dir, file).await?;
        }
        write_atomic(&dir, &path, &text).await?;
        if file != INDEX_FILE {
            self.restore_index_lines(&dir, file).await?;
        }
        tracing::info!(file, "memory file restored from trash");
        self.read(workspace, file).await
    }

    /// Lines of the newest trashed index that linked `file`, appended when
    /// the current index has none.
    async fn restore_index_lines(&self, dir: &Path, file: &str) -> Result<(), MemoryError> {
        let index_path = dir.join(INDEX_FILE);
        let index = tokio::fs::read_to_string(&index_path).await.unwrap_or_default();
        if index.lines().any(|l| index_targets(l).iter().any(|t| t == file)) {
            return Ok(());
        }
        for (_, copy) in trash_copies(dir, INDEX_FILE).await.iter().rev() {
            let old = tokio::fs::read_to_string(dir.join(TRASH_DIR).join(copy))
                .await
                .unwrap_or_default();
            let lines: Vec<&str> = old
                .lines()
                .filter(|l| index_targets(l).iter().any(|t| t == file))
                .collect();
            if lines.is_empty() {
                continue;
            }
            let mut text = index.clone();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            for line in lines {
                text.push_str(line);
                text.push('\n');
            }
            write_atomic(dir, &index_path, &text).await?;
            break;
        }
        Ok(())
    }

    /// Copy `file` to `.trash/<file>.<unix ms>`; prunes copies older than 30 days.
    async fn trash_copy(dir: &Path, file: &str) -> Result<String, MemoryError> {
        let trash = dir.join(TRASH_DIR);
        tokio::fs::create_dir_all(&trash).await?;
        let mut stamp = now_ms();
        let mut name = format!("{file}.{stamp}");
        while tokio::fs::symlink_metadata(trash.join(&name)).await.is_ok() {
            stamp += 1;
            name = format!("{file}.{stamp}");
        }
        tokio::fs::copy(dir.join(file), trash.join(&name)).await?;
        prune_trash(&trash).await;
        Ok(name)
    }
}

fn compose(frontmatter_raw: &str, body: &str) -> String {
    let front = frontmatter_raw.trim_end_matches(['\n', '\r']);
    let mut text = String::new();
    if !front.trim().is_empty() {
        text.push_str("---\n");
        text.push_str(front);
        text.push_str("\n---\n\n");
    }
    text.push_str(body);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Write through a temp file in the same dir, then rename over the target.
async fn write_atomic(dir: &Path, path: &Path, text: &str) -> Result<(), MemoryError> {
    let tmp = dir.join(format!(".memory-write-{}-{}", std::process::id(), now_ms()));
    tokio::fs::write(&tmp, text).await?;
    if let Err(err) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err.into());
    }
    Ok(())
}

/// Trash copies of `file`, oldest first, as `(unix ms, copy name)`.
async fn trash_copies(dir: &Path, file: &str) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    let prefix = format!("{file}.");
    let Ok(mut entries) = tokio::fs::read_dir(dir.join(TRASH_DIR)).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(stamp) = name.strip_prefix(&prefix).and_then(|s| s.parse::<i64>().ok()) {
            out.push((stamp, name));
        }
    }
    out.sort();
    out
}

async fn prune_trash(trash: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(trash).await else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(TRASH_KEEP)
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stamp = name.rsplit_once('.').and_then(|(_, s)| s.parse::<i64>().ok());
        if stamp.is_some_and(|s| s < cutoff) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

#[cfg(test)]
#[path = "memory_test.rs"]
mod memory_test;
