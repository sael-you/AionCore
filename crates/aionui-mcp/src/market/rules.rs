//! Write-time market rules (PLAN §2 R1–R11 as amended by Review R1). Mirrors
//! the kit-lock validator of `studio sync` (`tools/studio/lib/schema.js`) so a
//! row AionCore accepts is a row the generator compiles. Messages name keys,
//! ids and rules, never values.

use std::collections::BTreeMap;

use aionui_api_types::{McpMarketEnvKind, McpMarketTemplate};

use super::params::{compile_pattern, is_param, referenced_params, value_matches};
use crate::secret_mask::{is_secret_key, looks_like_secret_value};

pub const CLIS: &[&str] = &["claude", "codex", "gemini"];
pub const TIERS: &[&str] = &["read", "write"];
pub const STATUSES: &[&str] = &["active", "dormant", "revoked"];
pub const ENVIRONMENTS: &[&str] = &["production", "staging", "sandbox", "local"];
/// `proxy` (G3): the launcher's stdio proxy enforces the template's pinned
/// read allowlist (`read_tools`); counts as read-only enforcement for R6.
pub const ENFORCEMENTS: &[&str] = &["server", "credential", "proxy", "tools-only"];
/// Enforcements that make a production read instance acceptable (R6).
pub const READ_ENFORCEMENTS: &[&str] = &["server", "credential", "proxy"];
/// Allowed `billing` values (G7).
pub const BILLINGS: &[&str] = &["metered"];
pub const TRANSPORTS: &[&str] = &["stdio", "http"];
/// R2: the only instances that may be assigned globally.
pub const GLOBAL_IDS: &[&str] = aionui_db::GLOBAL_MCP_IDS;
pub const RESERVED: &[&str] = &["aionui-team", "aionui"];
/// R11: not in the market, matched as prefixes. GitHub and Chrome DevTools
/// were removed by the owner's picks (D49/D50, GAPS G2).
pub const NOT_IN_MARKET: &[&str] = &[
    "playwright",
    "mcp-playwright",
    "codex-cli",
    "gemini-cli",
    "sequential-thinking",
    "serena",
    "openai-image",
];
/// R1 #14.
pub const NAME_MAX: usize = 24;
/// `mcp__<id>__<tool>` (Claude/Codex) and Gemini function names.
pub const TOOL_QUALIFIED_MAX: usize = 64;
/// Max secret value size accepted by the secrets route.
pub const SECRET_VALUE_MAX: usize = 16 * 1024;

/// R1 #14: `^[a-z][a-z0-9]*(-[a-z0-9]+)*$`, at most [`NAME_MAX`] chars.
pub fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if name.is_empty() || name.len() > NAME_MAX || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    let mut prev_hyphen = false;
    for &b in bytes {
        match b {
            b'a'..=b'z' | b'0'..=b'9' => prev_hyphen = false,
            b'-' if !prev_hyphen => prev_hyphen = true,
            _ => return false,
        }
    }
    !prev_hyphen
}

pub fn not_in_market(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    NOT_IN_MARKET.iter().any(|p| lower.starts_with(p))
}

/// Instance-name problems (R1, R11, R1 #14), or `None`.
pub fn name_problem(name: &str) -> Option<String> {
    if !is_valid_name(name) {
        return Some(format!(
            "instance id \"{name}\" must match ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ and be at most {NAME_MAX} chars (R1 #14)"
        ));
    }
    if RESERVED.contains(&name) {
        return Some(format!("instance id \"{name}\" is reserved"));
    }
    if not_in_market(name) {
        return Some(format!("\"{name}\" is not in the market (R11)"));
    }
    None
}

