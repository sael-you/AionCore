//! x11vnc for the Camoufox panel (TASK-136 K6), from Debian 12's signed
//! archive, unpacked into the engine's own directory (the container image
//! has Xvfb but no VNC server, and system packages vanish with the image).
//!
//! Chain (run by hand 2026-11-17):
//! 1. Debian's archive keyrings from the image (`/usr/share/keyrings`,
//!    package debian-archive-keyring) → `gpg --import` into a throw-away home
//!    → BOTH pinned primary fingerprints present (Debian lists them on
//!    ftp-master.debian.org/keys.html: bookworm archive signing key and
//!    bookworm release key; InRelease carries one signature of each, and
//!    gpgv needs both keys to exit 0) → exported as the only keys;
//! 2. `gpgv --status-fd` on `dists/bookworm/InRelease`: exit 0 and a
//!    `VALIDSIG` whose primary key is pinned;
//! 3. the Release's SHA256 of `main/binary-amd64/Packages.xz` =
//!    sha256(download) → `xz -dc`;
//!    The Release must not be a replay: its `Date` may not be older than the
//!    newest one accepted on this host, and a `Valid-Until` must not have
//!    passed ([`check_freshness`]);
//! 4. each package's stanza: SHA256 + Size = the downloaded `.deb` →
//!    `dpkg-deb -x` (no maintainer scripts).
//!
//! Packages: x11vnc, libvncserver1, libvncclient1 (their other dependencies
//! are in the image: checked with `ldd` against the image's libraries).

use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::BrowserError;
use crate::install::{packages_entry, primary_fingerprints, release_sha256};

pub const DEBIAN_REPO: &str = "https://deb.debian.org/debian";
pub const SUITE: &str = "bookworm";
/// Debian 12/bookworm archive signing key (primary).
pub const ARCHIVE_FPR: &str = "B8B80B5B623EAB6AD8775C45B7C5D7D6350947F8";
/// Debian 12/bookworm release key.
pub const RELEASE_FPR: &str = "4D64FEC119C2029067D6E791F8D2585B8783D481";
const KEYRINGS: [&str; 2] = [
    "/usr/share/keyrings/debian-archive-bookworm-automatic.gpg",
    "/usr/share/keyrings/debian-archive-bookworm-stable.gpg",
];
const PACKAGES_XZ: &str = "main/binary-amd64/Packages.xz";
pub const VNC_PACKAGES: [&str; 3] = ["x11vnc", "libvncserver1", "libvncclient1"];
const MAX_DEB_BYTES: u64 = 32 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn internal(what: &str) -> BrowserError {
    BrowserError::Internal(what.to_owned())
}

/// apt's rule for a multi-signed InRelease: trusted when a pinned key made a
/// `VALIDSIG` and no signature is bad, expired or revoked. A signature by a
/// key we do not hold (`NO_PUBKEY`/`ERRSIG`, e.g. a newer archive key Debian
/// adds) does not count either way; gpgv then exits 2.
pub fn release_trusted(status: &str, exit_code: Option<i32>) -> bool {
    let bad = status.lines().any(|l| {
        ["BADSIG", "EXPSIG", "EXPKEYSIG", "REVKEYSIG"]
            .iter()
            .any(|k| l.starts_with(&format!("[GNUPG:] {k} ")))
    });
    let pinned = valid_signers(status)
        .iter()
        .any(|f| f == ARCHIVE_FPR || f == RELEASE_FPR);
    matches!(exit_code, Some(0 | 2)) && pinned && !bad
}

/// `VALIDSIG` lines of `gpgv --status-fd`: the primary-key fingerprint is
/// the last field.
pub fn valid_signers(status: &str) -> Vec<String> {
    status
        .lines()
        .filter_map(|l| l.strip_prefix("[GNUPG:] VALIDSIG "))
        .filter_map(|rest| rest.split_whitespace().last().map(str::to_owned))
        .collect()
}

async fn gpg(home: &Path, args: &[&str]) -> Result<std::process::Output, BrowserError> {
    let mut cmd = aionui_runtime::Builder::clean_cli("gpg");
    cmd.arg("--homedir")
        .arg(home)
        .arg("--batch")
        .arg("--no-tty")
        .arg("--no-autostart")
        .args(args);
    Ok(cmd.output().await?)
}

