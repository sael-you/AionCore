//! TASK-97: make claude ask before every call to an MCP server whose calls need
//! the user's confirmation (a production write market instance).
//!
//! claude's mechanism is a `permissions.ask` rule. Verified sources:
//! - `--settings <file-or-json>` "Path to a settings JSON file or a JSON string to
//!   load additional settings from" (`claude --help`, 2.1.325): ADDITIONAL, so the
//!   user's and the project's own settings keep applying.
//! - "To match every tool from an MCP server, write `mcp__<server>__*`"; names with
//!   characters such as `.` or spaces have them replaced by `_`
//!   (code.claude.com/docs/en/permissions, "MCP").
//! - "Rules are evaluated in order: deny, then ask, then allow" and "a matching ask
//!   rule prompts even when a more specific allow rule also matches" (same page).
//! - Claude Code doesn't auto-approve "Tools matched by an explicit ask rule" in any
//!   mode, "including `bypassPermissions`" (code.claude.com/docs/en/permission-modes).
//!
//! claude reads ONE `--settings` value; a caller-supplied one (agent metadata args)
//! is therefore MERGED with the ask rules, never replaced or shadowed.

use serde_json::{Map, Value, json};

use super::McpServerSpec;

const SETTINGS_FLAG: &str = "--settings";
const SETTINGS_PREFIX: &str = "--settings=";

