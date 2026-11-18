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
//! No downgrades: a release older than the highest version this server ever
//! installed of that engine is refused (the highest is kept root-only under
//! the data dir, so it survives a remove), and `current` only ever moves to a
//! strictly higher version. A rolled-back index or repository therefore
//! cannot bring an old, vulnerable browser back.
//!
//! Downloads talk to the engine's own vendor hosts only, redirects included.
//! Archives never make root follow a link: zips are unpacked in-process
//! (entry names must stay inside, links must point inside), and an extracted
//! tree is checked (no link out of the tree, no hard links, no special
//! files) before anything is done with it.
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
    /// A release pinned in AionCore's source (Camoufox): no update discovery.
    Pinned { version: String },
}

impl EngineRelease {
    pub fn version(&self) -> &str {
        match self {
            Self::Deb(e) => &e.version,
            Self::Zip { version, .. } => version,
            Self::Pinned { version } => version,
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

/// A pinned version used verbatim as a directory name: starts with a digit,
/// then digits, letters, `.` and `-` only.
pub fn pinned_dir_name(version: &str) -> bool {
    version.len() <= 64
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !version.contains("..")
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

/// Whether `url` is HTTPS on one of `hosts`.
fn url_on_hosts(url: &reqwest::Url, hosts: &[&str]) -> bool {
    url.scheme() == "https" && url.host_str().is_some_and(|h| hosts.contains(&h))
}

/// An HTTP client that only ever talks to `hosts` (every redirect hop too).
fn pinned_client(hosts: Vec<&'static str>) -> Result<reqwest::Client, BrowserError> {
    reqwest::Client::builder()
        .user_agent("aionui-browser-updater")
        .timeout(std::time::Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if url_on_hosts(attempt.url(), &hosts) {
                attempt.follow()
            } else {
                attempt.error("redirect to a host outside the engine's vendor list")
            }
        }))
        .build()
        .map_err(|e| BrowserError::Internal(e.to_string()))
}

/// `a` is strictly newer than `b`.
pub fn version_newer(a: &str, b: &str) -> bool {
    version_key(a) > version_key(b)
}

/// Lexical check that a relative link `target` placed in directory `dir`
/// (relative to the tree root) stays inside the tree.
fn link_stays_inside(dir: &Path, target: &Path) -> bool {
    use std::path::Component;
    let mut depth: i64 = dir.components().count() as i64;
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Refuse an extracted tree that could make root act outside it: `tree`
/// must be reached from `base` through real directories, and inside it there
/// may be only directories, regular files with one link, and symlinks whose
/// target stays inside the tree.
pub fn check_tree_contained(base: &Path, tree: &Path) -> Result<(), BrowserError> {
    use std::os::unix::fs::MetadataExt;
    let refuse =
        |what: &str, path: &Path| BrowserError::Internal(format!("refusing the archive: {what} at {}", path.display()));
    let rel = tree
        .strip_prefix(base)
        .map_err(|_| refuse("tree outside the extraction directory", tree))?;
    let mut at = base.to_path_buf();
    for part in rel.components() {
        at.push(part);
        let meta = std::fs::symlink_metadata(&at)?;
        if !meta.is_dir() {
            return Err(refuse("a link or file on the way to the browser directory", &at));
        }
    }
    fn walk(root: &Path, dir: &Path, refuse: &dyn Fn(&str, &Path) -> BrowserError) -> Result<(), BrowserError> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                let target = std::fs::read_link(&path)?;
                let parent = path
                    .parent()
                    .and_then(|p| p.strip_prefix(root).ok())
                    .unwrap_or(Path::new(""));
                if !link_stays_inside(parent, &target) {
                    return Err(refuse("a symlink that leaves the browser directory", &path));
                }
            } else if kind.is_dir() {
                walk(root, &path, refuse)?;
            } else if kind.is_file() {
                if meta.nlink() != 1 {
                    return Err(refuse("a hard link", &path));
                }
            } else {
                return Err(refuse("a special file", &path));
            }
        }
        Ok(())
    }
    walk(tree, tree, &refuse)
}

/// Unpack `zip_path` into the new directory `dest` without ever writing
/// through a link: entry names must stay inside (`enclosed_name`), files are
/// created new, symlinks must point inside and are created last.
pub fn unzip_contained(zip_path: &Path, dest: &Path) -> Result<(), BrowserError> {
    unzip_contained_within(zip_path, dest, 4 * MAX_DOWNLOAD_BYTES)
}

