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
//! Where it lives, in order of preference:
//! 1. [`super::SessionConfig::runtime_dir`] — AionUi's per-conversation skill view
//!    directory ([`McpConfigFile::write_in`]);
//! 2. a FRESH 0700 directory under [`super::SessionConfig::runtime_scratch_root`]
//!    (`{data_dir}/session-runtime`) when the first is missing or unwritable
//!    ([`McpConfigFile::write_scratch`]).
//!
//! Lifetime: one file per opened backend, removed when the guard is dropped — the
//! guard rides the claude wake recipe, so the file stays readable for every
//! `--resume` respawn (and is re-written by [`McpConfigFile::ensure_present`] if
//! something deleted it) and goes away with the backend. A hard exit (SIGKILL,
//! OOM, watchdog) skips `Drop`, so the app sweeps both locations at startup,
//! before any backend exists: [`is_mcp_config_file_name`] selects the files in
//! the view directories, [`sweep_scratch_root`] clears the scratch root.

use std::io::Write;
use std::path::{Path, PathBuf};

/// File-name prefix of every `--mcp-config` file. Unique per backend (a uuid
/// suffix), so a rebuilt session never shares — or deletes — the file of the
/// backend it replaces.
pub const MCP_CONFIG_FILE_PREFIX: &str = "claude-mcp-config-";
const MCP_CONFIG_FILE_SUFFIX: &str = ".json";
/// Directory name of the scratch root under the data dir.
pub const SESSION_SCRATCH_DIR_NAME: &str = "session-runtime";
/// Prefix of the per-backend directories created under the scratch root.
const SCRATCH_SUBDIR_PREFIX: &str = "claude-mcp-";

/// Whether `name` is a file this module writes (for the startup sweep of the
/// view directories).
pub fn is_mcp_config_file_name(name: &str) -> bool {
    name.starts_with(MCP_CONFIG_FILE_PREFIX) && name.ends_with(MCP_CONFIG_FILE_SUFFIX)
}

/// The scratch root for `data_dir` (`{data_dir}/session-runtime`).
pub fn session_scratch_root(data_dir: &Path) -> PathBuf {
    data_dir.join(SESSION_SCRATCH_DIR_NAME)
}

/// Remove every per-backend directory this module left under `root`. Only safe
/// while no backend runs (app startup, data-dir instance lock held): a live
/// backend's file would be deleted too. Returns how many entries were removed; a
/// missing root removes nothing.
pub fn sweep_scratch_root(root: &Path) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let is_ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(SCRATCH_SUBDIR_PREFIX));
        if is_ours && entry.file_type().is_ok_and(|t| t.is_dir()) && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// `create_dir_all` with owner-only (0700 on unix) permissions for every
