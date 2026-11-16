//! Where the server browser lives on disk (C30, decision d).
//!
//! ```text
//! /root/.clauterm/aionui-browser/          root:root 0711 (traverse only)
//!   chrome/<version>/chrome, chrome/current → <version>   root:root 0755
//!   home/                                  aionui-browser 0700 (HOME of Chrome)
//!   <user>/<n>/                            aionui-browser 0700 (profile of instance n)
//! <data_dir>/browser/settings/<user>.json  root 0600 (settings, not reachable by the browser user)
//! <data_dir>/browser/staging/              root 0700 (downloads + extraction)
//! ```
//!
//! Deviation from the brief ("aionui-browser owned by aionui-browser 0700"):
//! the top directory stays root-owned, because whoever owns a directory can
//! rename or replace its entries, which would let the browser user swap the
//! Chrome binary root installs. The browser user owns only `home/` and the
//! profile directories.

use std::path::{Path, PathBuf};

use aionui_api_types::BrowserErrorCode;

use crate::error::BrowserError;

/// Default root of the browser files.
pub const DEFAULT_BROWSER_ROOT: &str = "/root/.clauterm/aionui-browser";
/// The unprivileged account Chrome and Xvfb run as.
pub const BROWSER_USER: &str = "aionui-browser";
/// Directory names under the root that are not user ids.
const RESERVED: &[&str] = &["chrome", "home", "staging"];

#[derive(Debug, Clone)]
pub struct BrowserLayout {
    pub root: PathBuf,
    pub data_dir: PathBuf,
}

impl BrowserLayout {
    pub fn new(root: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            data_dir: data_dir.into(),
        }
    }

    pub fn chrome_dir(&self) -> PathBuf {
        self.root.join("chrome")
    }

    pub fn chrome_current(&self) -> PathBuf {
        self.chrome_dir().join("current")
    }

    pub fn chrome_binary(&self) -> PathBuf {
        self.chrome_current().join("chrome")
    }

    pub fn home_dir(&self) -> PathBuf {
        self.root.join("home")
    }

    pub fn user_dir(&self, user_id: &str) -> Result<PathBuf, BrowserError> {
        Ok(self.root.join(safe_user_segment(user_id)?))
    }

    pub fn profile_dir(&self, user_id: &str, instance: u8) -> Result<PathBuf, BrowserError> {
        Ok(self.user_dir(user_id)?.join(instance.to_string()))
    }

    pub fn settings_dir(&self) -> PathBuf {
        self.data_dir.join("browser").join("settings")
    }

    /// Paired devices of the owner's own browser (C34): root-only, never in
    /// the browser account's tree.
    pub fn devices_dir(&self) -> PathBuf {
        self.data_dir.join("browser").join("devices")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.data_dir.join("browser").join("staging")
    }

    /// `/root/.clauterm`-style parent of the root (for the host hardening).
    pub fn parent(&self) -> Option<&Path> {
        self.root.parent()
    }
}

/// A user id as one path segment: `[A-Za-z0-9_-]`, 1..=128, not reserved.
pub fn safe_user_segment(user_id: &str) -> Result<&str, BrowserError> {
    let ok = !user_id.is_empty()
        && user_id.len() <= 128
        && user_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !RESERVED.contains(&user_id);
    if ok {
        Ok(user_id)
    } else {
        Err(BrowserError::coded(
            BrowserErrorCode::BrowserBadMessage,
            "invalid user id",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_paths_follow_the_layout_and_refuse_unsafe_ids() {
        let l = BrowserLayout::new("/b", "/d");
        assert_eq!(
            l.profile_dir("system_default_user", 2).unwrap(),
            PathBuf::from("/b/system_default_user/2")
        );
        assert_eq!(l.chrome_binary(), PathBuf::from("/b/chrome/current/chrome"));
        assert_eq!(l.settings_dir(), PathBuf::from("/d/browser/settings"));
        for bad in ["", "..", "a/b", "chrome", "home", "a b", &"x".repeat(129)] {
            assert!(safe_user_segment(bad).is_err(), "{bad}");
        }
    }
}
