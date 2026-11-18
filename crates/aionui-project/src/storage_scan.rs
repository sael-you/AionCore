//! Disk usage of one directory tree for the project page (card C42 §3).
//!
//! A bounded, synchronous walk meant to run in a blocking task:
//! - never follows a symlink (`symlink_metadata` only; a symlink counts as
//!   its own size and is never descended into), so the walk cannot leave the
//!   root it was given;
//! - never crosses onto another file system is NOT assumed: mount points
//!   inside a workspace are walked like directories;
//! - stops after `max_entries` entries or `max_time` and reports
//!   `truncated`, the sizes then being lower bounds;
//! - sizes are apparent file sizes (`len`), not allocated blocks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Entries after which a root's walk stops.
pub const MAX_ENTRIES: u64 = 200_000;
/// Time after which a root's walk stops.
pub const MAX_TIME: Duration = Duration::from_secs(5);
/// Top-level children reported per root.
pub const MAX_CHILDREN: usize = 20;
/// Largest files reported per root.
pub const MAX_LARGEST: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedChild {
    pub name: String,
    pub bytes: u64,
    /// `dir` | `file` | `symlink` | `other`.
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScannedRoot {
    pub exists: bool,
    pub bytes: u64,
    pub entries: u64,
    pub truncated: bool,
    /// Largest first, at most [`MAX_CHILDREN`].
    pub children: Vec<ScannedChild>,
    /// (path relative to the root with `/`, bytes), largest first, at most [`MAX_LARGEST`].
    pub largest_files: Vec<(String, u64)>,
}

struct Walk {
    root: PathBuf,
    entries: u64,
    max_entries: u64,
    deadline: Instant,
    truncated: bool,
    largest: Vec<(String, u64)>,
}

impl Walk {
    fn over_budget(&mut self) -> bool {
        // The clock is read every 256 entries: cheap, still bounded.
        if self.entries >= self.max_entries || (self.entries.is_multiple_of(256) && Instant::now() >= self.deadline) {
            self.truncated = true;
        }
        self.truncated
    }

    fn note_file(&mut self, path: &Path, bytes: u64) {
        if self.largest.len() == MAX_LARGEST && self.largest.last().is_some_and(|(_, b)| *b >= bytes) {
            return;
        }
        let rel = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let at = self.largest.partition_point(|(_, b)| *b >= bytes);
        self.largest.insert(at, (rel, bytes));
        self.largest.truncate(MAX_LARGEST);
    }

    /// Total bytes below `dir` (iterative, so depth never grows the stack).
    fn dir_bytes(&mut self, dir: &Path) -> u64 {
        let mut total = 0u64;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in read.flatten() {
                if self.over_budget() {
                    return total;
                }
                self.entries += 1;
                let path = entry.path();
                // `DirEntry::metadata` does not follow symlinks.
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let file_type = meta.file_type();
                if file_type.is_dir() {
                    stack.push(path);
                } else {
                    total = total.saturating_add(meta.len());
                    if file_type.is_file() {
                        self.note_file(&path, meta.len());
                    }
                }
            }
        }
        total
    }
}

/// Scan `root` with the default budget.
pub fn scan_root(root: &Path) -> ScannedRoot {
    scan_root_with(root, MAX_ENTRIES, MAX_TIME)
}

