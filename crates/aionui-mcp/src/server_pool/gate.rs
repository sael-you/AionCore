//! The per-call decision of the built-in server MCPs (C36 §4, owner changes
//! 2026-11-17). Pure: the caller loads the server row, its grant and the key.
//!
//! - Not granted to the conversation now, switched off, deleted, no key or no
//!   host key → refused.
//! - The crucial channel (`aionui-servers-crucial`, every call asked to the
//!   owner by the CLI through the TASK-97 ask path) → runs; the agent may use
//!   it to declare a command crucial itself.
//! - The standard channel (`aionui-servers`) on a `production` server with
//!   the `write` tier → a crucial command is not run; the agent is told to
//!   send it through the crucial channel. Everything else runs.
//!
//! The tier is guidance only (owner decision): it never blocks a command.

use aionui_db::models::ServerRow;

use super::classifier::classify;
use crate::secret_mask::{MASK, is_secret_key, looks_like_secret_value};

/// MCP server name of the standard channel.
pub const STANDARD_SERVER_NAME: &str = "aionui-servers";
/// MCP server name of the crucial channel (`require_approval`).
pub const CRUCIAL_SERVER_NAME: &str = "aionui-servers-crucial";

/// Which built-in MCP server the call came through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecChannel {
    Standard,
    Crucial,
}

impl ExecChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecChannel::Standard => "standard",
            ExecChannel::Crucial => "crucial",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "standard" => Some(ExecChannel::Standard),
            "crucial" => Some(ExecChannel::Crucial),
            _ => None,
        }
    }

    pub fn server_name(self) -> &'static str {
        match self {
            ExecChannel::Standard => STANDARD_SERVER_NAME,
            ExecChannel::Crucial => CRUCIAL_SERVER_NAME,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecDecision {
    /// Run it; `crucial` = the classifier's (or the agent's) reason, if any.
    Run {
        crucial: Option<String>,
    },
    /// Crucial on a production write server, called on the standard channel.
    NeedsApproval(String),
    Refuse(String),
}

impl ExecDecision {
    /// `server_audit.decision` before the run (`ran`/`failed` replace `run`).
    pub fn audit_label(&self) -> &'static str {
        match self {
            ExecDecision::Run { .. } => "ran",
            ExecDecision::NeedsApproval(_) => "needs_approval",
            ExecDecision::Refuse(_) => "refused",
        }
    }
}

/// Longest command accepted (bytes).
pub const COMMAND_MAX: usize = 16 * 1024;

/// Decide one call. `server` = the live row (`None` = unknown or deleted),
/// `granted` = the conversation's effective set includes it now.
pub fn decide(channel: ExecChannel, server: Option<&ServerRow>, granted: bool, command: &str) -> ExecDecision {
    let Some(server) = server else {
        return ExecDecision::Refuse("unknown server; call list_servers for the servers of this conversation".into());
    };
    if !granted {
        return ExecDecision::Refuse(format!(
            "server {} is not assigned to this conversation (any more)",
            server.name
        ));
    }
    if !server.enabled {
        return ExecDecision::Refuse(format!("server {} is switched off in Settings → Servers", server.name));
    }
    if !server.has_key {
        return ExecDecision::Refuse(format!("server {} has no private key stored", server.name));
    }
    if server.host_key.trim().is_empty() {
        return ExecDecision::Refuse(format!(
            "server {} has no host key yet; the owner sets or scans it in Settings → Servers",
            server.name
        ));
    }
    if command.trim().is_empty() {
        return ExecDecision::Refuse("empty command".into());
    }
    if command.len() > COMMAND_MAX {
        return ExecDecision::Refuse(format!("command longer than {COMMAND_MAX} bytes"));
    }
    let reason = classify(command);
    match channel {
        ExecChannel::Crucial => ExecDecision::Run {
            crucial: Some(reason.unwrap_or_else(|| "declared crucial by the agent".to_owned())),
        },
        ExecChannel::Standard => match reason {
            Some(reason) if asks_for_crucial(server) => ExecDecision::NeedsApproval(format!(
                "crucial command ({reason}) on production write server {}: not run. Send the same call to \
                 the {CRUCIAL_SERVER_NAME} MCP server's run_command tool; the owner is asked to approve it first.",
                server.name
            )),
            other => ExecDecision::Run { crucial: other },
        },
    }
}

/// `production` + `write`: crucial commands go through the crucial channel.
pub fn asks_for_crucial(server: &ServerRow) -> bool {
    server.environment == "production" && server.tier == "write"
}

/// Longest command kept in the audit trail (chars).
const AUDIT_COMMAND_MAX: usize = 2000;

