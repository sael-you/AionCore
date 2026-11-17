//! Pairing codes and device tokens shared by every outbound client AionCore
//! pairs with (C34 browser extension, C38 Mac helper / Android companion app).
//!
//! - A pairing code: 8 characters from `A–Z2–9` without `I`/`O`, shown as
//!   `ABCD-2345`, one use, a few minutes, and a new code for the same key
//!   replaces the old one. Dashes, spaces and case are ignored.
//! - Wrong attempts are counted per source address; a source with
//!   [`MAX_FAILED_PAIRS`] misses in ten minutes is refused for the rest of the
//!   window. A miss never touches a pending code.
//! - A device token: 32 random bytes (hex). Only its SHA-256 is stored;
//!   compare hashes with [`crate::tokens_match`]-style constant time.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub const MAX_FAILED_PAIRS: u32 = 10;
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::getrandom(&mut out).expect("the OS random source is available");
    out
}

/// A fresh 8-character code (unbiased: 256 % 32 == 0).
pub fn new_pairing_code() -> String {
    random_bytes::<8>()
        .iter()
        .map(|b| char::from(CODE_ALPHABET[usize::from(*b) % CODE_ALPHABET.len()]))
        .collect()
}

/// `abcd-2345` / `ABCD 2345` → `ABCD2345`.
pub fn normalize_pairing_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t'))
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// `ABCD2345` → `ABCD-2345`.
pub fn display_pairing_code(code: &str) -> String {
    if code.len() == 8 {
        format!("{}-{}", &code[..4], &code[4..])
    } else {
        code.to_owned()
    }
}

/// 32 random bytes, hex (64 characters).
pub fn new_device_token() -> String {
    to_hex(&random_bytes::<32>())
}

/// Hex SHA-256 of a token: what is stored and compared.
pub fn token_sha256_hex(token: &str) -> String {
    to_hex(&Sha256::digest(token.trim().as_bytes()))
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Constant-time equality (no early exit); never true for empty input.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0 && !a.is_empty()
}

struct Pending {
    code: String,
    expires: Instant,
}

#[derive(Default)]
struct SourceFailures {
    window_start: Option<Instant>,
    count: u32,
}

/// In-memory pending codes, one per key (a user, or a user + client kind).
///
/// Wrong attempts are counted per SOURCE (the client address the gateway
/// forwards): a source with [`MAX_FAILED_PAIRS`] misses within
/// [`FAILED_WINDOW`] is refused for the rest of that window, right code or
/// not. A miss never touches anyone's pending code (train-10 review: with
/// "a miss counts against every pending code", ten wrong guesses from anyone
/// burned every owner's code).
pub struct PairingCodes<K> {
    ttl: Duration,
    codes: std::sync::Mutex<HashMap<K, Pending>>,
    failures: std::sync::Mutex<HashMap<String, SourceFailures>>,
}

impl<K> std::fmt::Debug for PairingCodes<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingCodes").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedeemOutcome<K> {
    Paired(K),
    Invalid,
    /// This source made too many wrong attempts; try again later.
    Throttled,
}

pub const FAILED_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_TRACKED_SOURCES: usize = 10_000;

impl<K: Clone + Eq + Hash> PairingCodes<K> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            codes: std::sync::Mutex::new(HashMap::new()),
            failures: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// A new code for `key` (the display form); the previous one stops working.
    pub fn issue(&self, key: K) -> String {
        let code = new_pairing_code();
        self.codes.lock().expect("pairing codes lock").insert(
            key,
            Pending {
                code: code.clone(),
                expires: Instant::now() + self.ttl,
            },
        );
        display_pairing_code(&code)
    }

    fn source_blocked(&self, source: &str, record_miss: bool) -> bool {
        let mut failures = self.failures.lock().expect("pairing failures lock");
        let now = Instant::now();
        if failures.len() > MAX_TRACKED_SOURCES {
            failures.retain(|_, f| f.window_start.is_some_and(|s| now.duration_since(s) < FAILED_WINDOW));
        }
        let entry = failures.entry(source.to_owned()).or_default();
        if entry
            .window_start
            .is_none_or(|s| now.duration_since(s) >= FAILED_WINDOW)
        {
            entry.window_start = Some(now);
            entry.count = 0;
        }
        if record_miss {
            entry.count = entry.count.saturating_add(1);
        }
        entry.count >= MAX_FAILED_PAIRS
    }

    /// Consume `raw` (any spelling) for the client at `source`.
    pub fn redeem(&self, raw: &str, source: &str) -> RedeemOutcome<K> {
        if self.source_blocked(source, false) {
            return RedeemOutcome::Throttled;
        }
        let wanted = normalize_pairing_code(raw);
        let hit = {
            let mut codes = self.codes.lock().expect("pairing codes lock");
            let now = Instant::now();
            codes.retain(|_, p| p.expires > now);
            let hit = codes
                .iter()
                .find(|(_, p)| constant_time_eq(&p.code, &wanted))
                .map(|(k, _)| k.clone());
            if let Some(key) = &hit {
                codes.remove(key);
            }
            hit
        };
        match hit {
            Some(key) => RedeemOutcome::Paired(key),
            None => {
                self.source_blocked(source, true);
                RedeemOutcome::Invalid
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_one_use_any_spelling_and_replaced() {
        let codes: PairingCodes<&str> = PairingCodes::new(Duration::from_secs(60));
        let first = codes.issue("u1");
        assert_eq!(first.len(), 9);
        assert!(first.chars().all(|c| c == '-' || CODE_ALPHABET.contains(&(c as u8))));
        let second = codes.issue("u1");
        if normalize_pairing_code(&first) != normalize_pairing_code(&second) {
            assert_eq!(codes.redeem(&first, "a"), RedeemOutcome::Invalid, "replaced");
        }
        let spelled = second.to_lowercase().replace('-', " ");
        assert_eq!(codes.redeem(&spelled, "a"), RedeemOutcome::Paired("u1"));
        assert_eq!(codes.redeem(&second, "a"), RedeemOutcome::Invalid, "one use");
    }

    /// Train-10 review: wrong guesses throttle their SOURCE only; nobody's
    /// pending code is burned and another source still pairs.
    #[test]
    fn misses_throttle_the_source_and_never_burn_codes() {
        let codes: PairingCodes<&str> = PairingCodes::new(Duration::from_secs(60));
        let u1 = codes.issue("u1");
        for _ in 0..MAX_FAILED_PAIRS {
            assert_eq!(codes.redeem("WRONGXXX", "attacker"), RedeemOutcome::Invalid);
        }
        assert_eq!(codes.redeem("WRONGXXX", "attacker"), RedeemOutcome::Throttled);
        assert_eq!(
            codes.redeem(&u1, "attacker"),
            RedeemOutcome::Throttled,
            "a throttled source is refused even with a right code"
        );
        assert_eq!(
            codes.redeem(&u1, "owner"),
            RedeemOutcome::Paired("u1"),
            "the code survived"
        );
    }

    #[test]
    fn expired_codes_do_not_pair() {
        let codes: PairingCodes<&str> = PairingCodes::new(Duration::from_millis(0));
        let code = codes.issue("u1");
        assert_eq!(codes.redeem(&code, "a"), RedeemOutcome::Invalid);
    }

    #[test]
    fn tokens_hash_and_compare() {
        let t = new_device_token();
        assert_eq!(t.len(), 64);
        assert_ne!(t, new_device_token());
        assert_eq!(token_sha256_hex(&t), token_sha256_hex(&format!(" {t} ")));
        assert_eq!(
            token_sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd") && !constant_time_eq("", ""));
    }
}
