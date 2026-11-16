//! MCP market (card C15) request/response types: templates, instances,
//! write-only secrets, assignments, status and the kit-lock snapshot.
//!
//! Secrets cross this boundary in one direction only: a
//! [`SetMcpMarketSecretRequest`] carries a value in; nothing ever carries one
//! out (responses hold names, `set` and `updated_at`).

use std::collections::BTreeMap;

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Templates (catalogue entries; `contributes.mcpServers[].market`)
// ---------------------------------------------------------------------------

/// Kind of a template env key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpMarketEnvKind {
    /// Stored encrypted in `mcp_secrets`; the instance holds a `${NAME}` ref.
    Secret,
    /// Stored as a plain value on the instance.
    Plain,
}

/// One env key a template declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpMarketEnvField {
    pub kind: McpMarketEnvKind,
    #[serde(default)]
    pub required: bool,
    /// Allowed values of a plain key (empty = free text).
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "enum")]
    pub allowed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Launch parameter (G1): an anchored regex every value of this plain key
    /// must fully match. Only a plain key with a pattern may be referenced as
    /// `${KEY}` in the pinned args or url; the launcher substitutes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// A stdio command (the pinned server command, or the stdio bridge an HTTP
/// template is run through by the launcher).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpMarketCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// A catalogue template: what a server is. Command/args/url are pinned here
/// and are the only launch data an instance ever uses (R1 #1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpMarketTemplate {
    pub id: String,
    pub name: String,
    /// Catalogue version of the template (e.g. the pinned package version);
    /// instances keep the snapshot they were created or last refreshed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `stdio` or `http`.
    pub transport: String,
    /// stdio: the pinned server command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<McpMarketCommand>,
    /// http: the server URL (no secrets in it, R1 #13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// http: header name -> env key whose value the header carries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// http with secrets: the pinned stdio bridge the launcher runs (it reads
    /// the URL and headers from its env).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<McpMarketCommand>,
    #[serde(default)]
    pub env: BTreeMap<String, McpMarketEnvField>,
    /// Tiers an instance may use (`read`, `write`).
    pub tiers: Vec<String>,
    /// Plain env the tier forces (e.g. `read` -> `PADDLE_MCP_TOOLS=read-only`);
    /// an instance of that tier cannot hold another value for these keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tier_env: BTreeMap<String, BTreeMap<String, String>>,
    /// How the tier is enforced: `server`, `credential` or `tools-only`.
    pub enforcement: String,
    /// CLIs the template supports.
    pub clis: Vec<String>,
    /// Tool name -> `read` | `write`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_classes: BTreeMap<String, String>,
    /// Pinned read allowlist (G3). With enforcement `proxy` a read instance
    /// may only allow these tools; the launcher's proxy enforces it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_tools: Vec<String>,
    /// Library browse metadata (D49): category slug (e.g. `developer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Who publishes the server (e.g. `Supabase`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Published by the vendor itself (vs a community server).
    #[serde(default)]
    pub official: bool,
    /// Optional pick (D50): shown apart in the library browse.
    #[serde(default)]
    pub optional: bool,
    /// One-line purpose for the library browse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// `metered` when calls spend money (G7); shown as a cost badge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<String>,
}

// ---------------------------------------------------------------------------
// Instances
// ---------------------------------------------------------------------------

/// Status of one secret key of an instance. Never carries the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpMarketSecretStatus {
    /// Env key in the template (e.g. `PADDLE_API_KEY`).
    pub key: String,
    /// Ref name (e.g. `TB_PADDLE_PROD_READ__PADDLE_API_KEY`).
    pub name: String,
    pub instance_id: String,
    pub instance_name: String,
    pub required: bool,
    pub set: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<TimestampMs>,
}

/// A market instance as the API shows it (refs only).
#[derive(Debug, Clone, Serialize)]
pub struct McpMarketInstanceResponse {
    pub id: String,
    /// Instance id = tool prefix (`mcp__<name>__*`).
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub template_id: String,
    /// Template version this instance was created or last refreshed from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_version: Option<String>,
    /// Version of the template in the catalogue now (`None` when the catalogue
    /// has no such template or no version, or is unavailable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_current_version: Option<String>,
    /// Whether the catalogue still offers this template.
    pub template_in_catalogue: bool,
    /// Whether the catalogue template differs from the instance's snapshot, i.e.
    /// `refresh-template` would change something.
    pub template_update_available: bool,
    pub transport: String,
    pub environment: String,
    pub tier: String,
    pub enforcement: String,
    pub status: String,
    pub clis: Vec<String>,
    pub plain_env: BTreeMap<String, String>,
    pub secrets: Vec<McpMarketSecretStatus>,
    /// Required keys (plain or secret) that have no value yet.
    pub missing: Vec<String>,
    /// Read tier allowlist; `None` for write instances (blocklist only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_allow: Option<Vec<String>>,
    pub tools_block: Vec<String>,
    /// Tool names seen at the last connection test.
    pub tools_seen: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_ref: Option<String>,
    pub last_test_status: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `POST /api/mcp/market/instances`. Launch data comes from the template,
/// never from this body (unknown fields such as `command` are rejected).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMcpMarketInstanceRequest {
    pub name: String,
    pub template_id: String,
    #[serde(default)]
    pub description: Option<String>,
    pub environment: String,
    pub tier: String,
    /// Defaults to `dormant`.
    #[serde(default)]
    pub status: Option<String>,
    pub clis: Vec<String>,
    #[serde(default)]
    pub plain_env: BTreeMap<String, String>,
    #[serde(default)]
    pub tools_allow: Option<Vec<String>>,
    #[serde(default)]
    pub tools_block: Vec<String>,
}