/// The command as stored in `server_audit`: credential-looking words and
/// `KEY=value` with a secret-looking key masked, cut to 2000 chars.
pub fn redact_command(command: &str) -> String {
    let redacted: Vec<String> = command
        .split(' ')
        .map(|word| {
            if let Some((key, value)) = word.split_once('=')
                && !value.is_empty()
                && (is_secret_key(key.trim_start_matches('-')) || looks_like_secret_value(value))
            {
                return format!("{key}={MASK}");
            }
            if looks_like_secret_value(word.trim_matches(|c| c == '\'' || c == '"')) {
                return MASK.to_owned();
            }
            word.to_owned()
        })
        .collect();
    let joined = redacted.join(" ");
    if joined.chars().count() <= AUDIT_COMMAND_MAX {
        return joined;
    }
    let mut out: String = joined.chars().take(AUDIT_COMMAND_MAX).collect();
    out.push_str(" [truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(environment: &str, tier: &str) -> ServerRow {
        ServerRow {
            id: "srv_1".into(),
            user_id: "u".into(),
            name: "web-1".into(),
            host: "10.0.0.5".into(),
            port: 22,
            ssh_user: "deploy".into(),
            host_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample".into(),
            labels: "[]".into(),
            environment: environment.into(),
            tier: tier.into(),
            notes: String::new(),
            enabled: true,
            has_key: true,
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        }
    }

    #[test]
    fn crucial_on_production_write_needs_the_crucial_channel() {
        let prod = server("production", "write");
        let decision = decide(ExecChannel::Standard, Some(&prod), true, "systemctl restart nginx");
        assert!(matches!(&decision, ExecDecision::NeedsApproval(msg) if msg.contains(CRUCIAL_SERVER_NAME)));
        assert_eq!(decision.audit_label(), "needs_approval");
        // Same command through the crucial channel (the CLI asked the owner) runs.
        assert_eq!(
            decide(ExecChannel::Crucial, Some(&prod), true, "systemctl restart nginx"),
            ExecDecision::Run {
                crucial: Some("stops, restarts or disables a service".into())
            }
        );
        // Routine commands never ask.
        assert_eq!(
            decide(ExecChannel::Standard, Some(&prod), true, "uptime"),
            ExecDecision::Run { crucial: None }
        );
    }

    #[test]
    fn agent_may_declare_a_command_crucial() {
        let prod = server("production", "write");
        assert_eq!(
            decide(ExecChannel::Crucial, Some(&prod), true, "uptime"),
            ExecDecision::Run {
                crucial: Some("declared crucial by the agent".into())
            }
        );
    }

    #[test]
    fn tier_and_staging_never_block() {
        // Read tier on production is guidance only: the command runs (audited).
        for (env, tier) in [("production", "read"), ("staging", "write"), ("sandbox", "read")] {
            let s = server(env, tier);
            assert_eq!(
                decide(ExecChannel::Standard, Some(&s), true, "systemctl restart nginx"),
                ExecDecision::Run {
                    crucial: Some("stops, restarts or disables a service".into())
                },
                "{env}/{tier}"
            );
        }
    }

    #[test]
    fn refusals_name_the_reason() {
        let s = server("staging", "read");
        assert!(matches!(
            decide(ExecChannel::Standard, None, true, "uptime"),
            ExecDecision::Refuse(_)
        ));
        assert!(
            matches!(decide(ExecChannel::Standard, Some(&s), false, "uptime"), ExecDecision::Refuse(m) if m.contains("not assigned"))
        );
        assert!(
            matches!(decide(ExecChannel::Crucial, Some(&s), false, "uptime"), ExecDecision::Refuse(m) if m.contains("not assigned"))
        );
        let mut off = s.clone();
        off.enabled = false;
        assert!(
            matches!(decide(ExecChannel::Standard, Some(&off), true, "uptime"), ExecDecision::Refuse(m) if m.contains("switched off"))
        );
        let mut no_key = s.clone();
        no_key.has_key = false;
        assert!(
            matches!(decide(ExecChannel::Standard, Some(&no_key), true, "uptime"), ExecDecision::Refuse(m) if m.contains("private key"))
        );
        let mut no_host = s.clone();
        no_host.host_key = String::new();
        assert!(
            matches!(decide(ExecChannel::Standard, Some(&no_host), true, "uptime"), ExecDecision::Refuse(m) if m.contains("host key"))
        );
        assert!(matches!(
            decide(ExecChannel::Standard, Some(&s), true, "  "),
            ExecDecision::Refuse(_)
        ));
        let long = "x".repeat(COMMAND_MAX + 1);
        assert!(matches!(
            decide(ExecChannel::Standard, Some(&s), true, &long),
            ExecDecision::Refuse(_)
        ));
    }

    #[test]
    fn audit_command_is_redacted() {
        let out = redact_command(
            "curl -H 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789' https://x PGPASSWORD=hunter2hunter2 psql",
        );
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{out}");
        assert!(!out.contains("hunter2hunter2"), "{out}");
        assert!(out.contains("psql"));
        assert!(redact_command(&"a ".repeat(3000)).ends_with("[truncated]"));
    }
}
