//! Chrome installer/updater (C30, decision c): Google's official stable .deb,
//! verified end to end against Google's signed apt repository, extracted
//! (no apt, no root scripts) under `<root>/chrome/<version>/`, and switched
//! to by an atomic `current` symlink.
//!
//! Chain (verified by hand 2026-11-16 for 157.0.8103.78):
//! 1. `linux_signing_key.pub` → `gpg --import` into a throw-away home (gpg
//!    checks the subkey bindings) → the PRIMARY fingerprint must equal
//!    [`GOOGLE_PRIMARY_FPR`] (google.com/linuxrepositories) → export it as
//!    the only key of a gpgv keyring;
//! 2. `gpgv` on `dists/stable/InRelease` → the signed Release text;
//! 3. its SHA256 line for `main/binary-amd64/Packages` = sha256(Packages);
//! 4. the `google-chrome-stable` stanza's SHA256 + Size = the downloaded .deb.
//!
//! Every download and extraction happens in root-only staging under the data
//! dir; only the finished tree is moved into the browser root.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::BrowserError;
use crate::layout::BrowserLayout;

pub const GOOGLE_PRIMARY_FPR: &str = "EB4C1BFD4F042F6DDDCCEC917721F63BD38B4796";
const REPO: &str = "https://dl.google.com/linux/chrome/deb";
const KEY_URL: &str = "https://dl.google.com/linux/linux_signing_key.pub";
const PACKAGES_PATH: &str = "main/binary-amd64/Packages";
const PACKAGE: &str = "google-chrome-stable";
/// Refuse absurd sizes before downloading (the .deb is ~150 MB).
const MAX_DEB_BYTES: u64 = 600 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    pub version: String,
    pub filename: String,
    pub sha256: String,
    pub size: u64,
}

/// The `SHA256:` entry for `path` in a (verified) Release text.
pub fn release_sha256(release: &str, path: &str) -> Option<String> {
    let mut in_sha = false;
    for line in release.lines() {
        if !line.starts_with(' ') {
            in_sha = line.trim_end() == "SHA256:";
            continue;
        }
        if in_sha {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 3 && parts[2] == path {
                return Some(parts[0].to_ascii_lowercase());
            }
        }
    }
    None
}

/// The stanza of `package` in a Packages text.
pub fn packages_entry(packages: &str, package: &str) -> Option<PackageEntry> {
    for stanza in packages.split("\n\n") {
        let field = |name: &str| {
            stanza.lines().find_map(|l| {
                l.strip_prefix(name)
                    .and_then(|v| v.strip_prefix(':'))
                    .map(|v| v.trim().to_owned())
            })
        };
        if field("Package").as_deref() != Some(package) {
            continue;
        }
        return Some(PackageEntry {
            version: field("Version")?,
            filename: field("Filename")?,
            sha256: field("SHA256")?.to_ascii_lowercase(),
            size: field("Size")?.parse().ok()?,
        });
    }
    None
}

