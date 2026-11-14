//! `aioncore board recover --from <db> [--dry-run]` orchestration (SPEC §12).
//!
//! Runs with AionUi stopped: it refuses while the data dir's instance guard
//! is held or (Linux) while any other process holds the database (or its
//! `-wal`/`-shm`) open, then opens the database, runs
//! [`aionui_db::recover_board`] and — after a successful non-dry run only —
//! atomically writes the recover marker (kept backup path + SHA-256). The
//! push script's preflight compares the marker's mtime with the newest
//! retained `backend-replaced-*` backup.
//!
//! Marker path: `$AIONUI_BOARD_RECOVER_MARKER` when set, else
//! `<data-dir>/board-last-recover`. Recover never broadcasts (the server is
//! down) and writes only the events described in `recover_board`.

use std::path::{Path, PathBuf};

use aionui_db::{BoardRecoverReport, DataDirInstanceGuard, init_database, recover_board};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::info;

/// Environment variable overriding the recover marker path.
pub const RECOVER_MARKER_ENV: &str = "AIONUI_BOARD_RECOVER_MARKER";
/// Default marker file name inside the data dir.
pub const DEFAULT_MARKER_FILE: &str = "board-last-recover";

/// The marker path: `$AIONUI_BOARD_RECOVER_MARKER`, else
/// `<data-dir>/board-last-recover`.
pub fn default_marker_path(data_dir: &Path) -> PathBuf {
    std::env::var_os(RECOVER_MARKER_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join(DEFAULT_MARKER_FILE))
}

#[derive(Debug, Clone)]
pub struct RecoverOptions {
    /// The current database (`<data-dir>/aionui-backend.db`).
    pub database: PathBuf,
    /// The kept database (e.g. a `backend-replaced-<ts>.db` backup).
    pub kept: PathBuf,
    pub dry_run: bool,
    pub marker: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecoverOutcome {
    pub kept: String,
    pub kept_sha256: String,
    /// Set when the marker was written (successful non-dry run).
    pub marker: Option<String>,
    pub report: BoardRecoverReport,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    #[error("kept database not found: {0}")]
    KeptMissing(String),
    #[error("the kept database is the current database")]
    SameFile,
    #[error("the database is in use (stop AionUi first); holders: {0:?}")]
    InUse(Vec<u32>),
    #[error("database error: {0}")]
    Database(String),
    #[error("could not write the recover marker: {0}")]
    Marker(String),
}

impl RecoverError {
    /// Stable machine code for the CLI envelope.
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeptMissing(_) => "recover_kept_missing",
            Self::SameFile => "recover_same_file",
            Self::InUse(_) => "recover_database_in_use",
            Self::Database(_) => "recover_database_failed",
            Self::Marker(_) => "recover_marker_failed",
        }
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Write the marker atomically (temp file in the same directory + rename).
pub fn write_marker(marker: &Path, kept: &Path, sha256: &str) -> std::io::Result<()> {
    if let Some(parent) = marker.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({
        "recovered_from": kept.to_string_lossy(),
        "sha256": sha256,
        "recovered_at_ms": aionui_common::now_ms(),
    });
    let temp = marker.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, format!("{body}\n"))?;
    std::fs::rename(&temp, marker).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Pids (other than this process) holding `database` or its WAL/SHM open.
#[cfg(target_os = "linux")]
pub fn database_holders(database: &Path) -> Vec<u32> {
    let Ok(canonical) = std::fs::canonicalize(database) else {
        return Vec::new();
    };
    let targets: Vec<PathBuf> = ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| PathBuf::from(format!("{}{suffix}", canonical.display())))
        .collect();
    let own = std::process::id();
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut holders = Vec::new();
    for entry in processes.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        if pid == own {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        if fds
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .any(|link| targets.contains(&link))
        {
            holders.push(pid);
        }
    }
    holders
}

/// Pids holding the database open (not determinable on this platform).
#[cfg(not(target_os = "linux"))]
pub fn database_holders(_database: &Path) -> Vec<u32> {
    Vec::new()
}

/// Run a recovery end to end. See the module docs.
pub async fn recover_database(options: &RecoverOptions) -> Result<RecoverOutcome, RecoverError> {
    if !options.kept.is_file() {
        return Err(RecoverError::KeptMissing(options.kept.display().to_string()));
    }
    if let (Ok(kept), Ok(current)) = (
        std::fs::canonicalize(&options.kept),
        std::fs::canonicalize(&options.database),
    ) && kept == current
    {
        return Err(RecoverError::SameFile);
    }
    // The running server holds the data-dir instance guard for its lifetime.
    let _guard = match DataDirInstanceGuard::try_acquire(&options.database) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => return Err(RecoverError::InUse(Vec::new())),
        // flock unavailable: fall back to the open-handle check below.
        Err(_) => None,
    };
    let holders = database_holders(&options.database);
    if !holders.is_empty() {
        return Err(RecoverError::InUse(holders));
    }
    let kept_sha256 = sha256_file(&options.kept).map_err(|error| RecoverError::KeptMissing(error.to_string()))?;
    let database = init_database(&options.database)
        .await
        .map_err(|error| RecoverError::Database(error.to_string()))?;
    let report = recover_board(database.pool(), &options.kept, options.dry_run).await;
    database.close().await;
    let report = report.map_err(|error| RecoverError::Database(error.to_string()))?;
    let marker = if options.dry_run {
        None
    } else {
        write_marker(&options.marker, &options.kept, &kept_sha256)
            .map_err(|error| RecoverError::Marker(error.to_string()))?;
        Some(options.marker.display().to_string())
    };
    info!(
        dry_run = options.dry_run,
        events_inserted = report.events_inserted,
        cards_inserted = report.cards_inserted,
        conflicts = report.conflicts.len(),
        "board recover finished"
    );
    Ok(RecoverOutcome {
        kept: options.kept.display().to_string(),
        kept_sha256,
        marker,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_written_atomically_with_path_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("kept.db");
        std::fs::write(&kept, b"abc").unwrap();
        let sha = sha256_file(&kept).unwrap();
        assert_eq!(sha, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let marker = dir.path().join("nested/last-recover");
        write_marker(&marker, &kept, &sha).unwrap();
        let body: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        assert_eq!(body["sha256"], sha);
        assert_eq!(body["recovered_from"], kept.to_string_lossy().as_ref());
        let leftovers: Vec<_> = std::fs::read_dir(marker.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn the_default_marker_lives_in_the_data_dir() {
        // The env override is read at call time; this test does not set it.
        if std::env::var_os(RECOVER_MARKER_ENV).is_none() {
            assert_eq!(
                default_marker_path(Path::new("/data")),
                PathBuf::from("/data/board-last-recover")
            );
        }
    }
}
