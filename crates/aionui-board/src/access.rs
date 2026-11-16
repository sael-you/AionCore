//! Agent gate access (TASK-116): how far agents (the Board Butler/Keeper and
//! assigned conversations) may move cards across gate columns.
//!
//! Stored as `settings.agent_access` of the board: `full` = agents move cards
//! into, out of and past every gate; `approval` = only with an owner approval
//! note on the move (`aioncore board move … --approval-note "<text>"`), recorded
//! on the card's history as `{by: "owner", via, at}`. Unset → `full` for the
//! owner's clauterm-studio and TB-test projects, `approval` elsewhere.
//! `deploying` stays closed either way: it is entered only through the deploy
//! approval of a deploy job.

use aionui_api_types::AgentAccess;
use serde_json::Value;

use crate::error::{BoardError, BoardResult};

pub const SETTINGS_KEY: &str = "agent_access";
/// Projects whose boards default to `full` (owner, 2026-11-16).
pub const FULL_ACCESS_PROJECTS: &[&str] = &["clauterm-studio", "TB-test"];
pub const APPROVAL_NOTE_MAX: usize = 500;

pub fn default_for(project_name: &str) -> AgentAccess {
    if FULL_ACCESS_PROJECTS
        .iter()
        .any(|name| name.eq_ignore_ascii_case(project_name.trim()))
    {
        AgentAccess::Full
    } else {
        AgentAccess::Approval
    }
}

/// The access a board grants: the stored value, else the project's default.
pub fn effective(settings: &Value, project_name: &str) -> AgentAccess {
    settings
        .get(SETTINGS_KEY)
        .and_then(Value::as_str)
        .and_then(AgentAccess::parse)
        .unwrap_or_else(|| default_for(project_name))
}

/// A settings write may set `agent_access` only to `full` or `approval`.
pub fn validate(settings: &Value) -> BoardResult<()> {
    match settings.get(SETTINGS_KEY) {
        None | Some(Value::Null) => Ok(()),
        Some(value) if value.as_str().and_then(AgentAccess::parse).is_some() => Ok(()),
        Some(_) => Err(BoardError::invalid(
            "settings.agent_access",
            "agent_access must be \"full\" or \"approval\"",
        )),
    }
}

/// May an agent move across a gate with this access and note?
pub fn permits(access: AgentAccess, approval_note: Option<&str>) -> bool {
    match access {
        AgentAccess::Full => true,
        AgentAccess::Approval => approval_note.is_some_and(|note| !note.trim().is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_owners_two_projects_default_to_full_and_others_to_approval() {
        assert_eq!(default_for("clauterm-studio"), AgentAccess::Full);
        assert_eq!(default_for("TB-test"), AgentAccess::Full);
        assert_eq!(default_for("tb-test"), AgentAccess::Full);
        assert_eq!(default_for("aionui"), AgentAccess::Approval);
        assert_eq!(effective(&json!({}), "aionui"), AgentAccess::Approval);
        assert_eq!(effective(&json!({"agent_access": "full"}), "aionui"), AgentAccess::Full);
        assert_eq!(
            effective(&json!({"agent_access": "approval"}), "TB-test"),
            AgentAccess::Approval,
            "a stored value beats the default"
        );
    }

    #[test]
    fn only_full_or_approval_can_be_stored() {
        assert!(validate(&json!({})).is_ok());
        assert!(validate(&json!({"agent_access": "full"})).is_ok());
        assert!(validate(&json!({"agent_access": "approval"})).is_ok());
        assert!(validate(&json!({"agent_access": "owner_only"})).is_err());
        assert!(validate(&json!({"agent_access": true})).is_err());
    }

    #[test]
    fn approval_needs_a_non_empty_note() {
        assert!(permits(AgentAccess::Full, None));
        assert!(!permits(AgentAccess::Approval, None));
        assert!(!permits(AgentAccess::Approval, Some("  ")));
        assert!(permits(AgentAccess::Approval, Some("owner in chat: go")));
    }
}