/// [`unzip_contained`] with the unpacked-size limit of the caller's engine
/// (Camoufox unpacks to ~2.6 GB).
pub fn unzip_contained_within(zip_path: &Path, dest: &Path, max_unpacked: u64) -> Result<(), BrowserError> {
    use std::os::unix::fs::OpenOptionsExt;
    let bad = |what: String| BrowserError::Internal(format!("refusing the archive: {what}"));
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path)?).map_err(|e| bad(e.to_string()))?;
    std::fs::create_dir(dest)?;
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| bad(e.to_string()))?;
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| bad(format!("entry name {:?} leaves the archive", entry.name())))?;
        let out = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if entry.is_symlink() {
            let mut target = String::new();
            std::io::Read::read_to_string(&mut std::io::Read::take(&mut entry, 4096), &mut target)?;
            let parent = rel.parent().unwrap_or(Path::new(""));
            if !link_stays_inside(parent, Path::new(&target)) {
                return Err(bad(format!("symlink {} leaves the archive", rel.display())));
            }
            links.push((out, PathBuf::from(target)));
            continue;
        }
        total = total.saturating_add(entry.size());
        if total > max_unpacked {
            return Err(bad("unpacked size is too large".into()));
        }
        let mode = entry.unix_mode().unwrap_or(0o644) & 0o755;
        // `create_new` never follows an existing link (O_EXCL).
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode | 0o600)
            .open(&out)?;
        std::io::copy(&mut entry, &mut file)?;
    }
    // Links last: no later entry can be written through one.
    for (path, target) in links {
        std::os::unix::fs::symlink(target, path)?;
    }
    Ok(())
}

pub struct EngineInstaller {
    layout: BrowserLayout,
    /// One client per engine, pinned to that engine's vendor hosts.
    http: std::collections::HashMap<BrowserEngine, (reqwest::Client, Vec<&'static str>)>,
}

impl EngineInstaller {
    pub fn new(layout: BrowserLayout) -> Result<Self, BrowserError> {
        let mut http = std::collections::HashMap::new();
        for engine in aionui_api_types::BROWSER_ENGINES {
            let hosts = spec(engine).source.hosts();
            http.insert(engine, (pinned_client(hosts.clone())?, hosts));
        }
        Ok(Self { layout, http })
    }