/// `^[A-Za-z0-9_.-]{1,128}$`
pub fn is_valid_tool_name(tool: &str) -> bool {
    !tool.is_empty()
        && tool.len() <= 128
        && tool
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// `^[A-Z][A-Z0-9_]*$` env key, the shape a `${NAME}` ref can carry.
pub fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Owner exception record id (provisional, matches the generator):
/// `OA-<yyyymmdd>-<slug>`.
pub fn is_valid_approval_ref(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("OA-") else {
        return false;
    };
    let Some((date, slug)) = rest.split_once('-') else {
        return false;
    };
    date.len() == 8
        && date.bytes().all(|b| b.is_ascii_digit())
        && !slug.is_empty()
        && slug
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// The ref name of an instance secret: `TB_PADDLE_PROD_READ__PADDLE_API_KEY`.
pub fn secret_ref_name(instance_name: &str, key: &str) -> String {
    format!(
        "{}__{}",
        instance_name.to_ascii_uppercase().replace('-', "_"),
        key.to_ascii_uppercase().replace('-', "_")
    )
}

fn has_control(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// Catalogue template checks (pinned launch data, schema).
pub fn template_problems(t: &McpMarketTemplate) -> Vec<String> {
    let mut p = Vec::new();
    let w = format!("template \"{}\"", t.id);
    if !is_valid_name(&t.id) || t.id.len() > 40 {
        p.push(format!("{w}: id must be a lowercase hyphenated name"));
    }
    if not_in_market(&t.id) {
        p.push(format!("{w}: not in the market (R11)"));
    }
    if !TRANSPORTS.contains(&t.transport.as_str()) {
        p.push(format!("{w}: transport must be stdio|http"));
    }
    if !ENFORCEMENTS.contains(&t.enforcement.as_str()) {
        p.push(format!("{w}: enforcement must be {}", ENFORCEMENTS.join("|")));
    }
    if t.tiers.is_empty() || t.tiers.iter().any(|x| !TIERS.contains(&x.as_str())) {
        p.push(format!("{w}: tiers must be a non-empty subset of read,write"));
    }
    if t.clis.is_empty() || t.clis.iter().any(|x| !CLIS.contains(&x.as_str())) {
        p.push(format!("{w}: clis must be a non-empty subset of claude,codex,gemini"));
    }
    for (key, field) in &t.env {
        if !is_valid_env_key(key) {
            p.push(format!("{w}: env key \"{key}\" is invalid"));
        }
        if field.kind == McpMarketEnvKind::Plain
            && let Some(default) = &field.default
            && looks_like_secret_value(default)
        {
            p.push(format!("{w}: env.{key} default looks like a credential (R4)"));
        }
        if let Some(pattern) = &field.pattern {
            if field.kind != McpMarketEnvKind::Plain {
                p.push(format!("{w}: env.{key} pattern is allowed on plain keys only (G1)"));
            } else if let Err(e) = compile_pattern(pattern) {
                p.push(format!("{w}: env.{key} {e} (G1)"));
            } else {
                let samples = field.default.iter().chain(field.allowed.iter());
                for value in samples {
                    if !value_matches(field, value) {
                        p.push(format!(
                            "{w}: env.{key} default/enum value does not match its pattern (G1)"
                        ));
                    }
                }
            }
        }
    }
    for key in referenced_params(t) {
        if !t.env[&key].required {
            p.push(format!("{w}: launch parameter {key} must be required (G1)"));
        }
    }
    if let Some(billing) = &t.billing
        && !BILLINGS.contains(&billing.as_str())
    {
        p.push(format!("{w}: billing must be one of {}", BILLINGS.join("|")));
    }
    for tool in &t.read_tools {
        if !is_valid_tool_name(tool) {
            p.push(format!("{w}: read_tools has an invalid tool name"));
        } else if t.tool_classes.get(tool).is_some_and(|c| c == "write") {
            p.push(format!("{w}: read_tools.{tool} is classified write (G3)"));
        }
    }
    if t.enforcement == "proxy" && t.tiers.iter().any(|x| x == "read") && t.read_tools.is_empty() {
        p.push(format!(
            "{w}: enforcement proxy needs a pinned read_tools allowlist (G3)"
        ));
    }
    let check_command =
        |cmd: &aionui_api_types::McpMarketCommand, what: &str, allow_refs: bool, p: &mut Vec<String>| {
            if cmd.command.trim().is_empty() || has_control(&cmd.command) || cmd.command.contains("${") {
                p.push(format!("{w}: {what} command must be a pinned literal"));
            }
            for (i, arg) in cmd.args.iter().enumerate() {
                if has_control(arg) {
                    p.push(format!("{w}: {what} args[{i}] has control characters"));
                }
                if arg.contains("${") {
                    // Launch parameters (G1) anywhere; other declared keys only in a bridge.
                    let ok = refs_in(arg)
                        .iter()
                        .all(|r| is_param(t, r) || (allow_refs && t.env.contains_key(r)));
                    if !ok {
                        p.push(format!("{w}: {what} args[{i}] contains a ${{...}} ref (R1 #13)"));
                    }
                } else if looks_like_secret_value(arg) {
                    p.push(format!("{w}: {what} args[{i}] looks like a credential (R4)"));
                }
            }
        };
    match t.transport.as_str() {
        "stdio" => match &t.command {
            Some(cmd) => check_command(cmd, "stdio", false, &mut p),
            None => p.push(format!("{w}: stdio templates need a pinned command")),
        },
        "http" => {
            match t.url.as_deref() {
                Some(url) if refs_in(url).iter().any(|r| !is_param(t, r)) => p.push(format!(
                    "{w}: no ${{NAME}} in url except launch parameters (R1 #13, G1)"
                )),
                Some(url) => match reqwest::Url::parse(&mask_params(url)) {
                    Ok(u) if matches!(u.scheme(), "http" | "https") => {
                        if looks_like_secret_value(url) {
                            p.push(format!("{w}: url holds a credential (R4)"));
                        }
                    }
                    _ => p.push(format!("{w}: url must be http(s)")),
                },
                None => p.push(format!("{w}: http templates need a url")),
            }
            for (header, key) in &t.headers {
                if !t.env.contains_key(key) {
                    p.push(format!("{w}: header {header} refers to undeclared env key {key}"));
                }
            }
            if let Some(bridge) = &t.bridge {
                // The bridge reads its URL/headers from env; `${KEY}` of a declared key is allowed here only.
                check_command(bridge, "bridge", true, &mut p);
            }
        }
        _ => {}
    }
    for (tool, class) in &t.tool_classes {
        if !is_valid_tool_name(tool) || !TIERS.contains(&class.as_str()) {
            p.push(format!("{w}: tool_classes.{tool} must be read|write"));
        }
    }
    for (tier, forced) in &t.tier_env {
        if !t.tiers.contains(tier) {
            p.push(format!("{w}: tier_env.{tier} is not a declared tier"));
        }
        for key in forced.keys() {
            if t.env.get(key).is_none_or(|f| f.kind != McpMarketEnvKind::Plain) {
                p.push(format!("{w}: tier_env.{tier}.{key} must be a declared plain key"));
            }
        }
    }
    p
}

/// `url` with every `${…}` replaced by a neutral token, for URL parsing.
fn mask_params(url: &str) -> String {
    let mut out = url.to_owned();
    for r in refs_in(url) {
        out = out.replace(&format!("${{{r}}}"), "x");
    }
    out
}

/// `${NAME}` refs inside a string.
pub fn refs_in(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { break };
        out.push(after[..end].to_owned());
        rest = &after[end + 1..];
    }
    out
}

/// The instance fields the rules look at.
#[derive(Debug, Clone)]
pub struct InstanceSpec<'a> {
    pub name: &'a str,
    pub environment: &'a str,
    pub tier: &'a str,
    pub status: &'a str,
    pub clis: &'a [String],
    pub plain_env: &'a BTreeMap<String, String>,
    pub tools_allow: Option<&'a [String]>,
    pub tools_block: &'a [String],
    pub approval_ref: Option<&'a str>,
}

/// Instance problems against its template (R1, R3, R4, R6, R11, R1 #13/#14).
/// `secrets_set` = template secret keys that hold a value (activation check).
pub fn instance_problems(spec: &InstanceSpec<'_>, t: &McpMarketTemplate, secrets_set: &[String]) -> Vec<String> {
    let mut p = Vec::new();
    let w = format!("instance \"{}\"", spec.name);
    if let Some(problem) = name_problem(spec.name) {
        p.push(problem);
    }
    if spec.name == t.id && !GLOBAL_IDS.contains(&spec.name) {
        p.push(format!("{w}: id must not equal its template id (R1)"));
    }
    if !ENVIRONMENTS.contains(&spec.environment) {
        p.push(format!("{w}: environment must be one of {}", ENVIRONMENTS.join("|")));
    }
    if !STATUSES.contains(&spec.status) {
        p.push(format!("{w}: status must be one of {}", STATUSES.join("|")));
    }
    if !TIERS.contains(&spec.tier) || !t.tiers.iter().any(|x| x == spec.tier) {
        p.push(format!(
            "{w}: tier must be one of the template's tiers ({})",
            t.tiers.join("|")
        ));
    }
    if spec.clis.is_empty() {
        p.push(format!("{w}: clis must not be empty"));
    }
    for cli in spec.clis {
        if !CLIS.contains(&cli.as_str()) || !t.clis.contains(cli) {
            p.push(format!("{w}: cli \"{cli}\" is not supported by the template"));
        }
    }
    // R3 relaxed by D50: write (full tools) runs on every CLI the template supports and a
    // production write instance may be active. The guard is the rule that a production
    // write needs the owner's typed OK per change; `approval_ref` is an optional record.
    if let Some(r) = spec.approval_ref
        && !is_valid_approval_ref(r)
    {
        p.push(format!(
            "{w}: approval_ref must be an owner approval id OA-<yyyymmdd>-<slug>"
        ));
    }
    // R6: allowlist for read, blocklist for write; production read needs server/credential enforcement.
    match (spec.tier, spec.tools_allow) {
        ("read", None) => p.push(format!(
            "{w}: read instances need tools_allow (allowlist, fail closed; R6)"
        )),
        ("write", Some(_)) => p.push(format!("{w}: write instances use tools_block, not tools_allow (R6)")),
        _ => {}
    }
    if spec.tier == "read" && spec.environment == "production" && !READ_ENFORCEMENTS.contains(&t.enforcement.as_str()) {
        p.push(format!(
            "{w}: production read instances need enforcement {} (R6)",
            READ_ENFORCEMENTS.join("|")
        ));
    }
    // G3: a proxy-enforced read instance allows only the template's pinned read tools.
    if spec.tier == "read" && t.enforcement == "proxy" {
        for tool in spec.tools_allow.unwrap_or_default() {
            if !t.read_tools.contains(tool) {
                p.push(format!(
                    "{w}: tool \"{tool}\" is not in the template's read allowlist (G3)"
                ));
            }
        }
    }
    for tool in spec.tools_allow.unwrap_or_default().iter().chain(spec.tools_block) {
        if !is_valid_tool_name(tool) {
            p.push(format!("{w}: invalid tool name \"{tool}\""));
        }
    }
    for tool in spec.tools_allow.unwrap_or_default() {
        let qualified = format!("mcp__{}__{tool}", spec.name);
        if qualified.len() > TOOL_QUALIFIED_MAX {
            p.push(format!(
                "{w}: qualified tool name {qualified} is {} chars (> {TOOL_QUALIFIED_MAX})",
                qualified.len()
            ));
        }
    }
    // R4: plain env holds only declared plain keys, never a credential or a ref.
    for (key, value) in spec.plain_env {
        match t.env.get(key) {
            None => p.push(format!("{w}: env key {key} is not declared by the template")),
            Some(field) if field.kind == McpMarketEnvKind::Secret => p.push(format!(
                "{w}: env.{key} is a secret; set it through the secrets route, never as a plain value (R4)"
            )),
            Some(field) => {
                if has_control(value) || value.contains("${") {
                    p.push(format!("{w}: env.{key} must be a plain literal"));
                } else if looks_like_secret_value(value) || (is_secret_key(key) && value.len() >= 24) {
                    p.push(format!("{w}: env.{key} looks like a credential (R4)"));
                } else if !field.allowed.is_empty() && !field.allowed.contains(value) {
                    p.push(format!("{w}: env.{key} must be one of {}", field.allowed.join("|")));
                } else if !value_matches(field, value) {
                    p.push(format!("{w}: env.{key} does not match the template pattern (G1)"));
                }
            }
        }
    }
    if let Some(forced) = t.tier_env.get(spec.tier) {
        for (key, value) in forced {
            if spec.plain_env.get(key) != Some(value) {
                p.push(format!("{w}: tier {} forces env.{key} = {value}", spec.tier));
            }
        }
    }
    // Activation: every required key has a value.
    if spec.status == "active" {
        for (key, field) in &t.env {
            let present = match field.kind {
                McpMarketEnvKind::Plain => spec.plain_env.contains_key(key),
                McpMarketEnvKind::Secret => secrets_set.contains(key),
            };
            if field.required && !present {
                p.push(format!("{w}: required env {key} has no value; it cannot be active"));
            }
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["tb-paddle-prod-read", "context7", "a1"] {
            assert!(is_valid_name(ok), "{ok}");
        }
        for bad in [
            "Tb",
            "tb_paddle",
            "tb--x",
            "tb-",
            "-tb",
            "1tb",
            "a-very-long-instance-name-x",
        ] {
            assert!(!is_valid_name(bad), "{bad}");
        }
        assert!(name_problem("aionui-team").is_some());
        assert!(name_problem("playwright-cloud").is_some());
        assert!(name_problem("sequential-thinking").is_some());
        // G2: the owner picked GitHub and Chrome DevTools (D49/D50).
        assert!(name_problem("github").is_none());
        assert!(name_problem("chrome-devtools").is_none());
        assert!(name_problem("tb-github-read").is_none());
    }

    #[test]
    fn approval_refs() {
        assert!(is_valid_approval_ref("OA-20261115-paddle-prod"));
        assert!(!is_valid_approval_ref("OA-2026111-paddle"));
        assert!(!is_valid_approval_ref("OA-20261115-"));
        assert!(!is_valid_approval_ref("ok"));
    }

    #[test]
    fn ref_names() {
        assert_eq!(
            secret_ref_name("tb-paddle-prod-read", "PADDLE_API_KEY"),
            "TB_PADDLE_PROD_READ__PADDLE_API_KEY"
        );
        assert_eq!(refs_in("Authorization:${TAVILY_AUTH}"), vec!["TAVILY_AUTH"]);
    }
}
