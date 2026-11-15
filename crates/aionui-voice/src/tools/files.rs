//! Project file access for the read tools: `read_file` and a bounded content
//! search (§3 ProjectFilesPort). Lexical containment reuses
//! `aionui_project::{canonical, containment}`; the realpath check (symlinks
//! leaving the project root are refused) runs right before IO, as that
//! module's contract requires.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aionui_project::types::FileOp;
use aionui_project::{canonical, containment};
use ignore::WalkBuilder;

use super::policy::{
    MAX_FILE_BYTES, MAX_READ_LINES, MAX_SEARCH_MATCHES, dir_name_allowed, file_name_allowed, looks_binary, path_allowed,
};

/// The search stops after this long and returns what it found.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileToolError {
    #[error("the project has no workspace folder")]
    NoRoot,
    #[error("path is outside the project")]
    OutsideProject,
    #[error("this file is not readable by voice tools")]
    Denied,
    #[error("file not found")]
    NotFound,
    #[error("file is too large or binary")]
    Unreadable,
}

/// Resolve a project root (canonical `file:` URI or absolute path) to its realpath.
pub fn project_root(root: &str) -> Result<PathBuf, FileToolError> {
    let uri = if root.starts_with("file:") {
        root.to_owned()
    } else {
        canonical::to_file_uri(Path::new(root)).map_err(|_| FileToolError::NoRoot)?
    };
    let canonical = canonical::canonicalize(&uri).map_err(|_| FileToolError::NoRoot)?;
    let path = canonical::fs_path(&canonical).map_err(|_| FileToolError::NoRoot)?;
    std::fs::canonicalize(path).map_err(|_| FileToolError::NoRoot)
}

/// Lexical containment + denylist + realpath check for `relative` under `root`.
pub fn resolve_in_project(root: &Path, relative: &str) -> Result<PathBuf, FileToolError> {
    let root_uri = canonical::to_file_uri(root).map_err(|_| FileToolError::NoRoot)?;
    let root_canonical = canonical::canonicalize(&root_uri).map_err(|_| FileToolError::NoRoot)?;
    let resolved = containment::resolve_relative(&root_canonical, relative.trim_start_matches("./"), FileOp::Read)
        .map_err(|_| FileToolError::OutsideProject)?;
    if !path_allowed(Path::new(&resolved.relative_path)) {
        return Err(FileToolError::Denied);
    }
    let absolute = resolved.absolute_path.ok_or(FileToolError::OutsideProject)?;
    let real = std::fs::canonicalize(&absolute).map_err(|_| FileToolError::NotFound)?;
    if !real.starts_with(root) {
        return Err(FileToolError::OutsideProject);
    }
    // The symlink target itself must pass the denylist too.
    let real_relative = real.strip_prefix(root).map_err(|_| FileToolError::OutsideProject)?;
    if !path_allowed(real_relative) {
        return Err(FileToolError::Denied);
    }
    Ok(real)
}

