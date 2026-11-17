//! Engine installer/updater (C30 decision c for Chrome; C35 for the other
//! engines of the Chromium family): the vendor's official download, verified
//! as far as the vendor allows, extracted (no apt, no root scripts) under the
//! engine's directory `<version>/`, and switched to by an atomic `current`
//! symlink.
//!
//! Signed apt repositories (Chrome, Brave, Edge; chain verified by hand
//! 2026-11-16 for Chrome 157.0.8103.78 and 2026-11-17 for Brave 1.99.57 and
//! Edge 157.0.4341.60):
//! 1. the vendor's key file → `gpg --import` into a throw-away home (gpg
//!    checks the subkey bindings) → the PRIMARY fingerprint must equal the
//!    pinned one ([`crate::engine`]) → export it as the only key of a gpgv
//!    keyring;
//! 2. `gpgv` on `dists/stable/InRelease` → the signed Release text;
//! 3. its SHA256 line for `main/binary-amd64/Packages` = sha256(Packages);
//! 4. the package's stanza with the highest version: SHA256 + Size = the
//!    downloaded .deb.
//!
//! Chrome for Testing and chrome-headless-shell: Google publishes neither a
//! signature nor a checksum (C35 §5), so the chain is HTTPS to Google's index
//! → the Stable entry → a URL that must sit under Google's download prefix
//! for exactly that version. Weaker than the apt chain; the status says so.
//!
//! Every install writes `AIONUI-VERIFIED.json` into its version directory:
//! what was checked and the sha256 of the downloaded file.
//!
//! Every download and extraction happens in root-only staging under the data
//! dir; only the finished tree is moved into the browser root.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use aionui_api_types::BrowserEngine;

use crate::engine::{AptSource, CFT_DOWNLOAD_PREFIX, CFT_INDEX_URL, EngineSource, spec};
use crate::error::BrowserError;
use crate::layout::BrowserLayout;

pub use crate::engine::GOOGLE_PRIMARY_FPR;
const PACKAGES_PATH: &str = "main/binary-amd64/Packages";
/// Refuse absurd sizes before downloading (the largest package is ~200 MB).
const MAX_DOWNLOAD_BYTES: u64 = 600 * 1024 * 1024;
/// The verification record inside every installed version directory.
pub const VERIFY_RECORD: &str = "AIONUI-VERIFIED.json";

/// What [`EngineInstaller::latest`] resolved: the newest release of an engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineRelease {
    /// A stanza of a signature-verified apt `Packages` file.
    Deb(PackageEntry),
    /// A Chrome for Testing zip (HTTPS + URL shape only).
    Zip { version: String, url: String },
}

impl EngineRelease {
    pub fn version(&self) -> &str {
        match self {
            Self::Deb(e) => &e.version,
            Self::Zip { version, .. } => version,
        }
    }
}

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

/// The numeric parts of a version (`157.0.4341.60-1` → `[157,0,4341,60,1]`).
fn version_key(version: &str) -> Vec<u64> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().unwrap_or(u64::MAX))
        .collect()
}

/// The stanza of `package` with the highest version in a Packages text
/// (Brave's and Microsoft's repositories keep every old version).
pub fn packages_entry(packages: &str, package: &str) -> Option<PackageEntry> {
    let mut best: Option<PackageEntry> = None;
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
        let entry = (|| {
            Some(PackageEntry {
                version: field("Version")?,
                filename: field("Filename")?,
                sha256: field("SHA256")?.to_ascii_lowercase(),
                size: field("Size")?.parse().ok()?,
            })
        })();
        let Some(entry) = entry else { continue };
        if best
            .as_ref()
            .is_none_or(|b| version_key(&entry.version) > version_key(&b.version))
        {
            best = Some(entry);
        }
    }
    best
}

/// The Stable `linux64` download of `product` in Google's Chrome for Testing
/// index: `(version, url)`. The URL must be exactly Google's download prefix
/// + that version + `/linux64/` + a plain zip name.
pub fn cft_stable_entry(index: &str, product: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(index).ok()?;
    let stable = v.get("channels")?.get("Stable")?;
    let version = dir_version(stable.get("version")?.as_str()?)?;
    let url = stable
        .get("downloads")?
        .get(product)?
        .as_array()?
        .iter()
        .find(|d| d.get("platform").and_then(|p| p.as_str()) == Some("linux64"))?
        .get("url")?
        .as_str()?;
    let name = url.strip_prefix(&format!("{CFT_DOWNLOAD_PREFIX}{version}/linux64/"))?;
    let ok = name.ends_with(".zip")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    ok.then(|| (version, url.to_owned()))
}

