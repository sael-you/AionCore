//! Secret detection and masking for MCP configs crossing the API boundary
//! (MCP market R4/R5, R1 #11).
//!
//! The rules mirror the kit-lock validator of `studio sync`
//! (`clauterm-studio/tools/studio/lib/schema.js`): a key is secret-looking when
//! it contains one of [`SECRET_KEY_PARTS`]; a value looks like a credential when
//! it carries an auth scheme, a known credential prefix, URL userinfo, a secret
//! query key, or a long high-entropy run. Masked values become [`MASK`], which a
//! later update treats as "unchanged" (mask round-trip).

use std::collections::HashMap;

use aionui_api_types::McpTransport;

use crate::types::McpServerTransport;

/// Placeholder shown instead of a secret value. An update carrying exactly
/// this value keeps the stored one.
pub const MASK: &str = "•••";

/// Key fragments that make an env/header/query key secret-looking (R4, R1 #13).
pub const SECRET_KEY_PARTS: &[&str] = &["KEY", "TOKEN", "SECRET", "PASS", "AUTH", "DSN", "CREDENTIAL", "COOKIE"];

/// Credential prefixes and the minimum body length after them.
const SECRET_PREFIXES: &[(&str, usize)] = &[
    ("sk_live_", 8),
    ("sk_test_", 8),
    ("pk_live_", 8),
    ("pk_test_", 8),
    ("rk_live_", 8),
    ("rk_test_", 8),
    ("pdl_live_", 8),
    ("pdl_sdbx_", 8),
    ("whsec_", 8),
    ("tvly-", 8),
    ("re_", 12),
    ("sntrys_", 12),
    ("glpat-", 12),
    ("npm_", 20),
    ("ya29.", 12),
    ("ghp_", 20),
    ("gho_", 20),
    ("ghu_", 20),
    ("ghs_", 20),
    ("ghr_", 20),
    ("github_pat_", 20),
    ("xoxa-", 10),
    ("xoxb-", 10),
    ("xoxp-", 10),
    ("xoxr-", 10),
    ("xoxs-", 10),
    ("AKIA", 16),
    ("AIza", 30),
    ("sk-", 20),
    ("eyJ", 20),
];

/// Whether `key` is secret-looking (case-insensitive substring match).
pub fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SECRET_KEY_PARTS.iter().any(|part| upper.contains(part))
}

/// `${NAME}` reference (not a secret itself).
pub fn is_ref(value: &str) -> bool {
    value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|name| {
            let mut chars = name.chars();
            chars.next().is_some_and(|c| c.is_ascii_uppercase())
                && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '_' | '-' | '/' | '.')
}

fn has_prefixed_credential(value: &str) -> bool {
    for (prefix, min_body) in SECRET_PREFIXES {
        let mut from = 0;
        while let Some(pos) = value[from..].find(prefix) {
            let start = from + pos;
            let boundary = value[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric());
            let body = value[start + prefix.len()..]
                .chars()
                .take_while(|c| token_char(*c))
                .count();
            if boundary && body >= *min_body {
                return true;
            }
            from = start + prefix.len();
        }
    }
    false
}

fn has_auth_scheme(value: &str) -> bool {
    let words: Vec<&str> = value.split_whitespace().collect();
    words
        .windows(2)
        .any(|pair| matches!(pair[0].to_ascii_lowercase().as_str(), "bearer" | "basic" | "token") && pair[1].len() >= 8)
}

fn entropy(s: &str) -> f64 {
    let mut counts: HashMap<char, usize> = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_default() += 1;
    }
    let len = s.chars().count() as f64;
    counts
        .values()
        .map(|&n| {
            let p = n as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// A random-looking run: >= 32 hex chars, or >= 24 base64-ish chars mixing
/// letters and digits with high entropy.
fn has_high_entropy_run(value: &str) -> bool {
    value
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '_' | '-')))
        .filter(|run| run.len() >= 24)
        .any(|run| {
            let core = run.trim_end_matches('=');
            if !core.chars().any(|c| c.is_ascii_digit()) || !core.chars().any(|c| c.is_ascii_alphabetic()) {
                return false;
            }
            (core.len() >= 32 && core.chars().all(|c| c.is_ascii_hexdigit())) || entropy(core) >= 3.7
        })
}

fn url_has_secret(value: &str) -> bool {
    for candidate in value.split_whitespace() {
        let Some(scheme_end) = candidate.find("://") else {
            continue;
        };
        let rest = &candidate[scheme_end + 3..];
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return true;
        }
        if let Ok(url) = reqwest::Url::parse(candidate)
            && url.query_pairs().any(|(k, _)| is_secret_key(&k))
        {
            return true;
        }
    }
    false
}

