//! Extra runtimes for market templates that are not Node (GAPS G4): `uv` /
//! `uvx` for the Python servers. The launcher looks for a pinned command in
//! `<data_dir>/toolchain/bin/` before `PATH`, so a host can provide a
//! verified binary without a new environment variable. The child gets that
//! directory first on its `PATH` (uvx starts `uv`) and uv's cache, tool and
//! Python dirs under `<data_dir>/toolchain/uv/`, which lives with the data
//! dir (persisted across container rebuilds).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Directory name under the data dir.
pub const TOOLCHAIN_DIR: &str = "toolchain";

/// A command found in the toolchain directory, with the env it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainCommand {
    pub program: PathBuf,
    pub env: Vec<(String, String)>,
}

/// `<data_dir>/toolchain/bin`.
pub fn toolchain_bin(data_dir: &Path) -> PathBuf {
    data_dir.join(TOOLCHAIN_DIR).join("bin")
}

/// Resolve a bare template command (`uvx`) in the toolchain directory. `None`
/// for paths, Node tools (the runtime resolver owns those) and commands the
/// toolchain does not hold (they go through the normal `PATH` lookup).
pub fn resolve_toolchain_command(
    command: &str,
    data_dir: &Path,
    parent_path: Option<&OsStr>,
) -> Option<ToolchainCommand> {
    let command = command.trim();
    if command.is_empty()
        || command.contains('/')
        || command.contains('\\')
        || matches!(command, "node" | "npm" | "npx")
    {
        return None;
    }
    let bin = toolchain_bin(data_dir);
    let program = bin.join(command);
    if !program.is_file() {
        return None;
    }
    let mut dirs = vec![bin.clone()];
    if let Some(path) = parent_path {
        dirs.extend(std::env::split_paths(path).filter(|p| p != &bin));
    }
    let path = std::env::join_paths(dirs).ok()?.to_string_lossy().into_owned();
    let mut env = vec![("PATH".to_owned(), path)];
    if matches!(command, "uv" | "uvx") {
        let uv = data_dir.join(TOOLCHAIN_DIR).join("uv");
        for (key, sub) in [
            ("UV_CACHE_DIR", "cache"),
            ("UV_TOOL_DIR", "tools"),
            ("UV_PYTHON_INSTALL_DIR", "python"),
        ] {
            env.push((key.to_owned(), uv.join(sub).to_string_lossy().into_owned()));
        }
    }
    Some(ToolchainCommand { program, env })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_uvx_in_the_toolchain_and_puts_it_first_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = toolchain_bin(dir.path());
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("uvx"), b"#!/bin/sh\n").unwrap();
        let found = resolve_toolchain_command("uvx", dir.path(), Some(OsStr::new("/usr/bin:/bin"))).unwrap();
        assert_eq!(found.program, bin.join("uvx"));
        let path = &found.env.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert_eq!(path, &format!("{}:/usr/bin:/bin", bin.display()));
        let cache = &found.env.iter().find(|(k, _)| k == "UV_CACHE_DIR").unwrap().1;
        assert!(cache.starts_with(&dir.path().to_string_lossy().into_owned()));

        assert_eq!(resolve_toolchain_command("npx", dir.path(), None), None);
        assert_eq!(resolve_toolchain_command("/usr/bin/uvx", dir.path(), None), None);
        assert_eq!(resolve_toolchain_command("pipx", dir.path(), None), None);
    }
}
