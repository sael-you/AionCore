//! Camoufox as a server-browser engine (TASK-136 K6, C35 v2 option A).
//!
//! One engine version directory holds three things, all root-owned and
//! world-readable, never in the repository:
//!
//! ```text
//! engines/camoufox/<version>/
//!   camoufox-bin, libxul.so, …        the release zip as published
//!   aionui-sidecar/                   sidecar.mjs + node_modules (npm ci of the pinned lock)
//!   aionui-vnc/usr/bin/x11vnc, …      x11vnc + libvncserver1 + libvncclient1 (Debian 12)
//!   AIONUI-VERIFIED.json              what was checked
//! ```
//!
//! Verification:
//! - the zip: GitHub publishes no signature, so its sha256 is pinned here and
//!   must ALSO equal the `digest` GitHub's release API reports for the asset
//!   (a re-uploaded asset fails both); the download URL must be the release's
//!   own `browser_download_url` on github.com;
//! - the sidecar runtime: `npm ci` of the committed `package-lock.json`
//!   (sha512 integrity of every package);
//! - x11vnc: Debian's signed archive ([`crate::debian`]).
//!
//! Pins (one Playwright for sidecar and agents): Camoufox `v159.0-beta.42`
//! (Firefox 159) and `@playwright/cli` 0.1.17 = playwright-core
//! `1.62.0-alpha-1783623505000`. Camoufox pins `playwright <1.63`
//! (`typescript/package.json`, `python/pyproject.toml` in daijro/camoufox):
//! every Playwright minor may change Juggler; 1.66 fails to launch it
//! (`Browser.setDownloadOptions … legacyInterceptor`, reproduced
//! 2026-11-17). A Camoufox update is a code change, tested live.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::BrowserError;

pub const REPO: &str = "daijro/camoufox";
pub const TAG: &str = "v159.0-beta.42";
pub const VERSION: &str = "159.0-beta.42";
pub const ASSET: &str = "camoufox-159.0-beta.42-lin.x86_64.zip";
/// sha256 of [`ASSET`], read from GitHub's release API and the download on
/// 2026-11-17.
pub const ASSET_SHA256: &str = "a79ad1ef1a4575e3e0d2c1c66b13ad79ebcad15a898a7c070075e6dd849533d4";
/// The asset is ~1.3 GB (2.4 GB unpacked).
pub const ASSET_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Total time allowed for the asset download.
pub const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90 * 60);
/// The Playwright pinned for the sidecar and the agents' client.
pub const PLAYWRIGHT_PIN: &str = "@playwright/cli 0.1.17 (playwright-core 1.62.0-alpha-1783623505000)";

/// Directory names inside a Camoufox version directory.
pub const SIDECAR_DIR: &str = "aionui-sidecar";
pub const VNC_DIR: &str = "aionui-vnc";

const SIDECAR_JS: &str = include_str!("../assets/camoufox-sidecar/sidecar.mjs");
const SIDECAR_PACKAGE: &str = include_str!("../assets/camoufox-sidecar/package.json");
const SIDECAR_LOCK: &str = include_str!("../assets/camoufox-sidecar/package-lock.json");

/// The release API answer for [`TAG`].
pub fn release_api_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/tags/{TAG}")
}

/// The only URL the asset may come from.
pub fn expected_download_url() -> String {
    format!("https://github.com/{REPO}/releases/download/{TAG}/{ASSET}")
}

