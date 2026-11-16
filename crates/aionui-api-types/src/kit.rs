//! Kits (card C27, D49/D50) request/response types: named bundles of library
//! skills + MCP market instances, attached with add/remove at four levels
//! (global → project → assistant → conversation), and the effective set a
//! conversation gets. Design: `clauterm-studio/design/skills-mcp/C27-KITS.md`.
//!
//! Wire names are snake_case (like the MCP market types). Request types are
//! `deny_unknown_fields`: the UI must send exactly these fields. Errors carry
//! a [`KitErrorCode`] in `ErrorResponse.code`.
//!
//! Targets: `project` = `projects.project_id` (the id the sidebar/project
//! routes use, = `conversations.project_id`); `assistant` = the assistant id of
//! `/api/assistants/{id}` (= `extra.preset_assistant_id` of a conversation);
//! `conversation` = the conversation id.

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Attach level, applied top-down; a lower level wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KitLevel {
    Global,
    Project,
    Assistant,
    Conversation,
}

impl KitLevel {
    /// Resolution order.
    pub const ORDER: [KitLevel; 4] = [
        KitLevel::Global,
        KitLevel::Project,
        KitLevel::Assistant,
        KitLevel::Conversation,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            KitLevel::Global => "global",
            KitLevel::Project => "project",
            KitLevel::Assistant => "assistant",
            KitLevel::Conversation => "conversation",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ORDER.into_iter().find(|l| l.as_str() == value)
    }
}

/// What an attachment points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KitItemKind {
    /// A whole kit (`item_ref` = kit id).
    Kit,
    /// One library skill (`item_ref` = skill name).
    Skill,
    /// One MCP market instance (`item_ref` = `mcp_servers.id`).
    Mcp,
}

impl KitItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KitItemKind::Kit => "kit",
            KitItemKind::Skill => "skill",
            KitItemKind::Mcp => "mcp",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [KitItemKind::Kit, KitItemKind::Skill, KitItemKind::Mcp]
            .into_iter()
            .find(|k| k.as_str() == value)
    }
}

/// Add or remove at a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KitOp {
    Add,
    Remove,
}

impl KitOp {
    pub fn as_str(self) -> &'static str {
        match self {
            KitOp::Add => "add",
            KitOp::Remove => "remove",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "add" => Some(KitOp::Add),
            "remove" => Some(KitOp::Remove),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

/// `ErrorResponse.code` of a refused kit request (with its HTTP status).
///
/// | code | status | when |
/// |---|---|---|
/// | `KIT_NAME_TAKEN` | 409 | create/rename: a live kit of the user already has this name (trimmed) |
/// | `KIT_PROJECT_HAS_KIT` | 409 | create with `project_id`: that project already has a live project kit (C28) |
/// | `KIT_STALE` | 409 | update: `expected_updated_at` no longer matches (re-read, retry) |
/// | `KIT_NOT_FOUND` | 404 | kit id unknown or deleted (GET/PUT/DELETE, or `item_kind: kit` attach) |
/// | `KIT_ATTACHMENT_NOT_FOUND` | 404 | DELETE of an unknown attachment id |
/// | `KIT_UNKNOWN_TARGET` | 404 | project/conversation `target_id` (or effective `conversation_id`) not found |
/// | `KIT_INVALID_NAME` | 400 | name empty or > 80 chars, description > 500 chars |
/// | `KIT_INVALID_ITEM` | 400 | bad skill name (`[A-Za-z0-9._-]{1,128}`), MCP instance not a live market instance of the user, template not in the catalogue, empty `item_ref`, > 200 members per kind |
/// | `KIT_INVALID_TARGET` | 400 | `target_id` missing for project/assistant/conversation, or present for global; a project kit attached at global, at another project, or at a conversation of another project (C28) |
/// | `KIT_GLOBAL_NOT_ALLOWED` | 400 | R2/R3: a global add of a non-global (not context7/studio-memory) or write MCP instance, directly or inside the kit; also a member update of a globally attached kit |
///
/// There is no duplicate-item error: member lists are trimmed and
/// de-duplicated, and attaching the same (level, target, kind, ref) again
/// upserts (flips `op`). Malformed JSON / unknown fields → 400 without a kit
/// code; a missing session → 401; a refused channel → 403
/// `MCP_MARKET_SESSION_REQUIRED`; Origin → 403 `ORIGIN_REQUIRED`/`ORIGIN_FORBIDDEN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KitErrorCode {
    NameTaken,
    ProjectHasKit,
    Stale,
    NotFound,
    AttachmentNotFound,
    UnknownTarget,
    InvalidName,
    InvalidItem,
    InvalidTarget,
    GlobalNotAllowed,
}

impl KitErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            KitErrorCode::NameTaken => "KIT_NAME_TAKEN",
            KitErrorCode::ProjectHasKit => "KIT_PROJECT_HAS_KIT",
            KitErrorCode::Stale => "KIT_STALE",
            KitErrorCode::NotFound => "KIT_NOT_FOUND",
            KitErrorCode::AttachmentNotFound => "KIT_ATTACHMENT_NOT_FOUND",
            KitErrorCode::UnknownTarget => "KIT_UNKNOWN_TARGET",
            KitErrorCode::InvalidName => "KIT_INVALID_NAME",
            KitErrorCode::InvalidItem => "KIT_INVALID_ITEM",
            KitErrorCode::InvalidTarget => "KIT_INVALID_TARGET",
            KitErrorCode::GlobalNotAllowed => "KIT_GLOBAL_NOT_ALLOWED",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            KitErrorCode::NameTaken | KitErrorCode::ProjectHasKit | KitErrorCode::Stale => 409,
            KitErrorCode::NotFound | KitErrorCode::AttachmentNotFound | KitErrorCode::UnknownTarget => 404,
            KitErrorCode::InvalidName
            | KitErrorCode::InvalidItem
            | KitErrorCode::InvalidTarget
            | KitErrorCode::GlobalNotAllowed => 400,
        }
    }
}