/// Up to [`MAX_READ_LINES`] lines from `start_line` (1-based).
pub fn read_lines(root: &Path, relative: &str, start_line: usize) -> Result<String, FileToolError> {
    let path = resolve_in_project(root, relative)?;
    let meta = std::fs::metadata(&path).map_err(|_| FileToolError::NotFound)?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(FileToolError::Unreadable);
    }
    let bytes = std::fs::read(&path).map_err(|_| FileToolError::NotFound)?;
    if looks_binary(&bytes) {
        return Err(FileToolError::Unreadable);
    }
    let text = String::from_utf8_lossy(&bytes);
    let start = start_line.max(1);
    let lines: Vec<String> = text
        .lines()
        .enumerate()
        .skip(start - 1)
        .take(MAX_READ_LINES)
        .map(|(i, line)| format!("{:>5}: {line}", i + 1))
        .collect();
    Ok(lines.join("\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Case-insensitive literal search over the project's text files: respects
/// `.gitignore`, skips hidden and denied paths, ≤ 50 matches, ≤ 2 s.
pub fn search(root: &Path, query: &str) -> Vec<SearchMatch> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(pattern) = regex::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
    else {
        return Vec::new();
    };
    let deadline = Instant::now() + SEARCH_TIMEOUT;
    let mut matches = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                dir_name_allowed(&name)
            } else {
                file_name_allowed(&name.to_ascii_lowercase())
            }
        })
        .build();
    'files: for entry in walker.flatten() {
        if Instant::now() >= deadline || matches.len() >= MAX_SEARCH_MATCHES {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if !path_allowed(relative) || entry.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if looks_binary(&bytes) {
            continue;
        }
        for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if pattern.is_match(line) {
                matches.push(SearchMatch {
                    path: relative.to_string_lossy().replace('\\', "/"),
                    line: index + 1,
                    text: line.trim().chars().take(200).collect(),
                });
                if matches.len() >= MAX_SEARCH_MATCHES {
                    break 'files;
                }
            }
        }
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    println!(\"kanban build\");\n}\n",
        )
        .unwrap();
        std::fs::write(root.join(".env"), "OPENAI_API_KEY=sk-secret\n").unwrap();
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/token.txt"), "kanban secret\n").unwrap();
        std::fs::write(root.join("blob.bin"), b"kanban\x00\x01").unwrap();
        dir
    }

    #[test]
    fn reads_bounded_lines_inside_the_project() {
        let dir = project();
        let root = project_root(dir.path().to_str().unwrap()).unwrap();
        let text = read_lines(&root, "src/main.rs", 2).unwrap();
        assert!(text.starts_with("    2:"));
        assert!(text.contains("kanban build"));
        let long: String = (0..500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(root.join("src/long.txt"), long).unwrap();
        assert_eq!(
            read_lines(&root, "src/long.txt", 1).unwrap().lines().count(),
            MAX_READ_LINES
        );
    }

    #[test]
    fn traversal_denylist_and_symlink_escape_are_refused() {
        let dir = project();
        let root = project_root(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(
            read_lines(&root, "../etc/passwd", 1),
            Err(FileToolError::OutsideProject)
        );
        assert_eq!(read_lines(&root, "/etc/passwd", 1), Err(FileToolError::OutsideProject));
        assert_eq!(read_lines(&root, ".env", 1), Err(FileToolError::Denied));
        assert_eq!(read_lines(&root, "secrets/token.txt", 1), Err(FileToolError::Denied));
        assert_eq!(read_lines(&root, "blob.bin", 1), Err(FileToolError::Denied));
        assert_eq!(read_lines(&root, "src/missing.rs", 1), Err(FileToolError::NotFound));

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("passwd"), "root:x:0:0").unwrap();
            std::os::unix::fs::symlink(outside.path().join("passwd"), root.join("src/link.txt")).unwrap();
            assert_eq!(read_lines(&root, "src/link.txt", 1), Err(FileToolError::OutsideProject));
            // A symlink inside the project pointing at a denied file.
            std::os::unix::fs::symlink(root.join(".env"), root.join("src/env.txt")).unwrap();
            assert_eq!(read_lines(&root, "src/env.txt", 1), Err(FileToolError::Denied));
        }
    }

    #[test]
    fn search_skips_denied_and_binary_files() {
        let dir = project();
        let root = project_root(dir.path().to_str().unwrap()).unwrap();
        let hits = search(&root, "KANBAN");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].path, "src/main.rs");
        assert_eq!(hits[0].line, 2);
        assert!(search(&root, "  ").is_empty());
    }

    #[test]
    fn search_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..200).map(|i| format!("needle {i}\n")).collect();
        std::fs::write(dir.path().join("many.txt"), body).unwrap();
        let root = project_root(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(search(&root, "needle").len(), MAX_SEARCH_MATCHES);
    }
}
