//! The private file claude's `--mcp-config` points at.
//!
//! The MCP config carries literal `env` / `headers` values (API keys, the team
//! coordination token). Passed inline, they sit in the process argv, which any
//! local user can read through `ps` / `/proc/<pid>/cmdline`. claude also accepts
//! a FILE path for the flag ("Load MCP servers from JSON files or strings",
//! verified: `claude --help` 2.1.325, and a live `mcp_status` probe against
//! 2.1.325 that loaded the servers from a file path), so the JSON is written to
//! a 0600 file instead and only the path rides argv.
//!
//! The file lives in the directory the orchestration layer hands over as
//! [`super::SessionConfig::runtime_dir`] (AionUi's per-conversation skill view
//! directory). Lifetime: one file per opened backend, removed when the guard is
//! dropped — the guard rides the claude wake recipe, so the file stays readable
//! for every `--resume` respawn and goes away with the backend. A hard crash can
//! skip `Drop`; the directory's own lifecycle (removed on conversation delete,
//! orphan views reaped at startup) covers that leftover.

use std::io::Write;
use std::path::{Path, PathBuf};

/// File-name prefix inside the runtime dir. Unique per backend (a uuid suffix),
/// so a rebuilt session never shares — or deletes — the file of the backend it
/// replaces.
const FILE_PREFIX: &str = "claude-mcp-config-";

/// Owns one written `--mcp-config` file and removes it on drop.
#[derive(Debug)]
pub(crate) struct McpConfigFile {
    path: PathBuf,
}

impl McpConfigFile {
    /// Write `json` to a NEW owner-only (0600 on unix) file under `dir`,
    /// creating `dir` when missing. `create_new` refuses to follow or reuse an
    /// existing path, so a planted file or symlink cannot capture the secrets.
    pub(crate) fn write(dir: &Path, json: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{FILE_PREFIX}{}.json", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        // From here on the guard owns the path, so a failed write still removes
        // the partial file.
        let guard = Self { path };
        file.write_all(json.as_bytes())?;
        file.flush()?;
        Ok(guard)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for McpConfigFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Path only: the contents are secret.
            Err(e) => tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "claude: failed to remove the private MCP config file"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_owner_only_file_and_removes_it_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("view");
        let file = McpConfigFile::write(&nested, r#"{"mcpServers":{}}"#).expect("write");
        let path = file.path().to_path_buf();
        assert!(path.starts_with(&nested), "the file lives in the runtime dir");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"mcpServers":{}}"#);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secrets file must be owner-only, got {mode:o}");
        }
        drop(file);
        assert!(!path.exists(), "dropping the guard removes the file");
    }

    #[test]
    fn two_files_in_one_dir_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let a = McpConfigFile::write(dir.path(), "{}").unwrap();
        let b = McpConfigFile::write(dir.path(), "{}").unwrap();
        assert_ne!(a.path(), b.path());
        let b_path = b.path().to_path_buf();
        drop(a);
        assert!(b_path.exists(), "dropping one backend's file leaves the other's");
    }
}
