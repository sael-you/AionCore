//! Paired devices of the owner's own browser (C34, TASK-125): one-time
//! pairing codes and per-device tokens.
//!
//! - A pairing code (8 characters from `A–Z2–9` without `I`/`O`, shown as
//!   `ABCD-2345`) lives 5 minutes in memory, is single-use, and a new code
//!   replaces the user's previous one. Dashes, spaces and case are ignored.
//! - Pairing returns a fresh device token ONCE. Only its SHA-256 is stored, in
//!   a root-only JSON file per user next to the settings (`0600`, atomic
//!   write). Revoking deletes the row; the token then never matches again.
//! - Failed pairing attempts are counted globally (the gateway hides client
//!   addresses): more than [`MAX_FAILED_PAIRS`] per minute refuses every pair
//!   attempt for the rest of that minute.
//!
//! Tokens and codes are never logged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use aionui_api_types::{BrowserErrorCode, BrowserPairingCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::BrowserError;
use crate::filter::tokens_match;
use crate::layout::safe_user_segment;
use crate::settings::write_private_json;

pub const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);
pub const MAX_FAILED_PAIRS: u32 = 10;
const FAILED_WINDOW: Duration = Duration::from_secs(60);
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
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

struct PendingCode {
    code: String,
    expires: Instant,
}

#[derive(Default)]
struct Failures {
    window_start: Option<Instant>,
    count: u32,
}

pub struct DeviceStore {
    dir: PathBuf,
    /// user id → that user's devices (loaded from `dir` at construction).
    users: std::sync::Mutex<HashMap<String, Vec<StoredDevice>>>,
    codes: std::sync::Mutex<HashMap<String, PendingCode>>,
    failures: std::sync::Mutex<Failures>,
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

fn sha256_hex(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    let mut filled = 0;
    while filled < N {
        let chunk = uuid::Uuid::new_v4().into_bytes();
        // Bytes 6 and 8 carry the version / variant bits.
        for (i, b) in chunk.iter().enumerate() {
            if i == 6 || i == 8 || filled == N {
                continue;
            }
            out[filled] = *b;
            filled += 1;
        }
    }
    out
}

fn new_code() -> String {
    random_bytes::<8>()
        .iter()
        .map(|b| char::from(CODE_ALPHABET[usize::from(*b) % CODE_ALPHABET.len()]))
        .collect()
}

/// `abcd-2345` / `ABCD 2345` → `ABCD2345`.
pub fn normalize_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t'))
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn display_code(code: &str) -> String {
    if code.len() == 8 {
        format!("{}-{}", &code[..4], &code[4..])
    } else {
        code.to_owned()
    }
}

pub fn new_device_token() -> String {
    hex::encode(random_bytes::<32>())
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
            codes: std::sync::Mutex::new(HashMap::new()),
            failures: std::sync::Mutex::new(Failures::default()),
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
        let code = new_code();
        let expires_at = now_ms() + i64::try_from(PAIRING_TTL.as_millis()).unwrap_or(0);
        self.codes.lock().expect("codes lock").insert(
            user_id.to_owned(),
            PendingCode {
                code: code.clone(),
                expires: Instant::now() + PAIRING_TTL,
            },
        );
        Ok(BrowserPairingCode {
            code: display_code(&code),
            expires_at,
        })
    }

    /// Whether pairing is locked after too many failures (and count one more
    /// attempt when `fail` is set).
    fn too_many_failures(&self, fail: bool) -> bool {
        let mut f = self.failures.lock().expect("failures lock");
        let now = Instant::now();
        if f.window_start.is_none_or(|s| now.duration_since(s) > FAILED_WINDOW) {
            f.window_start = Some(now);
            f.count = 0;
        }
        if fail {
            f.count = f.count.saturating_add(1);
        }
        f.count >= MAX_FAILED_PAIRS
    }

    /// Consume `code` and create a device. Returns the device and its token
    /// (the only time the token exists in clear outside the extension).
    pub fn pair(&self, code: &str, name: &str, browser: &str) -> Result<(Authenticated, String), BrowserError> {
        if self.too_many_failures(false) {
            return Err(pairing_invalid());
        }
        let wanted = normalize_code(code);
        let user_id = {
            let mut codes = self.codes.lock().expect("codes lock");
            let now = Instant::now();
            codes.retain(|_, p| p.expires > now);
            let hit = codes
                .iter()
                .find(|(_, p)| tokens_match(&p.code, &wanted))
                .map(|(u, _)| u.clone());
            if let Some(u) = &hit {
                codes.remove(u);
            }
            hit
        };
        let Some(user_id) = user_id else {
            self.too_many_failures(true);
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

fn not_found() -> BrowserError {
    BrowserError::coded(BrowserErrorCode::BrowserDeviceNotFound, "no such device")
}

#[cfg(test)]
mod tests {
    use super::*;

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
                .all(|c| c == '-' || CODE_ALPHABET.contains(&(c as u8)))
        );
        let second = store.pairing_code("u1").unwrap();
        // The first code no longer works (unless both draws collide).
        if normalize_code(&first.code) != normalize_code(&second.code) {
            let err = store.pair(&first.code, "Mac", "Chrome 157").unwrap_err();
            assert_eq!(err.code(), Some(BrowserErrorCode::BrowserPairingInvalid));
        }
        let lower = second.code.to_lowercase().replace('-', " ");
        let (auth, token) = store.pair(&lower, "Mac", "Chrome 157").unwrap();
        assert_eq!(auth.user_id, "u1");
        assert_eq!(token.len(), 64);
        let again = store.pair(&second.code, "Mac", "Chrome 157").unwrap_err();
        assert_eq!(again.code(), Some(BrowserErrorCode::BrowserPairingInvalid));
    }

    #[test]
    fn token_is_stored_hashed_survives_reload_and_revoke_kills_it() {
        let t = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(t.path());
        let code = store.pairing_code("u1").unwrap();
        let (auth, token) = store.pair(&code.code, "My\u{7}Mac", "").unwrap();
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
    fn too_many_wrong_codes_lock_pairing() {
        let t = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(t.path());
        for _ in 0..MAX_FAILED_PAIRS {
            assert!(store.pair("WRONGXXX", "Mac", "Chrome").is_err());
        }
        let code = store.pairing_code("u1").unwrap();
        let err = store.pair(&code.code, "Mac", "Chrome").unwrap_err();
        assert_eq!(
            err.code(),
            Some(BrowserErrorCode::BrowserPairingInvalid),
            "locked for the minute"
        );
    }
}
