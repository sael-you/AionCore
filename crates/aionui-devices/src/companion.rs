//! The Android companion app download (C38 K8). The signed release APK lives
//! in `<data dir>/devices/companion/` (`aionui-companion.apk`, `.sha256`,
//! `version.txt`); a companion pairing code comes with a short-lived download
//! link the UI shows as a QR code. The phone's browser has no AionUi session,
//! so the link carries a random token valid for [`DOWNLOAD_TTL`] (in memory;
//! links die with a restart). Tokens are never logged.
//!
//! The same folder is what a paired companion app updates itself from
//! (TASK-177): [`CompanionDownloads::published`] is the build on offer, and
//! only a consistent one counts (version, version code, and a `.sha256` that
//! is the hash of the APK on disk — a half-copied release is no offer). The
//! app gets a link whose token belongs to its pairing: a newer link or a
//! revoke of that pairing kills it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use aionui_common::now_ms;
use aionui_common::pairing::{constant_time_eq, new_device_token};
use dashmap::DashMap;
use sha2::{Digest, Sha256};

pub const DOWNLOAD_TTL: Duration = Duration::from_secs(10 * 60);
pub const APK_FILE: &str = "aionui-companion.apk";
const SHA_FILE: &str = "aionui-companion.apk.sha256";
const VERSION_FILE: &str = "version.txt";
/// Larger files are refused (a wrong file in the folder).
pub const APK_MAX_BYTES: u64 = 300 * 1024 * 1024;

/// The build on offer in the companion folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedCompanion {
    pub version: String,
    /// Android `versionCode` (line 2 of `version.txt`).
    pub version_code: u64,
    /// Lower-case hex SHA-256 of the APK, verified against the file.
    pub sha256: String,
    pub size: u64,
}

/// A download link for one pairing's app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateLink {
    /// `/api/devices/companion.apk?t=<token>` (the app adds its paired host).
    pub url: String,
    /// Unix ms.
    pub expires_at: i64,
}

/// What the three files looked like when they were last read: the hash is
/// only computed again when one of them changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Fingerprint {
    apk: Option<(u64, Option<SystemTime>)>,
    sha: Option<String>,
    version: Option<String>,
}

struct Token {
    until: Instant,
    /// The pairing an update link was made for; `None` for a pairing-code link.
    pairing_id: Option<String>,
}

