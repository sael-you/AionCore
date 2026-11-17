//! The Android companion app download (C38 K8). The signed release APK lives
//! in `<data dir>/devices/companion/` (`aionui-companion.apk`, `.sha256`,
//! `version.txt`); a companion pairing code comes with a short-lived download
//! link the UI shows as a QR code. The phone's browser has no AionUi session,
//! so the link carries a random token valid for [`DOWNLOAD_TTL`] (in memory;
//! links die with a restart). Tokens are never logged.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aionui_common::pairing::{constant_time_eq, new_device_token};
use dashmap::DashMap;

pub const DOWNLOAD_TTL: Duration = Duration::from_secs(10 * 60);
pub const APK_FILE: &str = "aionui-companion.apk";
const SHA_FILE: &str = "aionui-companion.apk.sha256";
const VERSION_FILE: &str = "version.txt";
/// Larger files are refused (a wrong file in the folder).
pub const APK_MAX_BYTES: u64 = 300 * 1024 * 1024;

pub struct CompanionDownloads {
    dir: PathBuf,
    tokens: DashMap<String, Instant>,
}

impl std::fmt::Debug for CompanionDownloads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanionDownloads").finish_non_exhaustive()
    }
}

impl CompanionDownloads {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            tokens: DashMap::new(),
        }
    }

    pub fn apk_path(&self) -> PathBuf {
        self.dir.join(APK_FILE)
    }

    pub fn available(&self) -> bool {
        std::fs::metadata(self.apk_path()).is_ok_and(|m| m.is_file() && m.len() <= APK_MAX_BYTES)
    }

    pub fn version(&self) -> Option<String> {
        read_trimmed(&self.dir.join(VERSION_FILE))
    }

    pub fn sha256(&self) -> Option<String> {
        read_trimmed(&self.dir.join(SHA_FILE)).and_then(|s| s.split_whitespace().next().map(str::to_owned))
    }

    /// A new download link for `host`, or `None` when no APK is installed.
    pub fn link(&self, host: Option<&str>) -> Option<String> {
        let host = host?;
        if !self.available() {
            return None;
        }
        let now = Instant::now();
        self.tokens.retain(|_, until| *until > now);
        let token = new_device_token();
        self.tokens.insert(token.clone(), now + DOWNLOAD_TTL);
        Some(format!("https://{host}/api/devices/companion.apk?t={token}"))
    }

    /// Whether `token` is a live link token (constant-time per entry).
    pub fn check(&self, token: &str) -> bool {
        let now = Instant::now();
        self.tokens
            .iter()
            .any(|e| *e.value() > now && constant_time_eq(e.key(), token))
    }

    pub async fn read_apk(&self) -> std::io::Result<Vec<u8>> {
        let path = self.apk_path();
        let meta = tokio::fs::metadata(&path).await?;
        if !meta.is_file() || meta.len() > APK_MAX_BYTES {
            return Err(std::io::Error::other("no usable APK"));
        }
        tokio::fs::read(path).await
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && s.len() <= 128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn links_only_with_an_apk_and_tokens_expire() {
        let dir = std::env::temp_dir().join(format!("aionui-companion-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let downloads = CompanionDownloads::new(&dir);
        assert!(downloads.link(Some("y.talkseeds.com")).is_none(), "no APK yet");
        std::fs::write(dir.join(APK_FILE), b"PK\x03\x04apk").unwrap();
        std::fs::write(dir.join(SHA_FILE), "abc123  aionui-companion.apk\n").unwrap();
        std::fs::write(dir.join(VERSION_FILE), "0.1.0\n").unwrap();
        assert!(downloads.link(None).is_none(), "no host, no link");
        let link = downloads.link(Some("y.talkseeds.com")).unwrap();
        let token = link.split("?t=").nth(1).unwrap();
        assert!(link.starts_with("https://y.talkseeds.com/api/devices/companion.apk?t="));
        assert!(downloads.check(token));
        assert!(!downloads.check("nope") && !downloads.check(""));
        assert_eq!(downloads.sha256().as_deref(), Some("abc123"));
        assert_eq!(downloads.version().as_deref(), Some("0.1.0"));
        assert_eq!(downloads.read_apk().await.unwrap(), b"PK\x03\x04apk");
        downloads
            .tokens
            .insert(token.to_owned(), Instant::now() - Duration::from_secs(1));
        assert!(!downloads.check(token), "expired");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
