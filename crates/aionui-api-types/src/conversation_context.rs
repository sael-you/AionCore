//! Wire types of the conversation Context panel (TASK-158, design C41):
//! `GET /api/conversations/{id}/context`, `PUT …/auto-compact`, `POST …/compact`.

use serde::{Deserialize, Serialize};

/// Key in a conversation's `extra` holding the auto-compact setting
/// (an [`AutoCompactMode`] wire string). Absent = `auto`.
pub const EXTRA_AUTO_COMPACT: &str = "auto_compact";

/// When the agent summarises older messages by itself.
/// `auto` = the backend's default; a number = at that percent of the
/// auto-compact window; `off` = never by itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoCompactMode {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "50")]
    P50,
    #[serde(rename = "60")]
    P60,
    #[serde(rename = "70")]
    P70,
    #[serde(rename = "80")]
    P80,
    #[serde(rename = "off")]
    Off,
}

impl AutoCompactMode {
    /// The wire string (also what is stored in `extra`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::P50 => "50",
            Self::P60 => "60",
            Self::P70 => "70",
            Self::P80 => "80",
            Self::Off => "off",
        }
    }

    /// Parse a stored / requested value; `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "auto" => Self::Auto,
            "50" => Self::P50,
            "60" => Self::P60,
            "70" => Self::P70,
            "80" => Self::P80,
            "off" => Self::Off,
            _ => return None,
        })
    }

    /// The percent of the window, for the numeric modes.
    pub fn percent(self) -> Option<u8> {
        match self {
            Self::P50 => Some(50),
            Self::P60 => Some(60),
            Self::P70 => Some(70),
            Self::P80 => Some(80),
            Self::Auto | Self::Off => None,
        }
    }
}

/// `PUT /api/conversations/{id}/auto-compact` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetAutoCompactRequest {
    pub mode: AutoCompactMode,
}

/// The auto-compact block of the Context panel (also the PUT answer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoCompactView {
    /// `false`: this backend has no threshold setting — hide the select.
    pub supported: bool,
    pub mode: AutoCompactMode,
    /// Tokens at which the agent compacts by itself, when the backend reports it.
    pub threshold_tokens: Option<u64>,
    /// `threshold_tokens − used`, floored at 0.
    pub until_tokens: Option<u64>,
}

/// One row of the breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCategory {
    /// Stable id (`system_prompt`, `mcp_tools`, `messages`, `free_space`, …;
    /// `other` for one the backend did not name).
    pub id: String,
    /// i18n key: `conversation.context.category.<id>`.
    pub label_key: String,
    /// The backend's own English label (fallback text).
    pub name: String,
    pub tokens: u64,
    /// Listed but not loaded into the window (not part of `used`).
    pub deferred: bool,
    /// Estimated by AionUi; the agent did not report it.
    pub estimated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMcpToolItem {
    pub name: String,
    pub server: String,
    pub tokens: u64,
    pub loaded: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMemoryFileItem {
    pub path: String,
    pub kind: String,
    pub tokens: u64,
}

/// A custom agent or a skill: name, where it comes from, tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextNamedItem {
    pub name: String,
    pub source: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItems {
    pub mcp_tools: Vec<ContextMcpToolItem>,
    pub memory_files: Vec<ContextMemoryFileItem>,
    pub agents: Vec<ContextNamedItem>,
    pub skills: Vec<ContextNamedItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextCost {
    pub amount: f64,
    pub currency: String,
}

/// Where the breakdown comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    /// The agent CLI's own context report.
    Cli,
    /// Totals from the agent, split estimated by AionUi.
    Estimated,
}

/// `GET /api/conversations/{id}/context` (`null` before the first usage).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationContextResponse {
    pub used: u64,
    pub size: Option<u64>,
    /// `used / size`, 0–100 with one decimal; `None` when the size is unknown.
    pub percent: Option<f64>,
    pub model: Option<String>,
    pub auto_compact: AutoCompactView,
    pub categories: Vec<ContextCategory>,
    pub items: ContextItems,
    pub cost: Option<ContextCost>,
    pub source: ContextSource,
    pub updated_at: i64,
}

/// `POST /api/conversations/{id}/compact` answer (202): the compact turn was
/// started; the new numbers arrive over the WebSocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactConversationResponse {
    pub msg_id: String,
}