    /// GET `url` for `engine`: HTTPS on the engine's vendor hosts only (the
    /// client refuses every redirect that leaves them).
    async fn get(&self, engine: BrowserEngine, url: &str) -> Result<Vec<u8>, BrowserError> {
        let (client, hosts) = self
            .http
            .get(&engine)
            .ok_or_else(|| BrowserError::Internal("no download client for the engine".into()))?;
        let parsed = reqwest::Url::parse(url).map_err(|e| BrowserError::Internal(format!("bad download URL: {e}")))?;
        if !url_on_hosts(&parsed, hosts) {
            return Err(BrowserError::Internal(
                "refusing a download outside the engine's vendor hosts".into(),
            ));
        }
        let resp = client
            .get(parsed)
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

    fn highest_path(&self, engine: BrowserEngine) -> PathBuf {
        self.layout
            .data_dir
            .join("browser")
            .join("engines")
            .join(format!("{}.highest", engine.id()))
    }

    /// The highest version of `engine` this server ever installed (kept
    /// root-only under the data dir; survives a remove).
    pub fn highest_installed(&self, engine: BrowserEngine) -> Option<String> {
        let recorded = std::fs::read_to_string(self.highest_path(engine))
            .ok()
            .and_then(|v| dir_version(v.trim()));
        let current = installed_version(&self.layout, engine).and_then(|v| dir_version(&v));
        match (recorded, current) {
            (Some(r), Some(c)) => Some(if version_newer(&c, &r) { c } else { r }),
            (r, c) => r.or(c),
        }
    }

    fn record_highest(&self, engine: BrowserEngine, version: &str) -> Result<(), BrowserError> {
        let path = self.highest_path(engine);
        let recorded = std::fs::read_to_string(&path).ok().and_then(|v| dir_version(v.trim()));
        if recorded.is_some_and(|r| !version_newer(version, &r)) {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            let mut b = std::fs::DirBuilder::new();
            b.recursive(true);
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, version)?;
        set_mode(&tmp, 0o600)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Refuse `version` when it is older than the highest ever installed.
    fn refuse_downgrade(&self, engine: BrowserEngine, version: &str) -> Result<(), BrowserError> {
        match self.highest_installed(engine) {
            Some(highest) if version_newer(&highest, version) => Err(BrowserError::Internal(format!(
                "refusing a downgrade of {}: {version} is older than {highest}",
                engine.id()
            ))),
            _ => Ok(()),
        }
    }

    /// The newest release of `engine`, verified as far as its source allows
    /// (apt: steps 1–3 of the signed chain).
    pub async fn latest(&self, engine: BrowserEngine) -> Result<EngineRelease, BrowserError> {
        match spec(engine).source {
            EngineSource::Apt(apt) => {
                let staging = self.fresh_staging("verify")?;
                let result = self.latest_apt(engine, &apt, &staging).await;
                let _ = std::fs::remove_dir_all(&staging);
                result.map(EngineRelease::Deb)
            }
            EngineSource::ChromeForTesting { product, .. } => {
                let index = self.get(engine, CFT_INDEX_URL).await?;
                let (version, url) = cft_stable_entry(&String::from_utf8_lossy(&index), product).ok_or_else(|| {
                    BrowserError::Internal(format!("no usable {product} download in the Chrome for Testing index"))
                })?;
                Ok(EngineRelease::Zip { version, url })
            }
            EngineSource::GithubRelease { .. } => Ok(EngineRelease::Pinned {
                version: crate::camoufox::VERSION.to_owned(),
            }),
        }
    }

    /// The signed chain for one apt package (steps 1–3); also used by other
    /// engine lanes for their own pinned repositories.
    pub(crate) async fn latest_apt(
        &self,
        engine: BrowserEngine,
        apt: &AptSource,
        staging: &Path,
    ) -> Result<PackageEntry, BrowserError> {
        let key = self.get(engine, apt.key_url).await?;
        let keyring = pinned_keyring(staging, &key, apt.primary_fpr).await?;
        let in_release = self
            .get(engine, &format!("{}/dists/{}/InRelease", apt.repo, apt.suite))
            .await?;
        let release = gpgv_verify(staging, &keyring, &in_release).await?;
        let want = release_sha256(&release, PACKAGES_PATH)
            .ok_or_else(|| BrowserError::Internal("Release has no Packages hash".into()))?;
        let packages = self
            .get(engine, &format!("{}/dists/{}/{PACKAGES_PATH}", apt.repo, apt.suite))
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
        let version = match release {
            // A pinned release keeps its exact name (`159.0-beta.42`): a later
            // beta must not collide with it.
            EngineRelease::Pinned { version } if pinned_dir_name(version) => version.clone(),
            _ => dir_version(release.version())
                .ok_or_else(|| BrowserError::Internal("unexpected engine version".into()))?,
        };
        let engine_dir = self.layout.engine_dir(engine);
        let dest = engine_dir.join(&version);
        // Before anything is downloaded: never go back to an older release.
        self.refuse_downgrade(engine, &version)?;
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
                    let deb = self.get(engine, &url).await?;
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
                    let zip = self.get(engine, url).await?;
                    if zip.len() as u64 > MAX_DOWNLOAD_BYTES {
                        return Err(BrowserError::Internal("download is too large".into()));
                    }
                    let zip_path = staging.join("engine.zip");
                    std::fs::write(&zip_path, &zip)?;
                    // In-process and link-safe (this download is not signed).
                    let (from, to) = (zip_path.clone(), x.clone());
                    tokio::task::spawn_blocking(move || unzip_contained(&from, &to))
                        .await
                        .map_err(|e| BrowserError::Internal(e.to_string()))??;
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
                (EngineSource::GithubRelease { .. }, EngineRelease::Pinned { .. }) => {
                    let (client, _) = self
                        .http
                        .get(&engine)
                        .ok_or_else(|| BrowserError::Internal("no download client for the engine".into()))?;
                    let built = crate::camoufox::build_tree(client, &staging, &x, &self.layout.cache_dir()).await?;
                    check_tree_contained(&x, &built.0)?;
                    built
                }
                _ => return Err(BrowserError::Internal("release does not match the engine".into())),
            };
            // Before root touches the tree: nothing in it may lead outside.
            check_tree_contained(&x, &tree)?;
            if !std::fs::symlink_metadata(tree.join(s.binary)).is_ok_and(|m| m.is_file()) {
                return Err(BrowserError::Internal(
                    "extracted package has no browser binary".into(),
                ));
            }
            let body = serde_json::to_vec_pretty(&record).map_err(|e| BrowserError::Internal(e.to_string()))?;
            // `create_new`: never through a link the archive could have put there.
            let mut record_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(tree.join(VERIFY_RECORD))?;
            std::io::Write::write_all(&mut record_file, &body)?;
            drop(record_file);
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
    ///
    /// Only forward: a version that is not strictly higher than the installed
    /// one (or lower than the highest ever installed) is refused.
    pub fn activate(&self, engine: BrowserEngine, version: &str) -> Result<(), BrowserError> {
        // A pinned release (Camoufox) keeps its exact name as directory.
        let pinned = matches!(spec(engine).source, EngineSource::GithubRelease { .. }) && pinned_dir_name(version);
        if !pinned && dir_version(version).as_deref() != Some(version) {
            return Err(BrowserError::Internal("unexpected engine version".into()));
        }
        self.refuse_downgrade(engine, version)?;
        if installed_version(&self.layout, engine).is_some_and(|current| !version_newer(version, &current)) {
            return Err(BrowserError::Internal(format!(
                "refusing to switch {} to {version}: not newer than the installed version",
                engine.id()
            )));
        }
        let dir = self.layout.engine_dir(engine);
        let tmp = dir.join(".current.tmp");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(version, &tmp)?;
        std::fs::rename(&tmp, self.layout.engine_current(engine))?;
        self.record_highest(engine, version)?;
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

    fn fake_version(layout: &BrowserLayout, engine: BrowserEngine, version: &str) {
        let d = layout.engine_dir(engine).join(version);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(spec(engine).binary), "#!").unwrap();
    }

    /// Review blocker: `current` only moves forward, and a release older
    /// than the highest version ever installed is refused before any
    /// download — also after the engine's files are gone.
    #[tokio::test]
    async fn downgrades_are_refused_and_the_highest_version_is_remembered() {
        let t = tempfile::tempdir().unwrap();
        let layout = BrowserLayout::new(t.path().join("b"), t.path().join("d"));
        let inst = EngineInstaller::new(layout.clone()).unwrap();
        let engine = BrowserEngine::ChromeForTesting;
        for v in ["157.0.8103.78", "156.0.7000.10", "157.0.8103.9"] {
            fake_version(&layout, engine, v);
        }
        inst.activate(engine, "157.0.8103.78").unwrap();
        assert_eq!(inst.highest_installed(engine).as_deref(), Some("157.0.8103.78"));
        // Older, numerically-not-lexically older, and the same version again.
        for old in ["156.0.7000.10", "157.0.8103.9", "157.0.8103.78"] {
            fake_version(&layout, engine, old);
            let e = inst.activate(engine, old).unwrap_err().to_string();
            assert!(e.contains("refusing"), "{old}: {e}");
            assert_eq!(installed_version(&layout, engine).as_deref(), Some("157.0.8103.78"));
        }
        assert!(inst.activate(engine, "../157.1").is_err());

        // A rolled-back index: refused before the download (the URL is never fetched).
        let old = EngineRelease::Zip {
            version: "150.0.1.1".into(),
            url: format!("{CFT_DOWNLOAD_PREFIX}150.0.1.1/linux64/chrome-linux64.zip"),
        };
        let e = inst.install(engine, &old).await.unwrap_err().to_string();
        assert!(e.contains("refusing a downgrade") && e.contains("157.0.8103.78"), "{e}");
        assert!(!layout.staging_dir().exists(), "nothing was staged");

        // The memory survives the files (a remove, a wiped browser root).
        std::fs::remove_dir_all(layout.engine_dir(engine)).unwrap();
        assert_eq!(installed_version(&layout, engine), None);
        assert_eq!(inst.highest_installed(engine).as_deref(), Some("157.0.8103.78"));
        assert!(
            inst.install(engine, &old)
                .await
                .unwrap_err()
                .to_string()
                .contains("downgrade")
        );
        fake_version(&layout, engine, "156.0.7000.10");
        assert!(inst.activate(engine, "156.0.7000.10").is_err());
        // Forward is fine, and other engines have their own memory.
        fake_version(&layout, engine, "158.0.1.1");
        inst.activate(engine, "158.0.1.1").unwrap();
        assert_eq!(inst.highest_installed(engine).as_deref(), Some("158.0.1.1"));
        assert_eq!(inst.highest_installed(BrowserEngine::Brave), None);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(inst.highest_path(engine))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn downloads_stay_on_the_engines_vendor_hosts() {
        let on = |url: &str, hosts: &[&str]| url_on_hosts(&reqwest::Url::parse(url).unwrap(), hosts);
        let google = ["dl.google.com"];
        assert!(on(
            "https://dl.google.com/linux/chrome/deb/dists/stable/InRelease",
            &google
        ));
        for bad in [
            "http://dl.google.com/linux/x",
            "https://dl.google.com.evil.example/x",
            "https://evil.example/dl.google.com",
            "https://packages.microsoft.com/x",
            "https://user@evil.example/x",
        ] {
            assert!(!on(bad, &google), "{bad}");
        }
    }

    /// A redirect to another host is refused by the client itself.
    #[tokio::test]
    async fn a_redirect_off_the_vendor_hosts_is_not_followed() {
        let attempt_policy = pinned_client(vec!["dl.google.com"]).unwrap();
        // 127.0.0.1 is not a vendor host: a local server that redirects there
        // stands in for "vendor redirects elsewhere".
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: https://evil.example/x.deb\r\nContent-Length: 0\r\n\r\n",
                );
            }
        });
        let err = attempt_policy
            .get(format!("http://127.0.0.1:{port}/start"))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_redirect(), "{err:?}");
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8], Option<u32>)]) {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, body, link_mode) in entries {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(link_mode.unwrap_or(0o755));
            if link_mode.is_some() {
                w.add_symlink(*name, String::from_utf8_lossy(body), opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(body).unwrap();
            }
        }
        w.finish().unwrap();
    }

    #[test]
    fn zips_are_unpacked_without_leaving_the_directory() {
        let t = tempfile::tempdir().unwrap();
        let good = t.path().join("good.zip");
        write_zip(
            &good,
            &[
                ("chrome-linux64/chrome", b"#!bin", None),
                ("chrome-linux64/locales/en.pak", b"pak", None),
                ("chrome-linux64/chrome-link", b"chrome", Some(0o777)),
                ("chrome-linux64/locales/up", b"../chrome", Some(0o777)),
            ],
        );
        let out = t.path().join("x");
        unzip_contained(&good, &out).unwrap();
        assert_eq!(std::fs::read(out.join("chrome-linux64/chrome")).unwrap(), b"#!bin");
        assert_eq!(
            std::fs::read_link(out.join("chrome-linux64/chrome-link")).unwrap(),
            PathBuf::from("chrome")
        );
        check_tree_contained(&out, &out.join("chrome-linux64")).unwrap();

        let secret = t.path().join("secret");
        std::fs::write(&secret, "root only").unwrap();
        type Entry<'a> = (&'a str, &'a [u8], Option<u32>);
        let evil: [(&str, Vec<Entry>); 4] = [
            ("dotdot", vec![("../escaped", b"x", None)]),
            ("absolute link", vec![("d/l", b"/etc/passwd", Some(0o777))]),
            ("link out", vec![("d/l", b"../../secret", Some(0o777))]),
            // A link, then a file written "through" it.
            (
                "through",
                vec![("d", b"../", Some(0o777)), ("d/secret", b"overwritten", None)],
            ),
        ];
        for (what, entries) in evil {
            let z = t.path().join("evil.zip");
            write_zip(&z, &entries);
            let out = t.path().join(format!("out-{}", what.replace(' ', "-")));
            assert!(unzip_contained(&z, &out).is_err(), "{what} must be refused");
            assert!(!t.path().join("escaped").exists(), "{what}");
            assert_eq!(std::fs::read_to_string(&secret).unwrap(), "root only", "{what}");
        }
    }

    #[test]
    fn an_extracted_tree_with_a_way_out_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let outside = t.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("f"), "x").unwrap();
        let tree = |name: &str| {
            let x = t.path().join(name);
            std::fs::create_dir_all(x.join("opt/vendor/app/sub")).unwrap();
            std::fs::write(x.join("opt/vendor/app/bin"), "#!").unwrap();
            x
        };
        let ok = tree("ok");
        std::os::unix::fs::symlink("bin", ok.join("opt/vendor/app/alias")).unwrap();
        std::os::unix::fs::symlink("../bin", ok.join("opt/vendor/app/sub/up")).unwrap();
        check_tree_contained(&ok, &ok.join("opt/vendor/app")).unwrap();

        let abs = tree("abs");
        std::os::unix::fs::symlink(&outside, abs.join("opt/vendor/app/out")).unwrap();
        let rel = tree("rel");
        std::os::unix::fs::symlink("../../../../outside/f", rel.join("opt/vendor/app/sub/out")).unwrap();
        let hard = tree("hard");
        std::fs::hard_link(outside.join("f"), hard.join("opt/vendor/app/hard")).unwrap();
        // The browser directory itself is a link to somewhere else.
        let via = t.path().join("via");
        std::fs::create_dir_all(via.join("opt/vendor")).unwrap();
        std::os::unix::fs::symlink(&outside, via.join("opt/vendor/app")).unwrap();
        for (what, x) in [
            ("absolute link", abs),
            ("relative link out", rel),
            ("hard link", hard),
            ("linked tree", via),
        ] {
            let e = check_tree_contained(&x, &x.join("opt/vendor/app"))
                .unwrap_err()
                .to_string();
            assert!(e.contains("refusing the archive"), "{what}: {e}");
        }
        assert!(
            check_tree_contained(&ok, &outside).is_err(),
            "a tree outside the extraction dir"
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