/// `mcp__<server>__*` for every server flagged `require_approval`, in order.
pub(crate) fn claude_ask_rules(servers: &[McpServerSpec]) -> Vec<String> {
    let mut rules: Vec<String> = Vec::new();
    for server in servers.iter().filter(|s| s.tool_filter.require_approval) {
        let rule = format!("mcp__{}__*", rule_server_name(&server.name));
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    rules
}

/// claude replaces characters such as `.` or spaces in a server name with `_` in
/// its tool names; letters, digits, `_` and `-` stay (the docs' plugin example
/// `mcp__plugin_my-plugin_database-tools__*` keeps the hyphens).
fn rule_server_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Add `ask` to the `--settings` value in `args`, or append `--settings <json>`
/// when there is none. A caller value that is a file path is read through
/// `read_file` and passed inline after the merge. A value that is neither a JSON
/// object nor a readable JSON-object file cannot be merged: the ask rules are
/// appended as the LAST `--settings` so the confirmation can never be lost
/// (fail closed), and the drop is logged.
pub(crate) fn merge_ask_into_settings_args(
    args: &mut Vec<String>,
    ask: &[String],
    read_file: impl Fn(&str) -> Option<String>,
) {
    if ask.is_empty() {
        return;
    }
    let Some((index, value)) = find_settings_value(args) else {
        args.push(SETTINGS_FLAG.to_owned());
        args.push(with_ask(Map::new(), ask));
        return;
    };
    let existing = parse_object(&value).or_else(|| read_file(&value).as_deref().and_then(parse_object));
    match existing {
        Some(settings) => {
            let merged = with_ask(settings, ask);
            if args[index] == SETTINGS_FLAG {
                args[index + 1] = merged;
            } else {
                args[index] = format!("{SETTINGS_PREFIX}{merged}");
            }
        }
        None => {
            tracing::warn!(
                "claude: caller --settings is not a JSON object or readable JSON file; \
                 appending the MCP ask rules as the effective --settings"
            );
            args.push(SETTINGS_FLAG.to_owned());
            args.push(with_ask(Map::new(), ask));
        }
    }
}

/// The LAST `--settings` occurrence: `(index of the flag-or-joined arg, value)`.
fn find_settings_value(args: &[String]) -> Option<(usize, String)> {
    let mut found = None;
    for (i, arg) in args.iter().enumerate() {
        if arg == SETTINGS_FLAG {
            if let Some(value) = args.get(i + 1) {
                found = Some((i, value.clone()));
            }
        } else if let Some(value) = arg.strip_prefix(SETTINGS_PREFIX) {
            found = Some((i, value.to_owned()));
        }
    }
    found
}

fn parse_object(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// `settings` with every `ask` rule present once in `permissions.ask`; every
/// other key (including `permissions.allow` / `deny`) is kept as it was.
fn with_ask(mut settings: Map<String, Value>, ask: &[String]) -> String {
    let permissions = settings.entry("permissions").or_insert_with(|| json!({}));
    if !permissions.is_object() {
        *permissions = json!({});
    }
    let Some(permissions) = permissions.as_object_mut() else {
        unreachable!("permissions was just made an object");
    };
    let rules = permissions.entry("ask").or_insert_with(|| json!([]));
    if !rules.is_array() {
        *rules = json!([]);
    }
    if let Some(rules) = rules.as_array_mut() {
        for rule in ask {
            if !rules.iter().any(|r| r.as_str() == Some(rule.as_str())) {
                rules.push(Value::String(rule.clone()));
            }
        }
    }
    Value::Object(settings).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{McpToolFilter, McpTransport};

    fn server(name: &str, require_approval: bool) -> McpServerSpec {
        McpServerSpec {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: "/opt/aioncore".into(),
                args: vec![],
                env: vec![],
            },
            tool_filter: McpToolFilter {
                require_approval,
                ..Default::default()
            },
        }
    }

    fn settings_json(args: &[String]) -> Value {
        let (_, value) = find_settings_value(args).expect("a --settings flag");
        serde_json::from_str(&value).unwrap()
    }

    fn no_file(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn only_flagged_servers_get_an_ask_rule() {
        let rules = claude_ask_rules(&[
            server("paddle-live", true),
            server("context7", false),
            server("paddle-live", true),
            server("my.server", true),
        ]);
        assert_eq!(rules, vec!["mcp__paddle-live__*", "mcp__my_server__*"]);
        assert!(claude_ask_rules(&[server("context7", false)]).is_empty());
    }

    #[test]
    fn no_rules_leave_the_args_untouched() {
        let mut args = vec!["--permission-mode".to_string(), "default".to_string()];
        let before = args.clone();
        merge_ask_into_settings_args(&mut args, &[], no_file);
        assert_eq!(args, before);
    }

    #[test]
    fn rules_without_caller_settings_append_one_settings_flag() {
        let mut args = vec!["--permission-mode".to_string(), "bypassPermissions".to_string()];
        merge_ask_into_settings_args(&mut args, &["mcp__paddle-live__*".to_string()], no_file);
        assert_eq!(args.iter().filter(|a| *a == SETTINGS_FLAG).count(), 1);
        assert_eq!(
            settings_json(&args),
            json!({"permissions": {"ask": ["mcp__paddle-live__*"]}})
        );
    }

    #[test]
    fn inline_caller_settings_are_merged_not_replaced() {
        let caller = r#"{"env":{"X":"1"},"permissions":{"allow":["Bash(git *)"],"ask":["WebFetch"]}}"#;
        let mut args = vec![SETTINGS_FLAG.to_string(), caller.to_string(), "--verbose".to_string()];
        merge_ask_into_settings_args(&mut args, &["mcp__paddle-live__*".to_string()], no_file);
        assert_eq!(args.len(), 3, "merged in place, no second --settings");
        assert_eq!(
            settings_json(&args),
            json!({
                "env": {"X": "1"},
                "permissions": {"allow": ["Bash(git *)"], "ask": ["WebFetch", "mcp__paddle-live__*"]}
            })
        );
    }

    #[test]
    fn joined_form_and_existing_rule_are_merged_once() {
        let mut args = vec![r#"--settings={"permissions":{"ask":["mcp__paddle-live__*"]}}"#.to_string()];
        merge_ask_into_settings_args(&mut args, &["mcp__paddle-live__*".to_string()], no_file);
        assert_eq!(args.len(), 1);
        assert_eq!(
            settings_json(&args),
            json!({"permissions": {"ask": ["mcp__paddle-live__*"]}})
        );
    }

    #[test]
    fn a_settings_file_is_read_and_merged_inline() {
        let mut args = vec![SETTINGS_FLAG.to_string(), "/etc/agent/settings.json".to_string()];
        let read = |path: &str| (path == "/etc/agent/settings.json").then(|| r#"{"model":"x"}"#.to_string());
        merge_ask_into_settings_args(&mut args, &["mcp__paddle-live__*".to_string()], read);
        assert_eq!(args.len(), 2);
        assert_eq!(
            settings_json(&args),
            json!({"model": "x", "permissions": {"ask": ["mcp__paddle-live__*"]}})
        );
    }

    #[test]
    fn an_unmergeable_caller_value_still_ends_with_the_ask_rules() {
        let mut args = vec![SETTINGS_FLAG.to_string(), "/missing.json".to_string()];
        merge_ask_into_settings_args(&mut args, &["mcp__paddle-live__*".to_string()], no_file);
        assert_eq!(&args[..2], &[SETTINGS_FLAG.to_string(), "/missing.json".to_string()]);
        assert_eq!(
            settings_json(&args),
            json!({"permissions": {"ask": ["mcp__paddle-live__*"]}}),
            "the LAST --settings carries the ask rules"
        );
    }
}