// ---------------------------------------------------------------------------
// Kits
// ---------------------------------------------------------------------------

/// One MCP instance member of a kit, with the instance's current market state
/// (`instance_name` empty and `missing` true when the instance was deleted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitMcpMember {
    pub instance_id: String,
    pub instance_name: String,
    #[serde(default)]
    pub missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitResponse {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `K0` (Global) and `K1`…`K12` for the seeded kits; `None` for owner kits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_key: Option<String>,
    /// `Some` = the project kit of that project (`projects.project_id`, C28):
    /// implicitly attached at its project; `None` = a shared kit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Library skill names, sorted.
    pub skills: Vec<String>,
    /// MCP market instances, sorted by id.
    pub mcps: Vec<KitMcpMember>,
    /// Wanted catalogue templates that have no instance in the kit yet
    /// (never delivered; "needs an instance").
    pub mcp_templates: Vec<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `POST /api/kits`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateKitRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub mcp_instance_ids: Vec<String>,
    #[serde(default)]
    pub mcp_templates: Vec<String>,
    /// C28: create the project kit of this project (one per project:
    /// `KIT_PROJECT_HAS_KIT` 409; unknown project: `KIT_UNKNOWN_TARGET` 404).
    /// Fixed once created (not in [`UpdateKitRequest`]).
    #[serde(default)]
    pub project_id: Option<String>,
}

/// Query of `GET /api/kits` (C28): `project_id` = only that project's kit;
/// omitted = every live kit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListKitsQuery {
    #[serde(default)]
    pub project_id: Option<String>,
}

/// `PUT /api/kits/{id}`. Every field optional; a list present REPLACES that
/// member set in one transaction. `expected_updated_at` (optional) → 409 when
/// the kit changed since the client read it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateKitRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_instance_ids: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_templates: Option<Vec<String>>,
    #[serde(default)]
    pub expected_updated_at: Option<TimestampMs>,
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitAttachmentResponse {
    pub id: String,
    pub level: KitLevel,
    /// `""` for global; project id, assistant id or conversation id otherwise.
    pub target_id: String,
    pub item_kind: KitItemKind,
    pub item_ref: String,
    /// Display name: kit name, skill name or MCP instance name (empty when the
    /// referenced kit/instance no longer exists).
    pub item_name: String,
    pub op: KitOp,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// `POST /api/kits/attachments` — upsert: the same (level, target, kind, ref)
/// attached again with the other `op` replaces the row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachKitItemRequest {
    pub level: KitLevel,
    /// Required for project/assistant/conversation; must be absent or empty for global.
    #[serde(default)]
    pub target_id: Option<String>,
    pub item_kind: KitItemKind,
    pub item_ref: String,
    pub op: KitOp,
}

