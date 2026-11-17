//! Secret redaction for provider error text (STT key hotfix, 2026-11-17).
//!
//! Upstream providers echo the API key in their error messages (OpenAI:
//! "Incorrect API key provided: sk-…"). Every error built from provider text
//! goes through [`redact_secrets`] before it is logged or returned: the
//! configured key values are replaced verbatim, and any key-shaped token is
//! replaced too (prefixed provider keys, masked keys like `sk-ab****yz`, long
//! random tokens).

pub const REDACTED: &str = "[REDACTED]";

/// Known provider key prefixes (OpenAI, Anthropic, Google, xAI, Groq, …).
const KEY_PREFIXES: &[&str] = &["sk-", "sk_", "rk-", "xai-", "gsk_", "AIza"];
/// A token this long with letters and digits is treated as a secret.
const LONG_TOKEN: usize = 32;

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '*' | '.')
}

fn key_shaped(token: &str) -> bool {
    let token = token.trim_matches('.');
    if KEY_PREFIXES
        .iter()
        .any(|prefix| token.starts_with(prefix) && token.len() > prefix.len())
    {
        return true;
    }
    // A masked key (`abc****xyz`) or a long random token (letters and digits).
    if token.contains("***") && token.len() >= 8 {
        return true;
    }
    token.len() >= LONG_TOKEN
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
        && !token.contains('.')
}

/// `text` with every configured key value (at least 8 characters) and every
/// key-shaped token replaced by `[REDACTED]`.
pub fn redact_secrets(text: &str, known: &[&str]) -> String {
    let mut out = text.to_owned();
    for key in known.iter().map(|k| k.trim()).filter(|k| k.len() >= 8) {
        out = out.replace(key, REDACTED);
    }
    let mut result = String::with_capacity(out.len());
    let mut token = String::new();
    let flush = |token: &mut String, result: &mut String| {
        if !token.is_empty() {
            if key_shaped(token) {
                result.push_str(REDACTED);
            } else {
                result.push_str(token);
            }
            token.clear();
        }
    };
    for c in out.chars() {
        if token_char(c) {
            token.push(c);
        } else {
            flush(&mut token, &mut result);
            result.push(c);
        }
    }
    flush(&mut token, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_KEY: &str = "sk-proj-FAKE0123456789abcdefFAKE0123456789";

    #[test]
    fn the_configured_key_and_key_shaped_tokens_never_survive() {
        let message = format!(
            "OpenAI realtime error (invalid_api_key): Incorrect API key provided: {FAKE_KEY}. You can find your API key at https://platform.openai.com/account/api-keys."
        );
        let redacted = redact_secrets(&message, &[FAKE_KEY]);
        assert!(!redacted.contains("FAKE0123"), "{redacted}");
        assert!(
            redacted.contains("Incorrect API key provided: [REDACTED]."),
            "{redacted}"
        );
        assert!(
            redacted.contains("https://platform.openai.com/account/api-keys"),
            "{redacted}"
        );
        assert!(redacted.contains("(invalid_api_key)"), "words stay");
    }

    #[test]
    fn masked_prefixed_and_long_tokens_are_redacted_without_knowing_the_key() {
        for secret in [
            "sk-ab********************yz12",
            "sk-shortish",
            "AIzaSyFAKEFAKEFAKEFAKEFAKE",
            "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
        ] {
            let redacted = redact_secrets(&format!("bad key {secret} here"), &[]);
            assert_eq!(redacted, "bad key [REDACTED] here", "{secret}");
        }
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let text = "OpenAI API returned 429 Too Many Requests: rate_limit_exceeded for gpt-4o-transcribe";
        assert_eq!(redact_secrets(text, &[]), text);
        assert_eq!(redact_secrets("", &["sk-x"]), "");
        // Short "keys" are not replaced verbatim (would shred normal words).
        assert_eq!(redact_secrets("the key abc", &["abc"]), "the key abc");
    }
}