/// `157.0.8103.78-1` → `157.0.8103.78` (also a safe directory name).
pub fn dir_version(version: &str) -> Option<String> {
    let v = version.split('-').next()?;
    (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')).then(|| v.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The version `current` points at, if Chrome is installed.
pub fn installed_version(layout: &BrowserLayout) -> Option<String> {
    let target = std::fs::read_link(layout.chrome_current()).ok()?;
    let name = target.file_name()?.to_string_lossy().into_owned();
    layout.chrome_binary().exists().then_some(name)
}

pub struct ChromeInstaller {
    layout: BrowserLayout,
    http: reqwest::Client,
}

impl ChromeInstaller {
    pub fn new(layout: BrowserLayout) -> Result<Self, BrowserError> {
        let http = reqwest::Client::builder()
            .user_agent("aionui-browser-updater")
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .map_err(|e| BrowserError::Internal(e.to_string()))?;
        Ok(Self { layout, http })
    }

    async fn get(&self, url: &str) -> Result<Vec<u8>, BrowserError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| BrowserError::Internal(format!("download failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(BrowserError::Internal(format!("{url} → HTTP {}", resp.status())));
        }
        Ok(resp
            .bytes()
            .await
            .map_err(|e| BrowserError::Internal(format!("download failed: {e}")))?
            .to_vec())
    }

    /// Steps 1–3: the verified latest stable entry.
    pub async fn latest(&self) -> Result<PackageEntry, BrowserError> {
        let staging = self.fresh_staging("verify")?;
        let result = self.latest_in(&staging).await;
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    async fn latest_in(&self, staging: &Path) -> Result<PackageEntry, BrowserError> {
        let key = self.get(KEY_URL).await?;
        let keyring = pinned_keyring(staging, &key).await?;
        let in_release = self.get(&format!("{REPO}/dists/stable/InRelease")).await?;
        let release = gpgv_verify(staging, &keyring, &in_release).await?;
        let want = release_sha256(&release, PACKAGES_PATH)
            .ok_or_else(|| BrowserError::Internal("Release has no Packages hash".into()))?;
        let packages = self.get(&format!("{REPO}/dists/stable/{PACKAGES_PATH}")).await?;
        if sha256_hex(&packages) != want {
            return Err(BrowserError::Internal(
                "Packages hash does not match the signed Release".into(),
            ));
        }
        packages_entry(&String::from_utf8_lossy(&packages), PACKAGE)
            .ok_or_else(|| BrowserError::Internal(format!("{PACKAGE} not in Packages")))
    }

    /// Install `entry` unless it is installed already; returns the version dir.
    /// Does NOT switch `current` (see [`Self::activate`]).
    pub async fn install(&self, entry: &PackageEntry) -> Result<String, BrowserError> {
        let version =
            dir_version(&entry.version).ok_or_else(|| BrowserError::Internal("unexpected Chrome version".into()))?;
        let dest = self.layout.chrome_dir().join(&version);
        if dest.join("chrome").exists() {
            return Ok(version);
        }
        if entry.size > MAX_DEB_BYTES || !entry.filename.starts_with("pool/") || entry.filename.contains("..") {
            return Err(BrowserError::Internal("refusing an unexpected package entry".into()));
        }
        let staging = self.fresh_staging("install")?;
        let result = async {
            let deb = self.get(&format!("{REPO}/{}", entry.filename)).await?;
            if deb.len() as u64 != entry.size || sha256_hex(&deb) != entry.sha256 {
                return Err(BrowserError::Internal(
                    ".deb does not match the signed Packages entry".into(),
                ));
            }
            let deb_path = staging.join("chrome.deb");
            std::fs::write(&deb_path, &deb)?;
            let x = staging.join("x");
            let mut cmd = aionui_runtime::Builder::clean_cli("dpkg-deb");
            cmd.arg("-x").arg(&deb_path).arg(&x);
            let out = cmd.output().await?;
            if !out.status.success() {
                return Err(BrowserError::Internal("dpkg-deb -x failed".into()));
            }
            let tree = x.join("opt/google/chrome");
            if !tree.join("chrome").exists() {
                return Err(BrowserError::Internal("extracted package has no chrome binary".into()));
            }
            make_world_readable(&tree)?;
            std::fs::create_dir_all(self.layout.chrome_dir())?;
            set_mode(&self.layout.chrome_dir(), 0o755)?;
            let tmp = self.layout.chrome_dir().join(format!(".{version}.tmp"));
            let _ = std::fs::remove_dir_all(&tmp);
            move_dir(&tree, &tmp)?;
            std::fs::rename(&tmp, &dest)?;
            Ok(version.clone())
        }
        .await;
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// Point `current` at `version` atomically (temp link + rename) and remove
    /// older versions. Callers do this only while no instance runs.
    pub fn activate(&self, version: &str) -> Result<(), BrowserError> {
        let dir = self.layout.chrome_dir();
        let tmp = dir.join(".current.tmp");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(version, &tmp)?;
        std::fs::rename(&tmp, self.layout.chrome_current())?;
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != version && name != "current" && dir_version(&name).is_some() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(())
    }

    fn fresh_staging(&self, what: &str) -> Result<PathBuf, BrowserError> {
        let dir = self
            .layout
            .staging_dir()
            .join(format!("{what}-{}", uuid::Uuid::new_v4()));
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(&dir)?;
        Ok(dir)
    }
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// `chmod -R a+rX` (root keeps ownership; the browser user reads and executes).
fn make_world_readable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o7777;
    let exec = if meta.is_dir() || mode & 0o111 != 0 { 0o111 } else { 0 };
    // Drop setuid/setgid bits (chrome-sandbox is never used: --no-sandbox).
    set_mode(path, (mode & 0o0777 & !0o022) | 0o444 | exec)?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            make_world_readable(&entry?.path())?;
        }
    }
    Ok(())
}

/// `rename`, or copy + remove across filesystems.
fn move_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_tree(from, to)?;
    std::fs::remove_dir_all(from)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
    } else if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        std::fs::set_permissions(to, meta.permissions())?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// Import `key` into a fresh gpg home, require the pinned primary fingerprint,
/// and export exactly that key as a gpgv keyring.
async fn pinned_keyring(staging: &Path, key: &[u8]) -> Result<PathBuf, BrowserError> {
    let home = staging.join("gnupg");
    let mut b = std::fs::DirBuilder::new();
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(&home)?;
    let key_path = staging.join("key.pub");
    std::fs::write(&key_path, key)?;
    let gpg = |args: &[&str]| {
        let mut cmd = aionui_runtime::Builder::clean_cli("gpg");
        cmd.arg("--homedir")
            .arg(&home)
            .arg("--batch")
            .arg("--no-tty")
            .args(args);
        cmd
    };
    let out = gpg(&["--import", &key_path.to_string_lossy()]).output().await?;
    if !out.status.success() {
        return Err(BrowserError::Internal("gpg --import failed".into()));
    }
    let out = gpg(&["--with-colons", "--fingerprint", GOOGLE_PRIMARY_FPR])
        .output()
        .await?;
    let listing = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !primary_fingerprints(&listing).iter().any(|f| f == GOOGLE_PRIMARY_FPR) {
        return Err(BrowserError::Internal("Google signing key fingerprint mismatch".into()));
    }
    let keyring = staging.join("google.gpg");
    let out = gpg(&["--export", "--output", &keyring.to_string_lossy(), GOOGLE_PRIMARY_FPR])
        .output()
        .await?;
    if !out.status.success() || !keyring.exists() {
        return Err(BrowserError::Internal("gpg --export failed".into()));
    }
    Ok(keyring)
}

/// Primary-key fingerprints in `gpg --with-colons` output (the `fpr` line
/// right after each `pub` line).
pub fn primary_fingerprints(colons: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut after_pub = false;
    for line in colons.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.first().copied() {
            Some("pub") => after_pub = true,
            Some("fpr") if after_pub => {
                if let Some(f) = fields.get(9) {
                    out.push((*f).to_owned());
                }
                after_pub = false;
            }
            Some("sub") => after_pub = false,
            _ => {}
        }
    }
    out
}

