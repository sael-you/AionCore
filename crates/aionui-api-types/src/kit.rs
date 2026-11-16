//! Kits (card C27, D49/D50) request/response types: named bundles of library
//! skills + MCP market instances, attached with add/remove at four levels
//! (global → project → assistant → conversation), and the effective set a
//! conversation gets. Design: `clauterm-studio/design/skills-mcp/C27-KITS.md`.
//!
//! Wire names are snake_case (like the MCP market types). Request types are
//! `deny_unknown_fields`: the UI must send exactly these fields.

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
    /// `K1`…`K12` for the kits seeded from KITS.md; `None` for owner kits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_key: Option<String>,
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