/// Scan `root`; a missing path, a symlink or a non-directory yields
/// `exists: false` (a symlinked root is never walked).
pub fn scan_root_with(root: &Path, max_entries: u64, max_time: Duration) -> ScannedRoot {
    let is_real_dir = std::fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_dir());
    if !is_real_dir {
        return ScannedRoot::default();
    }
    let mut walk = Walk {
        root: root.to_path_buf(),
        entries: 0,
        max_entries,
        deadline: Instant::now() + max_time,
        truncated: false,
        largest: Vec::new(),
    };
    let mut children: Vec<ScannedChild> = Vec::new();
    let mut bytes = 0u64;
    if let Ok(read) = std::fs::read_dir(root) {
        let mut top: Vec<std::fs::DirEntry> = read.flatten().collect();
        top.sort_by_key(std::fs::DirEntry::file_name);
        for entry in top {
            if walk.over_budget() {
                break;
            }
            walk.entries += 1;
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let file_type = meta.file_type();
            let path = entry.path();
            let (kind, size) = if file_type.is_dir() {
                ("dir", walk.dir_bytes(&path))
            } else if file_type.is_file() {
                walk.note_file(&path, meta.len());
                ("file", meta.len())
            } else if file_type.is_symlink() {
                ("symlink", meta.len())
            } else {
                ("other", meta.len())
            };
            bytes = bytes.saturating_add(size);
            children.push(ScannedChild {
                name: entry.file_name().to_string_lossy().into_owned(),
                bytes: size,
                kind,
            });
        }
    }
    children.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    children.truncate(MAX_CHILDREN);
    ScannedRoot {
        exists: true,
        bytes,
        entries: walk.entries,
        truncated: walk.truncated,
        children,
        largest_files: walk.largest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn sizes_children_and_largest_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        write(&root.join("src/a.rs"), 1000);
        write(&root.join("src/deep/b.rs"), 3000);
        write(&root.join("node_modules/big.bin"), 50_000);
        write(&root.join("README.md"), 200);
        std::fs::create_dir_all(root.join("empty")).unwrap();

        let scan = scan_root(&root);
        assert!(scan.exists && !scan.truncated);
        assert_eq!(scan.bytes, 54_200);
        assert_eq!(scan.entries, 8, "4 top-level, then a.rs, deep, b.rs, big.bin");
        let children: Vec<(&str, u64, &str)> = scan
            .children
            .iter()
            .map(|c| (c.name.as_str(), c.bytes, c.kind))
            .collect();
        assert_eq!(
            children,
            vec![
                ("node_modules", 50_000, "dir"),
                ("src", 4000, "dir"),
                ("README.md", 200, "file"),
                ("empty", 0, "dir")
            ]
        );
        assert_eq!(
            scan.largest_files,
            vec![
                ("node_modules/big.bin".to_owned(), 50_000),
                ("src/deep/b.rs".to_owned(), 3000),
                ("src/a.rs".to_owned(), 1000),
                ("README.md".to_owned(), 200)
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed_out_of_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        write(&outside.join("huge.bin"), 100_000);
        let root = tmp.path().join("ws");
        write(&root.join("a.txt"), 10);
        std::os::unix::fs::symlink(&outside, root.join("link-dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("huge.bin"), root.join("link-file")).unwrap();

        let scan = scan_root(&root);
        assert!(
            scan.bytes < 1000,
            "the 100 kB behind the links are not counted: {}",
            scan.bytes
        );
        assert_eq!(scan.largest_files, vec![("a.txt".to_owned(), 10)]);
        let kinds: Vec<(&str, &str)> = scan.children.iter().map(|c| (c.name.as_str(), c.kind)).collect();
        assert!(kinds.contains(&("link-dir", "symlink")) && kinds.contains(&("link-file", "symlink")));

        // A root that is itself a symlink is not walked at all.
        let link_root = tmp.path().join("ws-link");
        std::os::unix::fs::symlink(&outside, &link_root).unwrap();
        assert_eq!(scan_root(&link_root), ScannedRoot::default());
    }

    #[test]
    fn walk_is_bounded_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        for i in 0..50 {
            write(&root.join(format!("d/f{i}")), 10);
        }
        let scan = scan_root_with(&root, 20, MAX_TIME);
        assert!(scan.truncated);
        assert!(scan.entries <= 20);
        assert!(scan.bytes < 500, "a lower bound");
        let timed_out = scan_root_with(&root, MAX_ENTRIES, Duration::ZERO);
        assert!(timed_out.truncated);

        assert_eq!(scan_root(&tmp.path().join("missing")), ScannedRoot::default());
    }
}