/// Check the release API's entry for [`ASSET`]: the pinned sha256 must equal
/// the API's digest, the URL must be the release's own.
pub fn check_release(api: &Value) -> Result<(), BrowserError> {
    let bad = |what: &str| BrowserError::Internal(format!("Camoufox release check failed: {what}"));
    if api["tag_name"].as_str() != Some(TAG) {
        return Err(bad("unexpected tag"));
    }
    let asset = api["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(ASSET)))
        .ok_or_else(|| bad("asset missing"))?;
    let digest = asset["digest"].as_str().unwrap_or("");
    if digest != format!("sha256:{ASSET_SHA256}") {
        return Err(bad("GitHub's digest differs from the pinned sha256"));
    }
    if asset["browser_download_url"].as_str() != Some(expected_download_url().as_str()) {
        return Err(bad("unexpected download URL"));
    }
    let size = asset["size"].as_u64().unwrap_or(u64::MAX);
    if size > ASSET_MAX_BYTES {
        return Err(bad("asset too large"));
    }
    Ok(())
}

/// Stream `url` into `dest`, returning (sha256 hex, size); refuses more than
/// `max` bytes. Large engine downloads never sit in memory.
pub async fn download_to(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
    max: u64,
) -> Result<(String, u64), BrowserError> {
    let mut resp = http
        .get(url)
        // The installer client's 10-minute total timeout is too short for
        // ~1.3 GB on a busy host: this request gets its own.
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|e| BrowserError::Internal(format!("download failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(BrowserError::Internal(format!("download → HTTP {}", resp.status())));
    }
    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| BrowserError::Internal(format!("download failed: {e}")))?
    {
        size += chunk.len() as u64;
        if size > max {
            return Err(BrowserError::Internal("download is too large".into()));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    Ok((hex::encode(hasher.finalize()), size))
}

/// sha256 of a file, streamed.
pub fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// The verified zip in `cache` (root-only, 0700): reused when its sha256
/// still equals the pin, else downloaded (release entry checked first) and
/// kept. A mismatching cached file is deleted and fetched again.
pub async fn cached_asset(http: &reqwest::Client, cache: &Path) -> Result<(PathBuf, bool), BrowserError> {
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        b.create(cache)?;
    }
    let path = cache.join(ASSET);
    if path.exists() {
        if file_sha256(&path)? == ASSET_SHA256 {
            return Ok((path, true));
        }
        tracing::warn!("camoufox: cached archive does not match the pinned sha256; downloading again");
        std::fs::remove_file(&path)?;
    }
    let api: Value = http
        .get(release_api_url())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| BrowserError::Internal(format!("release lookup failed: {e}")))?
        .json()
        .await
        .map_err(|e| BrowserError::Internal(format!("release lookup failed: {e}")))?;
    check_release(&api)?;
    let partial = cache.join(format!(".{ASSET}.part"));
    match download_to(http, &expected_download_url(), &partial, ASSET_MAX_BYTES).await {
        Ok((sha, _)) if sha == ASSET_SHA256 => {
            std::fs::rename(&partial, &path)?;
            Ok((path, false))
        }
        Ok(_) => {
            let _ = std::fs::remove_file(&partial);
            Err(BrowserError::Internal(
                "Camoufox download does not match the pinned sha256".into(),
            ))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Remove the cached archives (the engine's "clear cache", used by remove).
pub fn clear_cache(cache: &Path) -> std::io::Result<u64> {
    let mut freed = 0;
    for name in [ASSET.to_owned(), format!(".{ASSET}.part")] {
        let p = cache.join(name);
        if let Ok(meta) = std::fs::metadata(&p) {
            freed += meta.len();
            std::fs::remove_file(&p)?;
        }
    }
    Ok(freed)
}

/// Build the engine tree for [`VERSION`] in `x` (staging): the zip (the
/// verified cached copy when there is one), the sidecar runtime and x11vnc.
/// Returns the tree and its verification record.
pub async fn build_tree(
    http: &reqwest::Client,
    staging: &Path,
    x: &Path,
    cache: &Path,
) -> Result<(PathBuf, Value), BrowserError> {
    // 1–2. The zip: release entry checked (pinned sha256 == GitHub's
    // digest), streamed and checked against the pin — or the cached copy,
    // re-checked against the pin.
    let (zip, reused) = cached_asset(http, cache).await?;
    let size = std::fs::metadata(&zip)?.len();
    let sha = ASSET_SHA256.to_owned();
    let tree = x.join("camoufox");
    std::fs::create_dir_all(&tree)?;
    let mut cmd = aionui_runtime::Builder::clean_cli("unzip");
    cmd.arg("-q").arg(&zip).arg("-d").arg(&tree);
    if !cmd.output().await?.status.success() {
        return Err(BrowserError::Internal("unzip failed".into()));
    }
    // 3. The sidecar runtime.
    let lock = install_sidecar(staging, &tree.join(SIDECAR_DIR)).await?;
    // 4. x11vnc for the panel.
    let debs = crate::debian::install_vnc(http, staging, &tree.join(VNC_DIR)).await?;
    let record = json!({
        "engine": "camoufox",
        "version": VERSION,
        "verification": "pinned-sha256",
        "checked": "sha256 pinned in AionCore == GitHub release API digest == sha256 of the download; URL = the release's browser_download_url",
        "not_checked": "Camoufox publishes no signature",
        "source": expected_download_url(),
        "from_cache": reused,
        "sha256": sha,
        "size": size,
        "sidecar": { "pin": PLAYWRIGHT_PIN, "packages": lock },
        "vnc": debs,
    });
    Ok((tree, record))
}

/// `npm ci --ignore-scripts` of the committed lock into `dir`; returns the
/// locked packages with their integrity.
async fn install_sidecar(staging: &Path, dir: &Path) -> Result<Value, BrowserError> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("package.json"), SIDECAR_PACKAGE)?;
    std::fs::write(dir.join("package-lock.json"), SIDECAR_LOCK)?;
    std::fs::write(dir.join("sidecar.mjs"), SIDECAR_JS)?;
    let home = staging.join("npm-home");
    std::fs::create_dir_all(&home)?;
    let mut cmd = aionui_runtime::Builder::clean_cli("npm");
    cmd.arg("ci")
        .arg("--ignore-scripts")
        .arg("--omit=optional")
        .arg("--no-audit")
        .arg("--no-fund")
        .current_dir(dir)
        .env("HOME", &home)
        .env("npm_config_cache", staging.join("npm-cache"))
        .env("npm_config_update_notifier", "false");
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(BrowserError::Internal("npm ci of the Camoufox sidecar failed".into()));
    }
    if !dir.join("node_modules/playwright-core/package.json").exists() {
        return Err(BrowserError::Internal("sidecar runtime is incomplete".into()));
    }
    locked_packages(SIDECAR_LOCK)
}

/// `name → {version, integrity}` of the lock's packages.
pub fn locked_packages(lock: &str) -> Result<Value, BrowserError> {
    let v: Value = serde_json::from_str(lock).map_err(|e| BrowserError::Internal(e.to_string()))?;
    let mut out = serde_json::Map::new();
    if let Some(pkgs) = v["packages"].as_object() {
        for (path, p) in pkgs {
            if let Some(name) = path.strip_prefix("node_modules/") {
                out.insert(
                    name.to_owned(),
                    json!({ "version": p["version"], "integrity": p["integrity"] }),
                );
            }
        }
    }
    Ok(Value::Object(out))
}

/// Paths inside an installed Camoufox version directory.
#[derive(Debug, Clone)]
pub struct CamoufoxPaths {
    pub binary: PathBuf,
    pub sidecar: PathBuf,
    pub x11vnc: PathBuf,
    pub vnc_libs: PathBuf,
}

impl CamoufoxPaths {
    pub fn new(version_dir: &Path) -> Self {
        let vnc = version_dir.join(VNC_DIR);
        Self {
            binary: version_dir.join("camoufox-bin"),
            sidecar: version_dir.join(SIDECAR_DIR).join("sidecar.mjs"),
            x11vnc: vnc.join("usr/bin/x11vnc"),
            vnc_libs: vnc.join("usr/lib/x86_64-linux-gnu"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(digest: &str, url: &str) -> Value {
        json!({"tag_name": TAG, "assets": [
            {"name": "camoufox-159.0-beta.42-lin.arm64.zip", "digest": "sha256:00", "browser_download_url": "x", "size": 1},
            {"name": ASSET, "digest": digest, "browser_download_url": url, "size": 1_313_639_575u64},
        ]})
    }

    #[test]
    fn release_check_needs_the_pinned_digest_and_the_release_url() {
        let pinned = format!("sha256:{ASSET_SHA256}");
        assert!(check_release(&api(&pinned, &expected_download_url())).is_ok());
        assert!(
            check_release(&api("sha256:beef", &expected_download_url())).is_err(),
            "digest changed"
        );
        assert!(
            check_release(&api(&pinned, "https://evil.example/x.zip")).is_err(),
            "url changed"
        );
        let mut other_tag = api(&pinned, &expected_download_url());
        other_tag["tag_name"] = json!("v160");
        assert!(check_release(&other_tag).is_err());
        assert!(check_release(&json!({"tag_name": TAG, "assets": []})).is_err());
    }

    #[test]
    fn the_lock_pins_one_playwright_for_sidecar_and_agents() {
        let pkgs = locked_packages(SIDECAR_LOCK).unwrap();
        assert_eq!(pkgs["@playwright/cli"]["version"], "0.1.17");
        assert_eq!(pkgs["playwright-core"]["version"], "1.62.0-alpha-1783623505000");
        assert_eq!(pkgs["playwright"]["version"], "1.62.0-alpha-1783623505000");
        for (name, p) in pkgs.as_object().unwrap() {
            assert!(p["integrity"].as_str().unwrap_or("").starts_with("sha512-"), "{name}");
        }
        assert!(SIDECAR_JS.contains("allow_hijacking_localhost") && SIDECAR_JS.contains("browser.bind("));
    }

    #[test]
    fn the_cache_is_clearable() {
        let t = tempfile::tempdir().unwrap();
        let cache = t.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(ASSET), b"0123456789").unwrap();
        std::fs::write(cache.join(format!(".{ASSET}.part")), b"01234").unwrap();
        assert_eq!(clear_cache(&cache).unwrap(), 15);
        assert!(!cache.join(ASSET).exists());
        assert_eq!(clear_cache(&cache).unwrap(), 0, "nothing left");
    }

    #[tokio::test]
    async fn a_cached_archive_with_the_wrong_sha_is_dropped_and_the_dir_is_root_only() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let cache = t.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(ASSET), b"tampered").unwrap();
        // The re-download cannot succeed here (no route); the tampered file
        // must be gone before it is attempted.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .unwrap();
        let _ = cached_asset(&http, &cache).await;
        assert!(!cache.join(ASSET).exists());
        let fresh = t.path().join("fresh");
        let _ = cached_asset(&http, &fresh).await;
        assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn paths_inside_a_version_dir() {
        let p = CamoufoxPaths::new(Path::new("/e/camoufox/159.0-beta.42"));
        assert_eq!(p.binary, Path::new("/e/camoufox/159.0-beta.42/camoufox-bin"));
        assert_eq!(
            p.sidecar,
            Path::new("/e/camoufox/159.0-beta.42/aionui-sidecar/sidecar.mjs")
        );
        assert_eq!(
            p.x11vnc,
            Path::new("/e/camoufox/159.0-beta.42/aionui-vnc/usr/bin/x11vnc")
        );
    }
}
