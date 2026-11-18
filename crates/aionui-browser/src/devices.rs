//! Paired devices of the owner's own browser (C34, TASK-125): one-time
//! pairing codes and per-device tokens.
//!
//! - A pairing code (8 characters from `A–Z2–9` without `I`/`O`, shown as
//!   `ABCD-2345`) lives 5 minutes in memory, is single-use, and a new code
//!   replaces the user's previous one. Dashes, spaces and case are ignored.
//! - Pairing returns a fresh device token ONCE. Only its SHA-256 is stored, in
//!   a root-only JSON file per user next to the settings (`0600`, atomic
//!   write). Revoking deletes the row; the token then never matches again.
//! - Codes, tokens, hashing and the per-source failure limit are the shared
//!   [`aionui_common::pairing`] (also used by the C38 device relay). With 32^8
//!   codes, 10 guesses per code stay negligible; the gateway also limits
//!   extension upgrades to 10 per minute per IP.
//!
//! Tokens and codes are never logged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use aionui_api_types::{BrowserErrorCode, BrowserPairingCode};
pub use aionui_common::pairing::{MAX_FAILED_PAIRS, new_device_token, normalize_pairing_code as normalize_code};
use aionui_common::pairing::{PairingCodes, RedeemOutcome, token_sha256_hex as sha256_hex};
use serde::{Deserialize, Serialize};

use crate::error::BrowserError;
use crate::filter::tokens_match;
use crate::layout::safe_user_segment;
use crate::settings::write_private_json;

pub const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_NAME: usize = 64;

/// One stored device (never the token itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDevice {
    pub device_id: String,
    pub name: String,
    pub browser: String,
    /// Hex SHA-256 of the device token.
    pub token_sha256: String,
    pub created_at: i64,
    #[serde(default)]
    pub last_seen_at: Option<i64>,
    /// The extension's self-reported version (TASK-133); display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_version: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UserFile {
    user_id: String,
    devices: Vec<StoredDevice>,
}

/// What a successful pair or hello yields.
#[derive(Debug, Clone)]
pub struct Authenticated {
    pub user_id: String,
    pub device: StoredDevice,
}

pub struct DeviceStore {
    dir: PathBuf,
    /// user id → that user's devices (loaded from `dir` at construction).
    users: std::sync::Mutex<HashMap<String, Vec<StoredDevice>>>,
    codes: PairingCodes<String>,
}

impl std::fmt::Debug for DeviceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceStore").finish_non_exhaustive()
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn clean_label(raw: &str, fallback: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect::<String>()
        .trim()
        .to_owned();
    if s.is_empty() { fallback.to_owned() } else { s }
}

fn pairing_invalid() -> BrowserError {
    BrowserError::coded(
        BrowserErrorCode::BrowserPairingInvalid,
        "the pairing code is invalid or expired",
    )
}