/// Query of `GET /api/kits/attachments`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListKitAttachmentsQuery {
    #[serde(default)]
    pub level: Option<KitLevel>,
    #[serde(default)]
    pub target_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Effective set
// ---------------------------------------------------------------------------

/// Query of `GET /api/kits/effective`: either a conversation (its project and
/// assistant are read from it) or a project/assistant pair (preview for a new
/// conversation).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KitEffectiveQuery {
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub assistant_id: Option<String>,
}

/// Why an item is in (or out of) the set: the LAST level that decided it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitProvenance {
    pub level: KitLevel,
    pub op: KitOp,
    /// The kit that carried it; `None` = attached as a single item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kit_name: Option<String>,
}

/// MCP details for the "what this conversation gets" view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitEffectiveMcp {
    pub instance_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// `active | dormant | revoked`; empty when the instance is gone.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// `server | credential | tools-only` (how read-only is enforced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforcement: Option<String>,
    pub clis: Vec<String>,
    /// `true` when a session start would launch it (included, active, not
    /// blocked by R2/R3); `reason` says why not otherwise.
    pub delivered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// C28: the conversation's workspace `.mcp.json` defines a server with the
    /// same name, so a CLI that also reads that file loads it twice. Only the
    /// file's server names are read, never its content. Always `false` for a
    /// project/assistant preview (no workspace).
    #[serde(default)]
    pub shadowed_by_project_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitEffectiveItem {
    pub kind: KitItemKind,
    /// Skill name or MCP instance id.
    pub item_ref: String,
    /// `true` = in the set; `false` = removed by `provenance`.
    pub included: bool,
    pub provenance: KitProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<KitEffectiveMcp>,
}

/// A template a contributing kit wants but has no instance for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitMissingTemplate {
    pub template_id: String,
    pub kit_id: String,
    pub kit_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitEffectiveSetResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
    /// Skills decided by kits/attachments (sorted by name).
    pub skills: Vec<KitEffectiveItem>,
    /// MCP instances decided by kits/attachments (sorted by instance id).
    pub mcps: Vec<KitEffectiveItem>,
    /// The conversation's frozen skill snapshot (`extra.skills`, base + kits
    /// applied at creation); `None` for a project/assistant preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_skills: Option<Vec<String>>,
    pub missing_templates: Vec<KitMissingTemplate>,
}

// ---------------------------------------------------------------------------
// Import keys from a project's .mcp.json (C28 §5)
// ---------------------------------------------------------------------------

/// How a `.mcp.json` server maps onto the AionUi catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KitImportMatch {
    /// A catalogue template (by package/command identity or URL host, then by name).
    Catalogue,
    /// No template: a custom server (not importable here; see the custom-template path).
    Custom,
    /// A server the market does not offer (R11 / dropped by D50).
    None,
}

/// One secret field of the matched template and whether the file holds a
/// usable value for it (never the value, never a masked value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitImportSecret {
    /// Template env key (e.g. `SUPABASE_ACCESS_TOKEN`).
    pub field: String,
    pub found: bool,
}

/// One server of `<workspace>/.mcp.json`. Never carries a value, a masked
/// value, the command, args or URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitImportPreviewServer {
    /// The server's key in `mcpServers`.
    pub name: String,
    pub template_id: Option<String>,
    #[serde(rename = "match")]
    pub match_kind: KitImportMatch,
    /// The template's secret fields (empty without a template).
    pub secrets: Vec<KitImportSecret>,
    /// A remote server configured without any credential in the file (its
    /// CLI used OAuth): connect it in the UI instead.
    pub oauth_only: bool,
}

/// `GET /api/projects/{project_id}/kit/import-preview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitImportPreviewResponse {
    pub project_id: String,
    /// `false` when the project has no workspace or the workspace no readable
    /// `.mcp.json` (then `servers` is empty).
    pub file_found: bool,
    pub servers: Vec<KitImportPreviewServer>,
}