/// directory it creates. Existing directories keep their mode.
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Write `json` to a NEW owner-only (0600 on unix) file. `create_new` refuses to
/// follow or reuse an existing path, so a planted file or symlink cannot capture
/// the secrets. A failed write removes the partial file.
fn write_new_private_file(path: &Path, json: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if let Err(e) = file.write_all(json.as_bytes()).and_then(|()| file.flush()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

fn file_name() -> String {
    format!(
        "{MCP_CONFIG_FILE_PREFIX}{}{MCP_CONFIG_FILE_SUFFIX}",
        uuid::Uuid::new_v4()
    )
}

/// Owns one written `--mcp-config` file and removes it on drop.
pub(crate) struct McpConfigFile {
    path: PathBuf,
    /// The fresh scratch directory created for this file, removed with it.
    /// `None` when the file sits in the shared runtime dir.
    owned_dir: Option<PathBuf>,
    /// The body, kept so a deleted file can be re-written at the SAME path
    /// before a respawn (the path is baked into the wake recipe's argv).
    json: String,
}

impl std::fmt::Debug for McpConfigFile {
    // Hand-written so the secret body never reaches a `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConfigFile")
            .field("path", &self.path)
            .field("owned_dir", &self.owned_dir)
            .finish_non_exhaustive()
    }
}

impl McpConfigFile {
    /// Write `json` into `dir` (the runtime dir), creating it 0700 when missing.
    pub(crate) fn write_in(dir: &Path, json: &str) -> std::io::Result<Self> {
        create_private_dir_all(dir)?;
        let path = dir.join(file_name());
        write_new_private_file(&path, json)?;
        Ok(Self {
            path,
            owned_dir: None,
            json: json.to_owned(),
        })
    }

    /// Write `json` into a FRESH 0700 directory under `root` (the scratch root).
    pub(crate) fn write_scratch(root: &Path, json: &str) -> std::io::Result<Self> {
        create_private_dir_all(root)?;
        let dir = root.join(format!("{SCRATCH_SUBDIR_PREFIX}{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        // Not recursive: the directory must be new, never a pre-existing one.
        builder.create(&dir)?;
        let path = dir.join(file_name());
        if let Err(e) = write_new_private_file(&path, json) {
            let _ = std::fs::remove_dir(&dir);
            return Err(e);
        }
        Ok(Self {
            path,
            owned_dir: Some(dir),
            json: json.to_owned(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Re-create the file at the SAME path if it is gone (e.g. a directory
    /// cleanup ran while the process was suspended). `Ok(true)` = re-written.
    pub(crate) fn ensure_present(&self) -> std::io::Result<bool> {
        if self.path.try_exists()? {
            return Ok(false);
        }
        if let Some(parent) = self.path.parent() {
            create_private_dir_all(parent)?;
        }
        write_new_private_file(&self.path, &self.json)?;
        Ok(true)
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
        if let Some(dir) = &self.owned_dir {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn writes_owner_only_file_and_removes_it_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("view");
        let file = McpConfigFile::write_in(&nested, r#"{"mcpServers":{}}"#).expect("write");
        let path = file.path().to_path_buf();
        assert!(path.starts_with(&nested), "the file lives in the runtime dir");
        assert!(is_mcp_config_file_name(path.file_name().unwrap().to_str().unwrap()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"mcpServers":{}}"#);
        #[cfg(unix)]
        {
            assert_eq!(mode(&path), 0o600, "secrets file must be owner-only");
            assert_eq!(mode(&nested), 0o700, "a created runtime dir is owner-only");
        }
        assert!(
            !format!("{file:?}").contains("mcpServers"),
            "Debug must not print the body"
        );
        drop(file);
        assert!(!path.exists(), "dropping the guard removes the file");
    }

    #[test]
    fn two_files_in_one_dir_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let a = McpConfigFile::write_in(dir.path(), "{}").unwrap();
        let b = McpConfigFile::write_in(dir.path(), "{}").unwrap();
        assert_ne!(a.path(), b.path());
        let b_path = b.path().to_path_buf();
        drop(a);
        assert!(b_path.exists(), "dropping one backend's file leaves the other's");
    }

    #[test]
    fn scratch_file_gets_a_fresh_private_dir_removed_with_it() {
        let data = tempfile::tempdir().unwrap();
        let root = session_scratch_root(data.path());
        let file = McpConfigFile::write_scratch(&root, "{}").unwrap();
        let dir = file.path().parent().unwrap().to_path_buf();
        assert_eq!(dir.parent(), Some(root.as_path()));
        #[cfg(unix)]
        {
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(file.path()), 0o600);
        }
        drop(file);
        assert!(!dir.exists(), "the per-backend scratch dir goes with the file");
        assert!(root.exists(), "the shared root stays");
    }

    #[test]
    fn ensure_present_rewrites_a_deleted_file_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = McpConfigFile::write_in(dir.path(), "body").unwrap();
        assert!(!file.ensure_present().unwrap(), "present → untouched");
        std::fs::remove_file(file.path()).unwrap();
        assert!(file.ensure_present().unwrap(), "missing → re-written");
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "body");
        #[cfg(unix)]
        assert_eq!(mode(file.path()), 0o600);
    }

    #[test]
    fn sweep_scratch_root_removes_only_our_leftovers() {
        let data = tempfile::tempdir().unwrap();
        let root = session_scratch_root(data.path());
        assert_eq!(sweep_scratch_root(&root).unwrap(), 0, "missing root → nothing");
        let leaked = McpConfigFile::write_scratch(&root, "{}").unwrap();
        let leaked_dir = leaked.path().parent().unwrap().to_path_buf();
        // Simulate a hard exit: the guard never runs.
        std::mem::forget(leaked);
        std::fs::create_dir(root.join("unrelated")).unwrap();
        assert_eq!(sweep_scratch_root(&root).unwrap(), 1);
        assert!(!leaked_dir.exists());
        assert!(root.join("unrelated").exists(), "foreign entries are left alone");
    }
}