/// Whether a literal value looks like a credential under any key.
pub fn looks_like_secret_value(value: &str) -> bool {
    !is_ref(value)
        && (url_has_secret(value)
            || has_auth_scheme(value)
            || has_prefixed_credential(value)
            || has_high_entropy_run(value))
}

/// Whether `(key, value)` must be hidden from API responses.
pub fn must_mask(key: &str, value: &str) -> bool {
    !value.is_empty() && !is_ref(value) && value != MASK && (is_secret_key(key) || looks_like_secret_value(value))
}

/// Mask every secret value of an env/header map in place.
pub fn mask_map(map: &mut HashMap<String, String>) {
    for (key, value) in map.iter_mut() {
        if must_mask(key, value) {
            *value = MASK.to_owned();
        }
    }
}

/// Mask URL userinfo and the values of secret-looking query keys.
pub fn mask_url(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return if looks_like_secret_value(url) {
            MASK.to_owned()
        } else {
            url.to_owned()
        };
    };
    let mut changed = false;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        let _ = parsed.set_username(MASK);
        let _ = parsed.set_password(None);
        changed = true;
    }
    if parsed.query_pairs().any(|(k, _)| is_secret_key(&k)) {
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| {
                let v = if is_secret_key(&k) {
                    MASK.to_owned()
                } else {
                    v.into_owned()
                };
                (k.into_owned(), v)
            })
            .collect();
        parsed.query_pairs_mut().clear().extend_pairs(pairs);
        changed = true;
    }
    if changed { parsed.to_string() } else { url.to_owned() }
}

/// Mask an API transport in place (env, headers, url).
pub fn mask_transport(transport: &mut McpTransport) {
    match transport {
        McpTransport::Stdio { args, env, .. } => {
            mask_map(env);
            mask_args(args);
        }
        McpTransport::Http { url, headers } | McpTransport::Sse { url, headers } => {
            mask_map(headers);
            *url = mask_url(url);
        }
    }
}

/// Mask argv entries that are credentials or the value of a secret-looking flag.
fn mask_args(args: &mut [String]) {
    let mut mask_next = false;
    for arg in args.iter_mut() {
        if mask_next && !arg.starts_with('-') {
            *arg = MASK.to_owned();
            mask_next = false;
            continue;
        }
        mask_next = false;
        let replacement = match arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) {
            Some(flag) => match flag.split_once('=') {
                Some((name, value)) if is_secret_key(&name.replace('-', "_")) && !value.is_empty() => {
                    Some(format!("{}={MASK}", &arg[..arg.len() - value.len() - 1]))
                }
                None if is_secret_key(&flag.replace('-', "_")) => {
                    mask_next = true;
                    None
                }
                _ => None,
            },
            None if looks_like_secret_value(arg) => Some(MASK.to_owned()),
            None => None,
        };
        if let Some(replacement) = replacement {
            *arg = replacement;
        }
    }
}

fn mask_json(value: &mut serde_json::Value, parent_key: Option<&str>) -> bool {
    let mut changed = false;
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match child {
                    serde_json::Value::String(s) if key == "url" || key == "httpUrl" => {
                        let masked = mask_url(s);
                        if masked != *s {
                            *s = masked;
                            changed = true;
                        }
                    }
                    serde_json::Value::String(s) if must_mask(key, s) => {
                        *s = MASK.to_owned();
                        changed = true;
                    }
                    _ => changed |= mask_json(child, Some(key)),
                }
            }
        }
        serde_json::Value::Array(items) => {
            if parent_key == Some("args") {
                let mut strings: Vec<String> = items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
                if strings.len() == items.len() {
                    let before = strings.clone();
                    mask_args(&mut strings);
                    if strings != before {
                        *items = strings.into_iter().map(serde_json::Value::String).collect();
                        changed = true;
                    }
                    return changed;
                }
            }
            for item in items {
                changed |= mask_json(item, parent_key);
            }
        }
        serde_json::Value::String(s) if looks_like_secret_value(s) => {
            *s = MASK.to_owned();
            changed = true;
        }
        _ => {}
    }
    changed
}

/// Mask the raw `original_json` text kept for edit restoration. Unchanged
/// text is returned as is; text that cannot be parsed is dropped (it could
/// hold anything).
pub fn mask_original_json(raw: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    if mask_json(&mut value, None) {
        serde_json::to_string(&value).ok()
    } else {
        Some(raw.to_owned())
    }
}