pub struct CompanionDownloads {
    dir: PathBuf,
    tokens: DashMap<String, Token>,
    published: tokio::sync::Mutex<Option<(Fingerprint, Option<PublishedCompanion>)>>,
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
            published: tokio::sync::Mutex::new(None),
        }
    }

    pub fn apk_path(&self) -> PathBuf {
        self.dir.join(APK_FILE)
    }

    pub fn available(&self) -> bool {
        std::fs::metadata(self.apk_path()).is_ok_and(|m| m.is_file() && m.len() <= APK_MAX_BYTES)
    }

    pub fn version(&self) -> Option<String> {
        // Line 1 = versionName, line 2 = versionCode (the Android build script).
        read_trimmed(&self.dir.join(VERSION_FILE)).and_then(|s| s.lines().next().map(|l| l.trim().to_owned()))
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
        let token = self.issue(None);
        Some(format!("https://{host}/api/devices/companion.apk?t={token}"))
    }

    fn issue(&self, pairing_id: Option<&str>) -> String {
        let now = Instant::now();
        // One live update link per pairing: a newer one replaces the older.
        self.tokens
            .retain(|_, t| t.until > now && (pairing_id.is_none() || t.pairing_id.as_deref() != pairing_id));
        let token = new_device_token();
        self.tokens.insert(
            token.clone(),
            Token {
                until: now + DOWNLOAD_TTL,
                pairing_id: pairing_id.map(str::to_owned),
            },
        );
        token
    }

    /// A download link for the app of `pairing_id` (path form; the app only
    /// ever asks its paired host). Valid for [`DOWNLOAD_TTL`].
    pub fn update_link(&self, pairing_id: &str) -> UpdateLink {
        let token = self.issue(Some(pairing_id));
        UpdateLink {
            url: format!("/api/devices/companion.apk?t={token}"),
            expires_at: now_ms() + i64::try_from(DOWNLOAD_TTL.as_millis()).unwrap_or(0),
        }
    }

    /// The pairing was revoked: its update links are dead at once.
    pub fn forget_pairing(&self, pairing_id: &str) {
        self.tokens.retain(|_, t| t.pairing_id.as_deref() != Some(pairing_id));
    }

    /// Whether `token` is a live link token (constant-time per entry).
    pub fn check(&self, token: &str) -> bool {
        let now = Instant::now();
        self.tokens
            .iter()
            .any(|e| e.value().until > now && constant_time_eq(e.key(), token))
    }

    /// The build on offer, or `None` when the folder holds no consistent
    /// release. Cheap when nothing changed since the last call (file sizes,
    /// times and the two small text files are compared; the APK is hashed
    /// again only after a change).
    pub async fn published(&self) -> Option<PublishedCompanion> {
        let mut cached = self.published.lock().await;
        let dir = self.dir.clone();
        let known = cached.as_ref().map(|(f, _)| f.clone());
        let read = tokio::task::spawn_blocking(move || {
            let now = fingerprint(&dir);
            if known.as_ref() == Some(&now) {
                return None;
            }
            let published = read_published(&dir, &now);
            Some((now, published))
        })
        .await
        .ok()?;
        if let Some(fresh) = read {
            *cached = Some(fresh);
        }
        cached.as_ref().and_then(|(_, p)| p.clone())
    }

    /// The APK path when it is a regular file of sane size (it is streamed
    /// from disk by the route, never loaded into memory).
    pub async fn servable_apk(&self) -> Option<PathBuf> {
        let path = self.apk_path();
        let meta = tokio::fs::symlink_metadata(&path).await.ok()?;
        (meta.is_file() && meta.len() <= APK_MAX_BYTES).then_some(path)
    }
}

fn fingerprint(dir: &Path) -> Fingerprint {
    Fingerprint {
        apk: std::fs::symlink_metadata(dir.join(APK_FILE))
            .ok()
            .filter(std::fs::Metadata::is_file)
            .map(|m| (m.len(), m.modified().ok())),
        sha: read_trimmed(&dir.join(SHA_FILE)),
        version: read_trimmed(&dir.join(VERSION_FILE)),
    }
}