impl DeviceStore {
    /// Load every user's file from `dir` (unreadable files are skipped).
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let mut users = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match std::fs::read(&path).map(|b| serde_json::from_slice::<UserFile>(&b)) {
                    Ok(Ok(file)) if safe_user_segment(&file.user_id).is_ok() => {
                        users.insert(file.user_id, file.devices);
                    }
                    _ => tracing::warn!(path = %path.display(), "browser devices file unreadable; skipped"),
                }
            }
        }
        Self {
            dir,
            users: std::sync::Mutex::new(users),
            codes: PairingCodes::new(PAIRING_TTL),
        }
    }

    fn persist(&self, user_id: &str, devices: &[StoredDevice]) -> Result<(), BrowserError> {
        let path = self.dir.join(format!("{}.json", safe_user_segment(user_id)?));
        write_private_json(
            &path,
            &UserFile {
                user_id: user_id.to_owned(),
                devices: devices.to_vec(),
            },
        )
    }

    /// A new one-time code for `user_id`; the previous one stops working.
    pub fn pairing_code(&self, user_id: &str) -> Result<BrowserPairingCode, BrowserError> {
        safe_user_segment(user_id)?;
        let expires_at = now_ms() + i64::try_from(PAIRING_TTL.as_millis()).unwrap_or(0);
        let code = self.codes.issue(user_id.to_owned());
        Ok(BrowserPairingCode { code, expires_at })
    }

    /// Consume `code` and create a device. Returns the device and its token
    /// (the only time the token exists in clear outside the extension).
    pub fn pair(
        &self,
        code: &str,
        name: &str,
        browser: &str,
        source: &str,
    ) -> Result<(Authenticated, String), BrowserError> {
        let user_id = match self.codes.redeem(code, source) {
            RedeemOutcome::Paired(user_id) => Some(user_id),
            RedeemOutcome::Invalid => None,
            RedeemOutcome::Throttled => {
                tracing::warn!(source, "browser pairing refused: too many wrong codes from this source");
                None
            }
        };
        let Some(user_id) = user_id else {
            return Err(pairing_invalid());
        };
        let token = new_device_token();
        let device = StoredDevice {
            device_id: format!("dev_{}", uuid::Uuid::new_v4().simple()),
            name: clean_label(name, "Mac"),
            browser: clean_label(browser, "Chrome"),
            token_sha256: sha256_hex(&token),
            created_at: now_ms(),
            last_seen_at: Some(now_ms()),
            extension_version: None,
        };
        let mut users = self.users.lock().expect("users lock");
        let list = users.entry(user_id.clone()).or_default();
        list.push(device.clone());
        if let Err(e) = self.persist(&user_id, list) {
            list.retain(|d| d.device_id != device.device_id);
            return Err(e);
        }
        tracing::info!(user_id, device_id = %device.device_id, "browser device paired");
        Ok((Authenticated { user_id, device }, token))
    }

    /// The device a token belongs to (constant-time compare of the hashes).
    pub fn authenticate(&self, token: &str) -> Result<Authenticated, BrowserError> {
        let hash = sha256_hex(token.trim());
        let users = self.users.lock().expect("users lock");
        let mut found = None;
        for (user_id, devices) in users.iter() {
            for d in devices {
                if tokens_match(&d.token_sha256, &hash) && found.is_none() {
                    found = Some(Authenticated {
                        user_id: user_id.clone(),
                        device: d.clone(),
                    });
                }
            }
        }
        found.ok_or_else(|| BrowserError::coded(BrowserErrorCode::BrowserDeviceRevoked, "the device is not paired"))
    }

    pub fn devices(&self, user_id: &str) -> Vec<StoredDevice> {
        self.users
            .lock()
            .expect("users lock")
            .get(user_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn device(&self, user_id: &str, device_id: &str) -> Option<StoredDevice> {
        self.devices(user_id).into_iter().find(|d| d.device_id == device_id)
    }

    /// Remove a device of `user_id` (another user's device is "not found").
    pub fn revoke(&self, user_id: &str, device_id: &str) -> Result<(), BrowserError> {
        let mut users = self.users.lock().expect("users lock");
        let list = users.get_mut(user_id).ok_or_else(not_found)?;
        let before = list.len();
        let removed: Vec<StoredDevice> = list.iter().filter(|d| d.device_id == device_id).cloned().collect();
        list.retain(|d| d.device_id != device_id);
        if list.len() == before {
            return Err(not_found());
        }
        if let Err(e) = self.persist(user_id, list) {
            list.extend(removed);
            return Err(e);
        }
        tracing::info!(user_id, device_id, "browser device revoked");
        Ok(())
    }

    /// Remember the version the extension reports about itself. Anything that
    /// is not a short dotted version is ignored (the value comes from the
    /// device and is only ever displayed).
    pub fn set_extension_version(&self, user_id: &str, device_id: &str, reported: &str) {
        // Padding around the reported value is dropped here; the validator
        // itself accepts only an exact version string.
        let Some(version) = valid_extension_version(reported.trim()) else {
            tracing::warn!(
                user_id,
                device_id,
                "browser device reported an unusable extension version; ignored"
            );
            return;
        };
        let mut users = self.users.lock().expect("users lock");
        let Some(list) = users.get_mut(user_id) else { return };
        let Some(d) = list.iter_mut().find(|d| d.device_id == device_id) else {
            return;
        };
        if d.extension_version.as_deref() == Some(version) {
            return;
        }
        d.extension_version = Some(version.to_owned());
        if let Err(e) = self.persist(user_id, list) {
            tracing::warn!(user_id, device_id, error = %e, "browser device extension version not saved");
        }
    }

    /// Record a connection (persisted; called on hello and disconnect only).
    pub fn touch(&self, user_id: &str, device_id: &str) {
        let mut users = self.users.lock().expect("users lock");
        let Some(list) = users.get_mut(user_id) else { return };
        let Some(d) = list.iter_mut().find(|d| d.device_id == device_id) else {
            return;
        };
        d.last_seen_at = Some(now_ms());
        if let Err(e) = self.persist(user_id, list) {
            tracing::warn!(user_id, device_id, error = %e, "browser device last-seen not saved");
        }
    }
}

/// A dotted numeric version with an optional `-pre` / `+build` tail, at most
/// 32 characters: `1.2`, `0.1.2`, `1.0.0-beta.1`. The WHOLE string must be
/// that: surrounding whitespace or a newline is refused (callers trim what
/// they accept; the stored value is shown as is).
pub fn valid_extension_version(raw: &str) -> Option<&str> {
    let v = raw;
    if v.is_empty() || v.len() > 32 {
        return None;
    }
    let (core, tail) = match v.find(['-', '+']) {
        Some(i) => (&v[..i], Some(&v[i + 1..])),
        None => (v, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    let core_ok = (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()));
    let tail_ok = tail.is_none_or(|t| {
        !t.is_empty()
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    });
    (core_ok && tail_ok).then_some(v)
}

fn not_found() -> BrowserError {
    BrowserError::coded(BrowserErrorCode::BrowserDeviceNotFound, "no such device")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_version_is_validated_stored_and_survives_reload() {
        for ok in ["0.1.2", "1.2", "10.20.30.40", "1.0.0-beta.1", "1.0.0+build.5"] {
            assert_eq!(valid_extension_version(ok), Some(ok), "{ok}");
        }
        for bad in [
            "",
            "1",
            "v1.2.3",
            "1..2",
            "1.2.3.4.5",
            "1.2.3-",
            "1.2.3 <script>",
            "1.2.3\\n",
            "1.2\n.3",
            "9999999.1",
            "1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(valid_extension_version(bad), None, "{bad:?}");
        }
        let t = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(t.path());
        let code = store.pairing_code("u1").unwrap();
        let (auth, _token) = store.pair(&code.code, "Mac", "Chrome 157", "src").unwrap();
        let id = auth.device.device_id;
        assert_eq!(store.device("u1", &id).unwrap().extension_version, None);
        store.set_extension_version("u1", &id, "<img src=x>");
        assert_eq!(
            store.device("u1", &id).unwrap().extension_version,
            None,
            "garbage is ignored"
        );
        store.set_extension_version("u1", &id, " 0.1.2 ");
        store.set_extension_version("u2", &id, "9.9.9");
        assert_eq!(
            DeviceStore::open(t.path())
                .device("u1", &id)
                .unwrap()
                .extension_version
                .as_deref(),
            Some("0.1.2")
        );
    }

    #[test]
    fn codes_are_single_use_case_and_dash_insensitive_and_replaced() {
        let t = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(t.path());
        let first = store.pairing_code("u1").unwrap();
        assert_eq!(first.code.len(), 9);
        assert!(
            first
                .code
                .chars()
                .all(|c| c == '-' || b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(&(c as u8)))
        );
        let second = store.pairing_code("u1").unwrap();
        // The first code no longer works (unless both draws collide).
        if normalize_code(&first.code) != normalize_code(&second.code) {
            let err = store.pair(&first.code, "Mac", "Chrome 157", "src").unwrap_err();
            assert_eq!(err.code(), Some(BrowserErrorCode::BrowserPairingInvalid));
        }
        let lower = second.code.to_lowercase().replace('-', " ");
        let (auth, token) = store.pair(&lower, "Mac", "Chrome 157", "src").unwrap();
        assert_eq!(auth.user_id, "u1");
        assert_eq!(token.len(), 64);
        let again = store.pair(&second.code, "Mac", "Chrome 157", "src").unwrap_err();
        assert_eq!(again.code(), Some(BrowserErrorCode::BrowserPairingInvalid));
    }

    #[test]
    fn token_is_stored_hashed_survives_reload_and_revoke_kills_it() {
        let t = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(t.path());
        let code = store.pairing_code("u1").unwrap();
        let (auth, token) = store.pair(&code.code, "My\u{7}Mac", "", "src").unwrap();
        assert_eq!(auth.device.name, "MyMac");
        assert_eq!(auth.device.browser, "Chrome");
        let raw = std::fs::read_to_string(t.path().join("u1.json")).unwrap();
        assert!(!raw.contains(&token), "the token is never stored in clear");
        assert!(raw.contains(&sha256_hex(&token)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(t.path().join("u1.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reloaded = DeviceStore::open(t.path());
        assert_eq!(
            reloaded.authenticate(&token).unwrap().device.device_id,
            auth.device.device_id
        );
        assert_eq!(
            reloaded.authenticate("nope").unwrap_err().code(),
            Some(BrowserErrorCode::BrowserDeviceRevoked)
        );
        // Another user cannot revoke it.
        assert_eq!(
            reloaded.revoke("u2", &auth.device.device_id).unwrap_err().code(),
            Some(BrowserErrorCode::BrowserDeviceNotFound)
        );
        reloaded.revoke("u1", &auth.device.device_id).unwrap();
        assert_eq!(
            reloaded.authenticate(&token).unwrap_err().code(),
            Some(BrowserErrorCode::BrowserDeviceRevoked)
        );
        assert!(DeviceStore::open(t.path()).devices("u1").is_empty());
    }

    #[test]
    fn wrong_codes_throttle_their_source_and_never_burn_a_code() {
        let t = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(t.path());
        let u1 = store.pairing_code("u1").unwrap();
        for _ in 0..MAX_FAILED_PAIRS {
            assert!(store.pair("WRONGXXX", "Mac", "Chrome", "attacker").is_err());
        }
        let err = store.pair(&u1.code, "Mac", "Chrome", "attacker").unwrap_err();
        assert_eq!(
            err.code(),
            Some(BrowserErrorCode::BrowserPairingInvalid),
            "source throttled"
        );
        assert_eq!(store.pair(&u1.code, "Mac", "Chrome", "owner").unwrap().0.user_id, "u1");
    }
}