/// `PUT /api/mcp/market/instances/{id}` (partial). `approval_ref` has its own route.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMcpMarketInstanceRequest {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub clis: Option<Vec<String>>,
    #[serde(default)]
    pub plain_env: Option<BTreeMap<String, String>>,
    /// `Some(None)` is not expressible in JSON here; send `[]` for an empty allowlist.
    #[serde(default)]
    pub tools_allow: Option<Vec<String>>,
    #[serde(default)]
    pub tools_block: Option<Vec<String>>,
}

/// `POST /api/mcp/market/instances/{id}/duplicate`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuplicateMcpMarketInstanceRequest {
    pub name: String,
}

/// `POST /api/mcp/market/instances/{id}/refresh-template` — the instance now
/// runs the catalogue's current template; secrets and assignments are kept.
#[derive(Debug, Clone, Serialize)]
pub struct McpMarketTemplateRefreshResponse {
    pub instance: McpMarketInstanceResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_version: Option<String>,
    /// Env keys the new template declares that the old one did not.
    pub added_keys: Vec<String>,
    /// Env keys the old template declared that the new one does not (their
    /// plain values are dropped; stored secrets are kept but no longer used).
    pub removed_keys: Vec<String>,
    /// Whether anything in the template snapshot changed.
    pub changed: bool,
}

/// `PUT /api/mcp/market/instances/{id}/approval` — the owner's exception
/// record (`OA-<yyyymmdd>-<slug>`); `null` clears it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMcpMarketApprovalRequest {
    pub approval_ref: Option<String>,
}

/// `PUT /api/mcp/market/instances/{id}/secrets/{key}` — write-only.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMcpMarketSecretRequest {
    pub value: String,
}

impl std::fmt::Debug for SetMcpMarketSecretRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetMcpMarketSecretRequest { value: <redacted> }")
    }
}

// ---------------------------------------------------------------------------
// Assignments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct McpMarketAssignmentResponse {
    pub id: String,
    pub instance_id: String,
    pub instance_name: String,
    /// `None` = global.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// `None` = whole project (native files); `Some` = AionUi assistant binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
    pub cli: String,
    pub created_at: TimestampMs,
}

/// `POST /api/mcp/market/assignments` — one row per CLI.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMcpMarketAssignmentRequest {
    pub instance_id: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub assistant_id: Option<String>,
    pub clis: Vec<String>,
}

/// Query of `GET /api/mcp/market/assignments`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListMcpMarketAssignmentsQuery {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub instance_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Status / snapshot
// ---------------------------------------------------------------------------

/// One instance's health as the market rules see it now.
#[derive(Debug, Clone, Serialize)]
pub struct McpMarketInstanceStatus {
    pub id: String,
    pub name: String,
    pub status: String,
    /// Whether the instance can be delivered now (active, no problems).
    pub deliverable: bool,
    /// Rule problems, by key/name only (never a value).
    pub problems: Vec<String>,
    pub assignments: usize,
}

/// `GET /api/mcp/market/status`.
#[derive(Debug, Clone, Serialize)]
pub struct McpMarketStatusResponse {
    pub instances: Vec<McpMarketInstanceStatus>,
    /// Native-file drift needs `studio sync --check` (wired in slice S3).
    pub drift: Option<serde_json::Value>,
    pub drift_available: bool,
}

/// Query of `GET /api/mcp/market/snapshot`: a project lock, or the global one.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpMarketSnapshotQuery {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub global: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_request_debug_is_redacted() {
        let req: SetMcpMarketSecretRequest =
            serde_json::from_value(serde_json::json!({"value": "sk_live_SENTINEL"})).unwrap();
        assert!(!format!("{req:?}").contains("SENTINEL"));
    }

    #[test]
    fn create_request_rejects_client_launch_data() {
        let err = serde_json::from_value::<CreateMcpMarketInstanceRequest>(serde_json::json!({
            "name": "tb-x", "template_id": "paddle", "environment": "production", "tier": "read",
            "clis": ["claude"], "command": "/bin/sh"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn template_parses_env_enum_alias() {
        let t: McpMarketTemplate = serde_json::from_value(serde_json::json!({
            "id": "paddle", "name": "Paddle", "transport": "stdio",
            "command": {"command": "npx", "args": ["-y", "@paddle/paddle-mcp@0.1.0"]},
            "env": {"PADDLE_ENVIRONMENT": {"kind": "plain", "required": true, "enum": ["production", "sandbox"]},
                    "PADDLE_API_KEY": {"kind": "secret", "required": true}},
            "tiers": ["read", "write"], "enforcement": "server", "clis": ["claude", "codex", "gemini"]
        }))
        .unwrap();
        assert_eq!(t.env["PADDLE_ENVIRONMENT"].allowed, vec!["production", "sandbox"]);
        assert_eq!(t.env["PADDLE_API_KEY"].kind, McpMarketEnvKind::Secret);
    }
}