/// Whether a domain transport still carries a [`MASK`] placeholder anywhere.
pub fn transport_contains_mask(transport: &McpServerTransport) -> bool {
    match transport {
        McpServerTransport::Stdio { args, env, .. } => {
            env.values().any(|v| v.contains(MASK)) || args.iter().any(|a| a.contains(MASK))
        }
        McpServerTransport::Http { url, headers } | McpServerTransport::Sse { url, headers } => {
            url.contains(MASK) || url.contains("%E2%80%A2") || headers.values().any(|v| v.contains(MASK))
        }
    }
}

fn unmask_map(incoming: &mut HashMap<String, String>, existing: &HashMap<String, String>) {
    for (key, value) in incoming.iter_mut() {
        if value == MASK
            && let Some(old) = existing.get(key)
        {
            value.clone_from(old);
        }
    }
}

fn unmask_args(incoming: &mut [String], existing: &[String]) {
    if incoming.len() != existing.len() {
        return;
    }
    let mut masked_existing = existing.to_vec();
    mask_args(&mut masked_existing);
    for ((arg, old), masked_old) in incoming.iter_mut().zip(existing).zip(&masked_existing) {
        if arg.contains(MASK) && arg == masked_old {
            arg.clone_from(old);
        }
    }
}

/// `origin + path` of a URL (scheme, host, port, path), or the whole string
/// when it does not parse.
fn url_destination(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => format!("{}{}", parsed.origin().ascii_serialization(), parsed.path()),
        Err(_) => url.to_owned(),
    }
}