/// `157.0.8103.78-1` → `157.0.8103.78` (also a safe directory name).
pub fn dir_version(version: &str) -> Option<String> {
    let v = version.split('-').next()?;
    (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')).then(|| v.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The version `current` points at, if `engine` is installed.
pub fn installed_version(layout: &BrowserLayout, engine: BrowserEngine) -> Option<String> {
    let target = std::fs::read_link(layout.engine_current(engine)).ok()?;
    let name = target.file_name()?.to_string_lossy().into_owned();
    layout.engine_binary(engine).exists().then_some(name)
}

pub struct EngineInstaller {
    layout: BrowserLayout,
    http: reqwest::Client,
}

impl EngineInstaller {
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

    /// The newest release of `engine`, verified as far as its source allows
    /// (apt: steps 1–3 of the signed chain).
    pub async fn latest(&self, engine: BrowserEngine) -> Result<EngineRelease, BrowserError> {
        match spec(engine).source {
            EngineSource::Apt(apt) => {
                let staging = self.fresh_staging("verify")?;
                let result = self.latest_apt(&apt, &staging).await;
                let _ = std::fs::remove_dir_all(&staging);
                result.map(EngineRelease::Deb)
            }
            EngineSource::ChromeForTesting { product, .. } => {
                let index = self.get(CFT_INDEX_URL).await?;
                let (version, url) = cft_stable_entry(&String::from_utf8_lossy(&index), product).ok_or_else(|| {
                    BrowserError::Internal(format!("no usable {product} download in the Chrome for Testing index"))
                })?;
                Ok(EngineRelease::Zip { version, url })
            }
        }
    }

    /// The signed chain for one apt package (steps 1–3); also used by other
    /// engine lanes for their own pinned repositories.
    pub(crate) async fn latest_apt(&self, apt: &AptSource, staging: &Path) -> Result<PackageEntry, BrowserError> {
        let key = self.get(apt.key_url).await?;
        let keyring = pinned_keyring(staging, &key, apt.primary_fpr).await?;
        let in_release = self.get(&format!("{}/dists/{}/InRelease", apt.repo, apt.suite)).await?;
        let release = gpgv_verify(staging, &keyring, &in_release).await?;
        let want = release_sha256(&release, PACKAGES_PATH)
            .ok_or_else(|| BrowserError::Internal("Release has no Packages hash".into()))?;
        let packages = self
            .get(&format!("{}/dists/{}/{PACKAGES_PATH}", apt.repo, apt.suite))
            .await?;
        if sha256_hex(&packages) != want {
            return Err(BrowserError::Internal(
                "Packages hash does not match the signed Release".into(),
            ));
        }
        packages_entry(&String::from_utf8_lossy(&packages), apt.package)
            .ok_or_else(|| BrowserError::Internal(format!("{} not in Packages", apt.package)))
    }

    /// Install `release` of `engine` unless it is installed already; returns
    /// the version dir. Does NOT switch `current` (see [`Self::activate`]).
    pub async fn install(&self, engine: BrowserEngine, release: &EngineRelease) -> Result<String, BrowserError> {
        let s = spec(engine);
        let version =
            dir_version(release.version()).ok_or_else(|| BrowserError::Internal("unexpected engine version".into()))?;
        let engine_dir = self.layout.engine_dir(engine);
        let dest = engine_dir.join(&version);
        if dest.join(s.binary).exists() {
            return Ok(version);
        }
        let staging = self.fresh_staging("install")?;
        let result = async {
            let x = staging.join("x");
            let (tree, record) = match (s.source, release) {
                (EngineSource::Apt(apt), EngineRelease::Deb(entry)) => {
                    if entry.size > MAX_DOWNLOAD_BYTES
                        || !entry.filename.starts_with("pool/")
                        || entry.filename.contains("..")
                    {
                        return Err(BrowserError::Internal("refusing an unexpected package entry".into()));
                    }
                    let url = format!("{}/{}", apt.repo, entry.filename);
                    let deb = self.get(&url).await?;
                    if deb.len() as u64 != entry.size || sha256_hex(&deb) != entry.sha256 {
                        return Err(BrowserError::Internal(
                            ".deb does not match the signed Packages entry".into(),
                        ));
                    }
                    let deb_path = staging.join("engine.deb");
                    std::fs::write(&deb_path, &deb)?;
                    let mut cmd = aionui_runtime::Builder::clean_cli("dpkg-deb");
                    cmd.arg("-x").arg(&deb_path).arg(&x);
                    if !cmd.output().await?.status.success() {
                        return Err(BrowserError::Internal("dpkg-deb -x failed".into()));
                    }
                    let record = serde_json::json!({
                        "engine": engine.id(),
                        "version": version,
                        "verification": "signed-repository",
                        "checked": "pinned primary key -> gpgv InRelease -> Packages sha256 -> package sha256 + size",
                        "signing_key": apt.primary_fpr,
                        "source": url,
                        "sha256": entry.sha256,
                        "size": entry.size,
                    });
                    (x.join(apt.tree), record)
                }
                (EngineSource::ChromeForTesting { zip_dir, .. }, EngineRelease::Zip { url, .. }) => {
                    if !url.starts_with(CFT_DOWNLOAD_PREFIX) {
                        return Err(BrowserError::Internal("refusing an unexpected download URL".into()));
                    }
                    let zip = self.get(url).await?;
                    if zip.len() as u64 > MAX_DOWNLOAD_BYTES {
                        return Err(BrowserError::Internal("download is too large".into()));
                    }
                    let zip_path = staging.join("engine.zip");
                    std::fs::write(&zip_path, &zip)?;
                    let mut cmd = aionui_runtime::Builder::clean_cli("unzip");
                    cmd.arg("-q").arg(&zip_path).arg("-d").arg(&x);
                    if !cmd.output().await?.status.success() {
                        return Err(BrowserError::Internal("unzip failed".into()));
                    }
                    let record = serde_json::json!({
                        "engine": engine.id(),
                        "version": version,
                        "verification": "tls-only",
                        "checked": "HTTPS to Google's Chrome for Testing index and download host; URL pinned to that version",
                        "not_checked": "the vendor publishes no signature and no checksum for this download",
                        "source": url,
                        "sha256": sha256_hex(&zip),
                        "size": zip.len(),
                    });
                    (x.join(zip_dir), record)
                }
                _ => return Err(BrowserError::Internal("release does not match the engine".into())),
            };
            if !tree.join(s.binary).exists() {
                return Err(BrowserError::Internal(
                    "extracted package has no browser binary".into(),
                ));
            }
            let body = serde_json::to_vec_pretty(&record).map_err(|e| BrowserError::Internal(e.to_string()))?;
            std::fs::write(tree.join(VERIFY_RECORD), body)?;
            make_world_readable(&tree)?;
            std::fs::create_dir_all(&engine_dir)?;
            // `engines/` and the engine's own directory: traversable, root-owned.
            for dir in [engine_dir.parent(), Some(engine_dir.as_path())].into_iter().flatten() {
                if dir != self.layout.root {
                    set_mode(dir, 0o755)?;
                }
            }
            let tmp = engine_dir.join(format!(".{version}.tmp"));
            let _ = std::fs::remove_dir_all(&tmp);
            move_dir(&tree, &tmp)?;
            std::fs::rename(&tmp, &dest)?;
            Ok(version.clone())
        }
        .await;
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// Point `engine`'s `current` at `version` atomically (temp link + rename)
    /// and remove older versions. Callers do this only while no instance of
    /// that engine runs.
    pub fn activate(&self, engine: BrowserEngine, version: &str) -> Result<(), BrowserError> {
        let dir = self.layout.engine_dir(engine);
        let tmp = dir.join(".current.tmp");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(version, &tmp)?;
        std::fs::rename(&tmp, self.layout.engine_current(engine))?;
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

/// Import `key` into a fresh gpg home, require the pinned primary fingerprint
/// `fpr`, and export exactly that key as a gpgv keyring (a key file may hold
/// several keys, e.g. Brave's: only the pinned one is trusted).
async fn pinned_keyring(staging: &Path, key: &[u8], fpr: &str) -> Result<PathBuf, BrowserError> {
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
            // Public keys only: no gpg-agent. With a long staging path the
            // agent's socket path is too long, it fails to start and
            // `--import` exits 2 although the key was imported.
            .arg("--no-autostart")
            .args(args);
        cmd
    };
    let out = gpg(&["--import", &key_path.to_string_lossy()]).output().await?;
    if !out.status.success() {
        return Err(BrowserError::Internal("gpg --import failed".into()));
    }
    let out = gpg(&["--with-colons", "--fingerprint", fpr]).output().await?;
    let listing = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !primary_fingerprints(&listing).iter().any(|f| f == fpr) {
        return Err(BrowserError::Internal("vendor signing key fingerprint mismatch".into()));
    }
    let keyring = staging.join("vendor.gpg");
    let out = gpg(&["--export", "--output", &keyring.to_string_lossy(), fpr])
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
        let inst = EngineInstaller::new(layout.clone()).unwrap();
        inst.activate(BrowserEngine::Chrome, "157.0.8103.78").unwrap();
        assert_eq!(
            installed_version(&layout, BrowserEngine::Chrome).as_deref(),
            Some("157.0.8103.78")
        );
        assert!(!layout.chrome_dir().join("156.0.1.1").exists());
        assert_eq!(
            installed_version(&layout, BrowserEngine::Edge),
            None,
            "engines are separate"
        );
    }

    #[test]
    fn engines_install_side_by_side_under_their_own_directory() {
        let t = tempfile::tempdir().unwrap();
        let layout = BrowserLayout::new(t.path().join("b"), t.path().join("d"));
        let d = layout.engine_dir(BrowserEngine::Edge).join("157.0.4341.60");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("msedge"), "#!").unwrap();
        let inst = EngineInstaller::new(layout.clone()).unwrap();
        inst.activate(BrowserEngine::Edge, "157.0.4341.60").unwrap();
        assert_eq!(
            installed_version(&layout, BrowserEngine::Edge).as_deref(),
            Some("157.0.4341.60")
        );
        assert_eq!(installed_version(&layout, BrowserEngine::Chrome), None);
        assert_eq!(
            layout.engine_binary(BrowserEngine::Edge),
            t.path().join("b/engines/edge/current/msedge")
        );
    }

    /// Brave's and Microsoft's repositories list every old version first.
    #[test]
    fn the_highest_version_of_a_package_wins() {
        let packages = "Package: brave-browser\nVersion: 1.51.114\nFilename: pool/main/b/brave-browser/a.deb\nSize: 1\nSHA256: aa\n\nPackage: brave-browser\nVersion: 1.99.57\nFilename: pool/main/b/brave-browser/c.deb\nSize: 3\nSHA256: cc\n\nPackage: brave-browser\nVersion: 1.9.200\nFilename: pool/main/b/brave-browser/b.deb\nSize: 2\nSHA256: bb\n\nPackage: brave-browser-beta\nVersion: 2.0.1\nFilename: pool/x.deb\nSize: 9\nSHA256: ee\n";
        let e = packages_entry(packages, "brave-browser").unwrap();
        assert_eq!((e.version.as_str(), e.size), ("1.99.57", 3));
        assert!(version_key("157.0.4341.60-1") > version_key("116.0.1938.76-1"));
        assert!(version_key("157.0.4341.60-2") > version_key("157.0.4341.60-1"));
    }

    #[test]
    fn chrome_for_testing_entry_needs_googles_prefix_and_the_same_version() {
        let index = |url: &str| {
            format!(
                r#"{{"channels":{{"Stable":{{"version":"157.0.8103.78","downloads":{{"chrome-headless-shell":[{{"platform":"mac-arm64","url":"https://storage.googleapis.com/chrome-for-testing-public/157.0.8103.78/mac-arm64/x.zip"}},{{"platform":"linux64","url":"{url}"}}]}}}}}}}}"#
            )
        };
        let good = "https://storage.googleapis.com/chrome-for-testing-public/157.0.8103.78/linux64/chrome-headless-shell-linux64.zip";
        assert_eq!(
            cft_stable_entry(&index(good), "chrome-headless-shell"),
            Some(("157.0.8103.78".to_owned(), good.to_owned()))
        );
        assert_eq!(cft_stable_entry(&index(good), "chrome"), None, "product missing");
        for bad in [
            "https://evil.example/chrome-for-testing-public/157.0.8103.78/linux64/a.zip",
            "https://storage.googleapis.com/chrome-for-testing-public/156.0.1.1/linux64/a.zip",
            "https://storage.googleapis.com/chrome-for-testing-public/157.0.8103.78/linux64/../../x/a.zip",
            "https://storage.googleapis.com/chrome-for-testing-public/157.0.8103.78/linux64/a.zip?x=1",
            "https://storage.googleapis.com/chrome-for-testing-public/157.0.8103.78/linux64/a.tar",
        ] {
            assert_eq!(cft_stable_entry(&index(bad), "chrome-headless-shell"), None, "{bad}");
        }
    }

    /// The real chains against the vendors (network; run with
    /// `AIONUI_BROWSER_NET_TESTS=1`). Verifies signatures and hashes only.
    #[tokio::test]
    async fn live_chains_resolve_every_engine() {
        if std::env::var("AIONUI_BROWSER_NET_TESTS").is_err() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let layout = BrowserLayout::new(t.path().join("b"), t.path().join("d"));
        let installer = EngineInstaller::new(layout).unwrap();
        for engine in aionui_api_types::BROWSER_ENGINES {
            let release = installer
                .latest(engine)
                .await
                .unwrap_or_else(|e| panic!("{engine:?}: {e}"));
            eprintln!("{engine:?}: {release:?}");
            assert!(dir_version(release.version()).is_some(), "{engine:?} {release:?}");
            if let EngineRelease::Deb(entry) = &release {
                assert_eq!(entry.sha256.len(), 64);
            }
        }
    }
}