async fn gpgv_verify(staging: &Path, keyring: &Path, signed: &[u8]) -> Result<String, BrowserError> {
    let input = staging.join("InRelease");
    let output = staging.join("Release");
    std::fs::write(&input, signed)?;
    let mut cmd = aionui_runtime::Builder::clean_cli("gpgv");
    cmd.arg("--keyring")
        .arg(keyring)
        .arg("--output")
        .arg(&output)
        .arg(&input);
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(BrowserError::Internal("InRelease signature did not verify".into()));
    }
    Ok(std::fs::read_to_string(&output)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = "Origin: Google LLC\nSHA256:\n 95b8d7f908a83ecf7b682ce11e943985f2dad24fa890db761061055b44289ef0 6205 main/binary-amd64/Packages\n 0000000000000000000000000000000000000000000000000000000000000000 100 main/binary-arm64/Packages\nSHA512:\n ffff 6205 main/binary-amd64/Packages\n";

    const PACKAGES: &str = "Package: google-chrome-beta\nVersion: 158.0.1-1\nFilename: pool/main/g/google-chrome-beta/b.deb\nSize: 1\nSHA256: aa\n\nPackage: google-chrome-stable\nVersion: 157.0.8103.78-1\nArchitecture: amd64\nFilename: pool/main/g/google-chrome-stable/google-chrome-stable_157.0.8103.78-1_amd64.deb\nSize: 147497676\nSHA256: 4D740636B5C96F687798602C5AAD6A5A270F44DD02E48DB0638AE77AA4867B1B\n";

    #[test]
    fn release_hash_is_read_from_the_sha256_section_only() {
        assert_eq!(
            release_sha256(RELEASE, "main/binary-amd64/Packages").as_deref(),
            Some("95b8d7f908a83ecf7b682ce11e943985f2dad24fa890db761061055b44289ef0")
        );
        assert_eq!(release_sha256(RELEASE, "main/binary-i386/Packages"), None);
    }

    #[test]
    fn packages_stanza_of_stable() {
        let e = packages_entry(PACKAGES, "google-chrome-stable").unwrap();
        assert_eq!(e.version, "157.0.8103.78-1");
        assert_eq!(e.size, 147_497_676);
        assert_eq!(
            e.sha256,
            "4d740636b5c96f687798602c5aad6a5a270f44dd02e48db0638ae77aa4867b1b"
        );
        assert!(e.filename.starts_with("pool/main/g/google-chrome-stable/"));
        assert_eq!(dir_version(&e.version).as_deref(), Some("157.0.8103.78"));
        assert_eq!(dir_version("../1-1"), None);
    }

    #[test]
    fn primary_fingerprint_is_the_one_after_pub() {
        let colons = "pub:-:4096:1:7721F63BD38B4796:1:::-:::scESC::::::23::0:\nfpr:::::::::EB4C1BFD4F042F6DDDCCEC917721F63BD38B4796:\nuid:-::::1::X::Google Inc.::::::::::0:\nsub:-:4096:1:1397BC53640DB551:1::::::s::::::23:\nfpr:::::::::3B068FB4789ABE4AEFA3BB491397BC53640DB551:\n";
        assert_eq!(primary_fingerprints(colons), vec![GOOGLE_PRIMARY_FPR.to_owned()]);
    }

    #[test]
    fn activate_switches_current_and_prunes_old_versions() {
        let t = tempfile::tempdir().unwrap();
        let layout = BrowserLayout::new(t.path().join("b"), t.path().join("d"));
        for v in ["156.0.1.1", "157.0.8103.78"] {
            let d = layout.chrome_dir().join(v);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("chrome"), "#!").unwrap();
        }
        let inst = ChromeInstaller::new(layout.clone()).unwrap();
        inst.activate("157.0.8103.78").unwrap();
        assert_eq!(installed_version(&layout).as_deref(), Some("157.0.8103.78"));
        assert!(!layout.chrome_dir().join("156.0.1.1").exists());
    }

    /// The real chain against dl.google.com (network; run with
    /// `AIONUI_BROWSER_NET_TESTS=1`). Verifies signatures and hashes only.
    #[tokio::test]
    async fn live_signed_chain_resolves_the_stable_entry() {
        if std::env::var("AIONUI_BROWSER_NET_TESTS").is_err() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let layout = BrowserLayout::new(t.path().join("b"), t.path().join("d"));
        let entry = ChromeInstaller::new(layout).unwrap().latest().await.unwrap();
        assert!(entry.version.contains('.'), "{entry:?}");
        assert_eq!(entry.sha256.len(), 64);
    }
}