/// Blocking: reads the whole APK once to hash it.
fn read_published(dir: &Path, seen: &Fingerprint) -> Option<PublishedCompanion> {
    let (size, _) = seen.apk?;
    if size == 0 || size > APK_MAX_BYTES {
        return None;
    }
    let mut lines = seen.version.as_deref()?.lines().map(str::trim);
    let version = lines.next().filter(|v| !v.is_empty())?.to_owned();
    let version_code = lines.next()?.parse::<u64>().ok().filter(|c| *c >= 1)?;
    let sha256 = seen.sha.as_deref()?.split_whitespace().next()?.to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut file = std::fs::File::open(dir.join(APK_FILE)).ok()?;
    let mut hasher = Sha256::new();
    let read = std::io::copy(&mut file, &mut hasher).ok()?;
    let actual = hasher.finalize().iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    });
    (read == size && actual == sha256).then_some(PublishedCompanion {
        version,
        version_code,
        sha256,
        size,
    })
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
        std::fs::write(dir.join(VERSION_FILE), "0.1.0\n1\n").unwrap();
        assert!(downloads.link(None).is_none(), "no host, no link");
        let link = downloads.link(Some("y.talkseeds.com")).unwrap();
        let token = link.split("?t=").nth(1).unwrap();
        assert!(link.starts_with("https://y.talkseeds.com/api/devices/companion.apk?t="));
        assert!(downloads.check(token));
        assert!(!downloads.check("nope") && !downloads.check(""));
        assert_eq!(downloads.sha256().as_deref(), Some("abc123"));
        assert_eq!(downloads.version().as_deref(), Some("0.1.0"));
        assert_eq!(downloads.servable_apk().await, Some(dir.join(APK_FILE)));
        downloads.tokens.insert(
            token.to_owned(),
            Token {
                until: Instant::now() - Duration::from_secs(1),
                pairing_id: None,
            },
        );
        assert!(!downloads.check(token), "expired");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sha_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn publish(dir: &Path, apk: &[u8], version: &str) {
        std::fs::write(dir.join(APK_FILE), apk).unwrap();
        std::fs::write(dir.join(SHA_FILE), format!("{}  {APK_FILE}\n", sha_hex(apk))).unwrap();
        std::fs::write(dir.join(VERSION_FILE), version).unwrap();
    }

    #[tokio::test]
    async fn only_a_consistent_release_is_published() {
        let dir = std::env::temp_dir().join(format!("aionui-companion-published-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let downloads = CompanionDownloads::new(&dir);
        assert_eq!(downloads.published().await, None, "empty folder");

        let release_five: &[u8] = b"PK\x03\x04 release five";
        publish(&dir, release_five, "0.2.2\n5\n");
        let five = downloads.published().await.expect("published");
        assert_eq!(
            (five.version.as_str(), five.version_code, five.size),
            ("0.2.2", 5, release_five.len() as u64)
        );
        assert_eq!(five.sha256, sha_hex(release_five));
        assert_eq!(
            downloads.published().await,
            Some(five.clone()),
            "unchanged files, same answer"
        );

        // A release being copied in: the APK is new, its hash file is not yet.
        std::fs::write(dir.join(APK_FILE), b"PK\x03\x04 release six, longer").unwrap();
        assert_eq!(downloads.published().await, None, "hash file does not match the APK");
        publish(&dir, b"PK\x03\x04 release six, longer", "0.2.3\n6\n");
        assert_eq!(downloads.published().await.map(|p| p.version_code), Some(6));

        // No version code, a bad one, a hash that is not a hash.
        for version in ["0.2.3\n", "0.2.3\nsix\n", "0.2.3\n0\n", "\n6\n"] {
            std::fs::write(dir.join(VERSION_FILE), version).unwrap();
            assert_eq!(downloads.published().await, None, "version.txt {version:?}");
        }
        std::fs::write(dir.join(VERSION_FILE), "0.2.3\n6\n").unwrap();
        std::fs::write(dir.join(SHA_FILE), "not-a-hash\n").unwrap();
        assert_eq!(downloads.published().await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_update_link_belongs_to_its_pairing() {
        let dir = std::env::temp_dir().join(format!("aionui-companion-links-{}", std::process::id()));
        let downloads = CompanionDownloads::new(&dir);
        let token = |link: &UpdateLink| link.url.split("?t=").nth(1).unwrap().to_owned();
        let before = now_ms();
        let a1 = downloads.update_link("pair_a");
        assert!(a1.url.starts_with("/api/devices/companion.apk?t="), "path form");
        let ttl = i64::try_from(DOWNLOAD_TTL.as_millis()).unwrap();
        assert!((before + ttl..=now_ms() + ttl).contains(&a1.expires_at));
        let b = downloads.update_link("pair_b");
        assert!(downloads.check(&token(&a1)) && downloads.check(&token(&b)));

        // A newer link for the same pairing replaces the older one.
        let a2 = downloads.update_link("pair_a");
        assert!(!downloads.check(&token(&a1)));
        assert!(downloads.check(&token(&a2)) && downloads.check(&token(&b)));

        // A revoke kills that pairing's link and nobody else's.
        downloads.forget_pairing("pair_a");
        assert!(!downloads.check(&token(&a2)));
        assert!(downloads.check(&token(&b)));
    }
}
