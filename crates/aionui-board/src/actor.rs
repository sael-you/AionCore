//! The three board actors (SPEC §2) and how each is identified.

use aionui_api_types::BoardActorKind;
use serde_json::Value;

/// `actor_ref` of the Studio supervisor.
pub const SUPERVISOR_REF: &str = "supervisor";

/// An agent caller resolved from a validated runtime token (R7): its
/// conversation, the project it belongs to and its `actor_ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCaller {
    pub user_id: String,
    pub conversation_id: String,
    pub project_id: String,
    /// `assistant:<id>@<backend>`, read from the conversation row.
    pub actor_ref: String,
    /// The caller's assistant id (the `me` of `list --assignee me`).
    pub assistant_id: Option<String>,
    /// The caller is the project's Board Keeper conversation (D40): it may
    /// move cards between non-gate columns regardless of `agent_move`.
    pub keeper: bool,
}

/// Who performs a board operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardActor {
    /// The owner through the user router.
    User { user_id: String },
    /// The Studio supervisor: user router + `x-aionui-board-actor:
    /// supervisor`, Local mode only.
    System { user_id: String },
    /// An agent through the agent router.
    Agent(AgentCaller),
}

impl BoardActor {
    pub fn user(user_id: impl Into<String>) -> Self {
        Self::User {
            user_id: user_id.into(),
        }
    }

    pub fn system(user_id: impl Into<String>) -> Self {
        Self::System {
            user_id: user_id.into(),
        }
    }

    /// The server principal every repository call is scoped by (R7b).
    pub fn user_id(&self) -> &str {
        match self {
            Self::User { user_id } | Self::System { user_id } => user_id,
            Self::Agent(caller) => &caller.user_id,
        }
    }

    pub fn kind(&self) -> BoardActorKind {
        match self {
            Self::User { .. } => BoardActorKind::User,
            Self::System { .. } => BoardActorKind::System,
            Self::Agent(_) => BoardActorKind::Agent,
        }
    }

    /// user → user id; system → `supervisor`; agent → `assistant:<id>@<backend>`.
    pub fn actor_ref(&self) -> &str {
        match self {
            Self::User { user_id } => user_id,
            Self::System { .. } => SUPERVISOR_REF,
            Self::Agent(caller) => &caller.actor_ref,
        }
    }

    /// The agent's conversation (recorded on its events for chat notices).
    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::Agent(caller) => Some(&caller.conversation_id),
            _ => None,
        }
    }

    pub fn as_agent(&self) -> Option<&AgentCaller> {
        match self {
            Self::Agent(caller) => Some(caller),
            _ => None,
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }
}

/// Build `assistant:<id>@<backend>` from the caller's conversation row.
///
/// The assistant id comes from the conversation's assistant snapshot, then
/// `extra.preset_assistant_id` / `extra.assistant_id`; the backend from
/// `extra.backend`, else the conversation type. Never from a request body.
pub fn agent_actor_ref(assistant_id: Option<&str>, extra: &Value, conversation_type: &str) -> String {
    let text = |key: &str| {
        extra
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let assistant = assistant_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| text("preset_assistant_id"))
        .or_else(|| text("assistant_id"))
        .unwrap_or("unknown");
    let backend = text("backend").unwrap_or(conversation_type);
    format!("assistant:{assistant}@{backend}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn actor_refs_follow_the_spec() {
        assert_eq!(BoardActor::user("u1").actor_ref(), "u1");
        assert_eq!(BoardActor::system("u1").actor_ref(), "supervisor");
        assert_eq!(BoardActor::system("u1").user_id(), "u1");
    }

    #[test]
    fn agent_ref_prefers_the_snapshot_then_extra_then_type() {
        let extra = json!({ "backend": "claude", "preset_assistant_id": "preset" });
        assert_eq!(
            agent_actor_ref(Some("asst_1"), &extra, "acp"),
            "assistant:asst_1@claude"
        );
        assert_eq!(agent_actor_ref(None, &extra, "acp"), "assistant:preset@claude");
        assert_eq!(
            agent_actor_ref(Some("  "), &json!({}), "gemini"),
            "assistant:unknown@gemini"
        );
    }
}