/// A gpgv keyring holding exactly the two pinned Debian keys.
async fn pinned_keyring(staging: &Path) -> Result<std::path::PathBuf, BrowserError> {
    let home = staging.join("debian-gnupg");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&home)?;
    }
    for keyring in KEYRINGS {
        // gpg exits 2 after a successful import when it cannot reach an
        // agent (`--no-autostart`, or a staging path too long for its
        // socket); the keys are in. The pinned-fingerprint check below is
        // what decides.
        let out = gpg(&home, &["--import", keyring]).await?;
        if !matches!(out.status.code(), Some(0 | 2)) {
            return Err(internal("Debian keyring import failed"));
        }
    }
    let listing = gpg(&home, &["--with-colons", "--fingerprint"]).await?;
    let fprs = primary_fingerprints(&String::from_utf8_lossy(&listing.stdout));
    for pinned in [ARCHIVE_FPR, RELEASE_FPR] {
        if !fprs.iter().any(|f| f == pinned) {
            return Err(internal("Debian signing key fingerprint mismatch"));
        }
    }
    let keyring = staging.join("debian.gpg");
    let out = gpg(
        &home,
        &[
            "--export",
            "--output",
            &keyring.to_string_lossy(),
            ARCHIVE_FPR,
            RELEASE_FPR,
        ],
    )
    .await?;
    if !out.status.success() || !keyring.exists() {
        return Err(internal("gpg --export failed"));
    }
    Ok(keyring)
}

async fn get(http: &reqwest::Client, url: &str, max: u64) -> Result<Vec<u8>, BrowserError> {
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| BrowserError::Internal(format!("download failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(BrowserError::Internal(format!("{url} → HTTP {}", resp.status())));
    }
    if resp.content_length().unwrap_or(0) > max {
        return Err(internal("download is too large"));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| BrowserError::Internal(format!("download failed: {e}")))?;
    if body.len() as u64 > max {
        return Err(internal("download is too large"));
    }
    Ok(body.to_vec())
}

/// Verify and unpack x11vnc and its two missing libraries into `dest`.
/// Seconds since the epoch of an RFC 2822 date as Debian writes it in a
/// Release file (`Sat, 11 Jul 2026 10:14:05 UTC`).
pub fn parse_release_date(raw: &str) -> Option<i64> {
    let rest = raw.split_once(", ").map_or(raw, |(_, r)| r);
    let mut f = rest.split_whitespace();
    let day: i64 = f.next()?.parse().ok()?;
    let month = match f.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = f.next()?.parse().ok()?;
    let mut hms = f.next()?.split(':');
    let (h, m, sec): (i64, i64, i64) = (
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
    );
    if !matches!(f.next(), Some("UTC" | "GMT" | "+0000")) || !(1..=31).contains(&day) || year < 2000 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + m * 60 + sec)
}

fn release_field<'a>(release: &'a str, name: &str) -> Option<&'a str> {
    release
        .lines()
        .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(": ")))
        .map(str::trim)
}

/// A signed Release must not be a replay: its `Date` may not be older than
/// the newest one this host has accepted (kept in `state`, root-only), and
/// a `Valid-Until`, when present, must lie in the future. Returns the date
/// to remember.
pub fn check_freshness(release: &str, last_seen: Option<i64>, now: i64) -> Result<i64, BrowserError> {
    let date = release_field(release, "Date")
        .and_then(parse_release_date)
        .ok_or_else(|| internal("Debian Release has no usable Date"))?;
    if let Some(until) = release_field(release, "Valid-Until") {
        let until = parse_release_date(until).ok_or_else(|| internal("Debian Release has a bad Valid-Until"))?;
        if until < now {
            return Err(internal("Debian Release has expired (Valid-Until passed)"));
        }
    }
    if last_seen.is_some_and(|seen| date < seen) {
        return Err(internal("Debian Release is older than the last one accepted (replay?)"));
    }
    Ok(date)
}