/// One server to import.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KitImportServerRequest {
    /// The server's key in `.mcp.json`.
    pub name: String,
    /// Catalogue template the instance is created from (its pinned command).
    pub template_id: String,
    /// New market instance id (R1 #14; must differ from the template id, R1).
    pub instance_name: String,
    /// `production | staging | sandbox | local`.
    pub environment: String,
    /// `read | write`.
    pub tier: String,
}

/// `POST /api/projects/{project_id}/kit/import`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KitImportRequest {
    pub servers: Vec<KitImportServerRequest>,
}

/// Per-server import result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitImportServerResult {
    pub name: String,
    /// The created (dormant) instance, added to the project kit; `None` on error.
    pub instance_id: Option<String>,
    /// `None` on success; else a market or kit code (`MCP_MARKET_INVALID`,
    /// `MCP_MARKET_CONFLICT`, `KIT_IMPORT_NOT_IN_FILE`, `KIT_IMPORT_UNKNOWN_TEMPLATE`, ...).
    pub error_code: Option<String>,
    /// Human-readable reason for `error_code` (names keys and rules, never a value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Required secret fields the file did not provide: the instance stays
    /// dormant until the owner sets them (`needs_secret`).
    #[serde(default)]
    pub missing_secrets: Vec<String>,
}

/// Response of `POST /api/projects/{project_id}/kit/import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitImportResponse {
    /// The project kit the instances were added to (created when missing).
    pub kit_id: String,
    pub servers: Vec<KitImportServerResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_lowercase() {
        assert_eq!(serde_json::to_string(&KitLevel::Assistant).unwrap(), "\"assistant\"");
        assert_eq!(KitLevel::parse("conversation"), Some(KitLevel::Conversation));
        assert_eq!(KitItemKind::parse("mcp"), Some(KitItemKind::Mcp));
        assert_eq!(KitOp::parse("remove"), Some(KitOp::Remove));
        assert_eq!(KitOp::parse("drop"), None);
    }

    #[test]
    fn import_wire_shapes() {
        let server = KitImportPreviewServer {
            name: "supabase".into(),
            template_id: Some("supabase".into()),
            match_kind: KitImportMatch::Catalogue,
            secrets: vec![KitImportSecret {
                field: "SUPABASE_ACCESS_TOKEN".into(),
                found: true,
            }],
            oauth_only: false,
        };
        let v = serde_json::to_value(&server).unwrap();
        assert_eq!(v["match"], "catalogue");
        assert!(v.get("match_kind").is_none());
        let bad = serde_json::from_str::<KitImportRequest>(
            r#"{"servers":[{"name":"a","template_id":"b","instance_name":"c","environment":"production","tier":"read","command":"x"}]}"#,
        );
        assert!(bad.is_err(), "a client command is never accepted");
    }

    #[test]
    fn project_kit_wire_fields() {
        let req: CreateKitRequest = serde_json::from_str(r#"{"name":"tb","project_id":"p1"}"#).unwrap();
        assert_eq!(req.project_id.as_deref(), Some("p1"));
        let shared: CreateKitRequest = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert!(shared.project_id.is_none());
        assert_eq!(KitErrorCode::ProjectHasKit.as_str(), "KIT_PROJECT_HAS_KIT");
        assert_eq!(KitErrorCode::ProjectHasKit.http_status(), 409);
        let mcp: KitEffectiveMcp =
            serde_json::from_str(r#"{"instance_name":"a","status":"active","clis":[],"delivered":true}"#).unwrap();
        assert!(!mcp.shadowed_by_project_file);
    }

    #[test]
    fn attach_request_rejects_unknown_fields() {
        let ok: AttachKitItemRequest = serde_json::from_str(
            r#"{"level":"project","target_id":"p1","item_kind":"kit","item_ref":"k1","op":"add"}"#,
        )
        .unwrap();
        assert_eq!(ok.level, KitLevel::Project);
        let bad = serde_json::from_str::<AttachKitItemRequest>(
            r#"{"level":"project","target":"p1","item_kind":"kit","item_ref":"k1","op":"add"}"#,
        );
        assert!(bad.is_err());
    }
}