/// Restore stored values for every field the client sent back masked (the
/// mask round-trip of `PUT /api/mcp/servers/{id}` and `test-connection {id}`).
/// Only values whose masked form equals what the client sent are restored,
/// and only while the destination is unchanged: the same command and args
/// (stdio) or the same URL origin + path (http/sse). A masked value sent with
/// a changed destination stays masked, so the caller refuses it with
/// `MCP_MASKED_VALUE` — a stored secret never travels to a new command or
/// host (train-2 review C2 #2, TASK-69).
pub fn unmask_transport(incoming: &mut McpServerTransport, existing: &McpServerTransport) {
    match (incoming, existing) {
        (
            McpServerTransport::Stdio { command, args, env },
            McpServerTransport::Stdio {
                command: old_command,
                args: old_args,
                env: old_env,
            },
        ) => {
            let mut restored_args = args.clone();
            unmask_args(&mut restored_args, old_args);
            if command != old_command || restored_args != *old_args {
                return;
            }
            *args = restored_args;
            unmask_map(env, old_env);
        }
        (
            McpServerTransport::Http { url, headers },
            McpServerTransport::Http {
                url: old_url,
                headers: old_headers,
            },
        )
        | (
            McpServerTransport::Sse { url, headers },
            McpServerTransport::Sse {
                url: old_url,
                headers: old_headers,
            },
        ) => {
            let restored_url = if *url != *old_url && *url == mask_url(old_url) {
                old_url.clone()
            } else {
                url.clone()
            };
            if url_destination(&restored_url) != url_destination(old_url) {
                return;
            }
            *url = restored_url;
            unmask_map(headers, old_headers);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_KEY: &str = "pdl_live_FAKEFAKEFAKE1234567890";

    #[test]
    fn secret_keys_and_values() {
        assert!(is_secret_key("PADDLE_API_KEY"));
        assert!(is_secret_key("x-auth"));
        assert!(!is_secret_key("PADDLE_ENVIRONMENT"));
        assert!(looks_like_secret_value(FAKE_KEY));
        assert!(looks_like_secret_value("Bearer abcdefgh12345"));
        assert!(looks_like_secret_value("https://u:p@db.example.com/x"));
        assert!(looks_like_secret_value("https://mcp.example.com/?tavilyApiKey=abc"));
        assert!(looks_like_secret_value(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        ));
        assert!(!looks_like_secret_value("production"));
        assert!(!looks_like_secret_value("@paddle/paddle-mcp@0.1.0"));
        assert!(!looks_like_secret_value("${TB_PADDLE__PADDLE_API_KEY}"));
    }

    #[test]
    fn transport_masking_and_round_trip() {
        let stored = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "srv".into(), "--api-key".into(), FAKE_KEY.into()],
            env: HashMap::from([
                ("PADDLE_API_KEY".into(), FAKE_KEY.into()),
                ("PADDLE_ENVIRONMENT".into(), "production".into()),
            ]),
        };
        let mut api: McpTransport = stored.clone().into();
        mask_transport(&mut api);
        let text = serde_json::to_string(&api).unwrap();
        assert!(!text.contains("FAKEFAKE"), "{text}");
        assert!(text.contains("production"));

        let mut back = McpServerTransport::from(api);
        assert!(transport_contains_mask(&back));
        unmask_transport(&mut back, &stored);
        assert_eq!(back, stored);
    }

    #[test]
    fn url_masking_round_trip() {
        let stored = McpServerTransport::Http {
            url: "https://mcp.example.com/mcp?tavilyApiKey=tvly-SECRETSECRET&x=1".into(),
            headers: HashMap::from([("Authorization".into(), "Bearer SECRETSECRET99".into())]),
        };
        let mut api: McpTransport = stored.clone().into();
        mask_transport(&mut api);
        let text = serde_json::to_string(&api).unwrap();
        assert!(!text.contains("SECRETSECRET"), "{text}");
        let mut back = McpServerTransport::from(api);
        unmask_transport(&mut back, &stored);
        assert_eq!(back, stored);
    }

    #[test]
    fn original_json_masking() {
        let raw = format!(
            r#"{{"mcpServers":{{"p":{{"command":"npx","env":{{"PADDLE_API_KEY":"{FAKE_KEY}","MODE":"x"}}}}}}}}"#
        );
        let masked = mask_original_json(&raw).unwrap();
        assert!(!masked.contains("FAKEFAKE"));
        assert!(masked.contains("MODE"));
        let clean = r#"{"name":"stdio-full"}"#;
        assert_eq!(mask_original_json(clean).as_deref(), Some(clean));
        assert_eq!(mask_original_json("not json"), None);
    }

    /// TASK-69: a masked value is restored only while the destination
    /// (stdio command + args, http/sse URL origin + path) is unchanged.
    #[test]
    fn masked_values_are_not_restored_onto_a_changed_destination() {
        let stdio = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "srv".into()],
            env: HashMap::from([("PADDLE_API_KEY".into(), FAKE_KEY.into())]),
        };
        let masked_env = HashMap::from([("PADDLE_API_KEY".to_owned(), MASK.to_owned())]);
        for (command, args) in [("evil", vec!["-y", "srv"]), ("npx", vec!["-y", "evil-srv"])] {
            let mut back = McpServerTransport::Stdio {
                command: command.into(),
                args: args.into_iter().map(String::from).collect(),
                env: masked_env.clone(),
            };
            unmask_transport(&mut back, &stdio);
            assert!(transport_contains_mask(&back), "{command}: secret must stay masked");
        }
        let mut same = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "srv".into()],
            env: masked_env,
        };
        unmask_transport(&mut same, &stdio);
        assert_eq!(same, stdio);

        let headers = HashMap::from([("Authorization".to_owned(), "Bearer SECRETSECRET99".to_owned())]);
        let masked_headers = HashMap::from([("Authorization".to_owned(), MASK.to_owned())]);
        for make in [
            |url: &str, headers: HashMap<String, String>| McpServerTransport::Http {
                url: url.into(),
                headers,
            },
            |url: &str, headers: HashMap<String, String>| McpServerTransport::Sse {
                url: url.into(),
                headers,
            },
        ] {
            let stored = make("https://mcp.example.com/mcp?x=1", headers.clone());
            for url in [
                "https://evil.example/mcp?x=1",
                "https://mcp.example.com/other?x=1",
                "http://mcp.example.com/mcp?x=1",
            ] {
                let mut back = make(url, masked_headers.clone());
                unmask_transport(&mut back, &stored);
                assert!(transport_contains_mask(&back), "{url}: header must stay masked");
            }
            // Same origin + path, other non-secret query: restored.
            let mut back = make("https://mcp.example.com/mcp?x=2", masked_headers.clone());
            unmask_transport(&mut back, &stored);
            assert_eq!(back, make("https://mcp.example.com/mcp?x=2", headers.clone()));
        }
        // A masked secret query value moved to another host is not restored.
        let stored = McpServerTransport::Http {
            url: "https://mcp.example.com/mcp?tavilyApiKey=tvly-SECRETSECRET".into(),
            headers: HashMap::new(),
        };
        let moved = mask_url("https://mcp.example.com/mcp?tavilyApiKey=tvly-SECRETSECRET")
            .replace("mcp.example.com", "evil.example");
        let mut back = McpServerTransport::Http {
            url: moved,
            headers: HashMap::new(),
        };
        unmask_transport(&mut back, &stored);
        assert!(transport_contains_mask(&back));
    }
}
