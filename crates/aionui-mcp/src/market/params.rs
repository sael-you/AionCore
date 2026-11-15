//! Per-instance launch parameters (GAPS G1). A template may reference a plain
//! env key as `${KEY}` in its pinned stdio args, bridge args or url when that
//! key declares a `pattern`; the launcher substitutes the instance's value.
//! Values must fully match the pattern AND consist of URL/argv-safe
//! characters only, so a loose pattern still cannot inject `&`, `?`, `#`,
//! `/`, spaces or `..`. Secret refs are never substituted here (a bridge
//! expands those from its own env).

use std::collections::BTreeMap;

use aionui_api_types::{McpMarketCommand, McpMarketEnvField, McpMarketEnvKind, McpMarketTemplate};
use regex::Regex;

use super::rules::refs_in;

/// Compile a template pattern; it must be anchored (`^…$`).
pub fn compile_pattern(pattern: &str) -> Result<Regex, String> {
    if !pattern.starts_with('^') || !pattern.ends_with('$') {
        return Err("pattern must be anchored with ^ and $".to_owned());
    }
    Regex::new(pattern).map_err(|_| "pattern is not a valid regex".to_owned())
}

/// Characters a parameter value may hold, whatever the pattern says.
fn safe_chars(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains("..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `value` is acceptable for a plain key with a pattern.
pub fn value_matches(field: &McpMarketEnvField, value: &str) -> bool {
    match field.pattern.as_deref() {
        None => true,
        Some(pattern) => safe_chars(value) && compile_pattern(pattern).is_ok_and(|re| re.is_match(value)),
    }
}

/// Whether `key` is a launch parameter of `t` (plain, with a pattern).
pub fn is_param(t: &McpMarketTemplate, key: &str) -> bool {
    t.env
        .get(key)
        .is_some_and(|f| f.kind == McpMarketEnvKind::Plain && f.pattern.is_some())
}

/// Launch parameters referenced by the template's args/url.
pub fn referenced_params(t: &McpMarketTemplate) -> Vec<String> {
    let mut texts: Vec<&str> = Vec::new();
    for cmd in [&t.command, &t.bridge].into_iter().flatten() {
        texts.extend(cmd.args.iter().map(String::as_str));
    }
    if let Some(url) = &t.url {
        texts.push(url);
    }
    let mut out: Vec<String> = texts
        .into_iter()
        .flat_map(refs_in)
        .filter(|k| is_param(t, k))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Replace every `${KEY}` of a launch parameter in `text` with its value.
/// Other refs are left as they are. `Err` names the key only.
pub fn substitute(text: &str, t: &McpMarketTemplate, plain_env: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = text.to_owned();
    for key in refs_in(text) {
        if !is_param(t, &key) {
            continue;
        }
        let field = &t.env[&key];
        let value = plain_env
            .get(&key)
            .ok_or_else(|| format!("launch parameter {key} has no value"))?;
        if !value_matches(field, value) {
            return Err(format!("launch parameter {key} does not match the template pattern"));
        }
        out = out.replace(&format!("${{{key}}}"), value);
    }
    Ok(out)
}

/// A pinned command with its launch parameters substituted.
pub fn substitute_command(
    cmd: &McpMarketCommand,
    t: &McpMarketTemplate,
    plain_env: &BTreeMap<String, String>,
) -> Result<McpMarketCommand, String> {
    Ok(McpMarketCommand {
        command: cmd.command.clone(),
        args: cmd
            .args
            .iter()
            .map(|a| substitute(a, t, plain_env))
            .collect::<Result<_, _>>()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template() -> McpMarketTemplate {
        serde_json::from_value(serde_json::json!({
            "id": "supabase", "name": "Supabase", "transport": "http",
            "url": "https://mcp.supabase.com/mcp?project_ref=${SUPABASE_PROJECT_REF}",
            "bridge": {"command": "npx", "args": ["-y", "mcp-remote@0.16.6",
                "https://mcp.supabase.com/mcp?project_ref=${SUPABASE_PROJECT_REF}",
                "--header", "Authorization:Bearer ${SUPABASE_ACCESS_TOKEN}"]},
            "env": {
                "SUPABASE_ACCESS_TOKEN": {"kind": "secret", "required": true},
                "SUPABASE_PROJECT_REF": {"kind": "plain", "required": true, "pattern": "^[a-z]{20}$"}
            },
            "tiers": ["read", "write"], "enforcement": "server", "clis": ["claude"]
        }))
        .unwrap()
    }

    #[test]
    fn substitutes_params_and_keeps_secret_refs() {
        let t = template();
        let env = BTreeMap::from([("SUPABASE_PROJECT_REF".to_owned(), "abcdefghijklmnopqrst".to_owned())]);
        let cmd = substitute_command(t.bridge.as_ref().unwrap(), &t, &env).unwrap();
        assert_eq!(cmd.args[2], "https://mcp.supabase.com/mcp?project_ref=abcdefghijklmnopqrst");
        assert_eq!(cmd.args[4], "Authorization:Bearer ${SUPABASE_ACCESS_TOKEN}");
        assert_eq!(referenced_params(&t), vec!["SUPABASE_PROJECT_REF"]);
    }

    #[test]
    fn rejects_missing_or_injected_values() {
        let t = template();
        let url = t.url.clone().unwrap();
        assert!(substitute(&url, &t, &BTreeMap::new()).is_err());
        for bad in ["abc", "abcdefghijklmnopqrs&", "abcdefghijklmnopqrst&read_only=false", "ABCDEFGHIJKLMNOPQRST"] {
            let env = BTreeMap::from([("SUPABASE_PROJECT_REF".to_owned(), bad.to_owned())]);
            assert!(substitute(&url, &t, &env).is_err(), "{bad}");
        }
    }

    #[test]
    fn loose_patterns_still_refuse_unsafe_characters() {
        let field: McpMarketEnvField = serde_json::from_value(serde_json::json!({
            "kind": "plain", "pattern": "^.+$"
        }))
        .unwrap();
        for bad in ["a&b", "a/b", "a b", "..", "a?b", "a#b"] {
            assert!(!value_matches(&field, bad), "{bad}");
        }
        assert!(value_matches(&field, "12345"));
        assert!(compile_pattern("[a-z]+").is_err());
    }
}