const LAST_RELEASE_FILE: &str = "debian-bookworm-release-date";

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `state`: a root-only directory that remembers the newest Release date.
pub async fn install_vnc(
    http: &reqwest::Client,
    staging: &Path,
    dest: &Path,
    state: &Path,
) -> Result<Value, BrowserError> {
    let keyring = pinned_keyring(staging).await?;
    let in_release = get(http, &format!("{DEBIAN_REPO}/dists/{SUITE}/InRelease"), MAX_INDEX_BYTES).await?;
    let signed = staging.join("debian-InRelease");
    let release_path = staging.join("debian-Release");
    std::fs::write(&signed, &in_release)?;
    let mut cmd = aionui_runtime::Builder::clean_cli("gpgv");
    cmd.arg("--status-fd")
        .arg("1")
        .arg("--keyring")
        .arg(&keyring)
        .arg("--output")
        .arg(&release_path)
        .arg(&signed);
    let out = cmd.output().await?;
    if !release_trusted(&String::from_utf8_lossy(&out.stdout), out.status.code()) || !release_path.exists() {
        return Err(internal("Debian InRelease signature did not verify"));
    }
    let release = std::fs::read_to_string(&release_path)?;
    let seen_file = state.join(LAST_RELEASE_FILE);
    let last_seen = std::fs::read_to_string(&seen_file)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    let release_date = check_freshness(&release, last_seen, now_secs())?;
    let want = release_sha256(&release, PACKAGES_XZ).ok_or_else(|| internal("Release has no Packages.xz hash"))?;
    let xz = get(
        http,
        &format!("{DEBIAN_REPO}/dists/{SUITE}/{PACKAGES_XZ}"),
        MAX_INDEX_BYTES,
    )
    .await?;
    if sha256_hex(&xz) != want {
        return Err(internal("Packages.xz does not match the signed Release"));
    }
    let xz_path = staging.join("Packages.xz");
    std::fs::write(&xz_path, &xz)?;
    let mut cmd = aionui_runtime::Builder::clean_cli("xz");
    cmd.arg("-dc").arg(&xz_path);
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(internal("xz -dc failed"));
    }
    let packages = String::from_utf8_lossy(&out.stdout).into_owned();
    std::fs::create_dir_all(dest)?;
    let mut record = Vec::new();
    for name in VNC_PACKAGES {
        let entry =
            packages_entry(&packages, name).ok_or_else(|| BrowserError::Internal(format!("{name} not in Packages")))?;
        if entry.size > MAX_DEB_BYTES || !entry.filename.starts_with("pool/main/") || entry.filename.contains("..") {
            return Err(internal("refusing an unexpected package entry"));
        }
        let deb = get(http, &format!("{DEBIAN_REPO}/{}", entry.filename), MAX_DEB_BYTES).await?;
        if deb.len() as u64 != entry.size || sha256_hex(&deb) != entry.sha256.to_ascii_lowercase() {
            return Err(internal(".deb does not match the signed Packages entry"));
        }
        let deb_path = staging.join(format!("{name}.deb"));
        std::fs::write(&deb_path, &deb)?;
        let mut cmd = aionui_runtime::Builder::clean_cli("dpkg-deb");
        cmd.arg("-x").arg(&deb_path).arg(dest);
        if !cmd.output().await?.status.success() {
            return Err(internal("dpkg-deb -x failed"));
        }
        record.push(json!({"package": name, "version": entry.version, "sha256": entry.sha256, "size": entry.size}));
    }
    std::fs::create_dir_all(state)?;
    std::fs::write(&seen_file, release_date.to_string())?;
    Ok(json!({
        "release_date": release_date,
        "verification": "signed-repository",
        "checked": "pinned Debian keys -> gpgv InRelease -> Packages.xz sha256 -> package sha256 + size",
        "signing_keys": [ARCHIVE_FPR, RELEASE_FPR],
        "source": format!("{DEBIAN_REPO} {SUITE} main"),
        "packages": record,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_dates_parse_and_replays_are_refused() {
        // 2026-07-11 10:14:05 UTC.
        assert_eq!(parse_release_date("Sat, 11 Jul 2026 10:14:05 UTC"), Some(1_783_764_845));
        assert_eq!(
            parse_release_date("Thu, 01 Jan 1970 00:00:00 UTC"),
            None,
            "implausible year"
        );
        assert_eq!(parse_release_date("Sat, 11 Jul 2026 10:14:05 PST"), None);
        assert_eq!(parse_release_date("yesterday"), None);
        let release = "Origin: Debian\nDate: Sat, 11 Jul 2026 10:14:05 UTC\nSHA256:\n";
        let date = check_freshness(release, None, 1_800_000_000).unwrap();
        assert_eq!(date, 1_783_764_845);
        assert!(
            check_freshness(release, Some(date), 1_800_000_000).is_ok(),
            "same Release again"
        );
        assert!(
            check_freshness(release, Some(date + 1), 1_800_000_000).is_err(),
            "older than last seen"
        );
        assert!(check_freshness("Origin: Debian\n", None, 0).is_err(), "no Date");
        let dated = "Date: Sat, 11 Jul 2026 10:14:05 UTC\nValid-Until: Sat, 18 Jul 2026 10:14:05 UTC\n";
        assert!(check_freshness(dated, None, 1_783_764_845 + 86_400).is_ok());
        assert!(
            check_freshness(dated, None, 1_783_764_845 + 8 * 86_400).is_err(),
            "expired"
        );
    }

    #[test]
    fn validsig_primary_is_the_last_field() {
        let status = "[GNUPG:] GOODSIG 6ED0E7B82643E131 Debian Archive Automatic Signing Key\n\
            [GNUPG:] VALIDSIG 4CB50190207B4758A3F73A796ED0E7B82643E131 2026-07-11 1783765031 0 4 0 1 8 01 B8B80B5B623EAB6AD8775C45B7C5D7D6350947F8\n\
            [GNUPG:] VALIDSIG 4D64FEC119C2029067D6E791F8D2585B8783D481 2026-07-11 1783765141 0 4 0 22 8 01 4D64FEC119C2029067D6E791F8D2585B8783D481\n";
        assert_eq!(
            valid_signers(status),
            vec![ARCHIVE_FPR.to_owned(), RELEASE_FPR.to_owned()]
        );
        assert!(valid_signers("[GNUPG:] BADSIG x").is_empty());
    }

    // Real gpgv output on Debian's bookworm InRelease, 2026-11-17: two pinned
    // signatures and one by a key the image does not carry.
    const STATUS: &str = "[GNUPG:] VALIDSIG 4CB50190207B4758A3F73A796ED0E7B82643E131 2026-07-11 1783765031 0 4 0 1 8 01 B8B80B5B623EAB6AD8775C45B7C5D7D6350947F8\n\
        [GNUPG:] ERRSIG 78DBA3BC47EF2265 1 8 01 1783765032 9 B8E5F13176D2A7A75220028078DBA3BC47EF2265\n\
        [GNUPG:] NO_PUBKEY 78DBA3BC47EF2265\n\
        [GNUPG:] VALIDSIG 4D64FEC119C2029067D6E791F8D2585B8783D481 2026-07-11 1783765141 0 4 0 22 8 01 4D64FEC119C2029067D6E791F8D2585B8783D481\n";

    #[test]
    fn a_pinned_good_signature_is_enough_but_a_bad_one_is_fatal() {
        assert!(release_trusted(STATUS, Some(2)), "unknown extra signer tolerated");
        assert!(release_trusted(STATUS, Some(0)));
        assert!(!release_trusted(STATUS, Some(1)), "gpgv error");
        assert!(!release_trusted(STATUS, None));
        let bad = format!("{STATUS}[GNUPG:] BADSIG 6ED0E7B82643E131 Debian\n");
        assert!(!release_trusted(&bad, Some(2)), "a bad signature is fatal");
        let expired = format!("{STATUS}[GNUPG:] EXPKEYSIG F8D2585B8783D481 Debian\n");
        assert!(!release_trusted(&expired, Some(2)));
        assert!(
            !release_trusted("[GNUPG:] NO_PUBKEY 78DBA3BC47EF2265\n", Some(2)),
            "no pinned signature"
        );
    }
}
