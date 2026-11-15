//! Board error codes (SPEC §5) and the crate-owned error type.
//!
//! Routes map [`BoardError`] to `ApiError::coded` (user router) or to the CLI
//! envelope (agent router); the code string and HTTP status come from
//! [`BoardErrorCode`] so both routers answer the same contract.

use aionui_db::DbError;
use serde_json::Value;

macro_rules! board_codes {
    ($($(#[$meta:meta])* $variant:ident => ($wire:literal, $status:literal)),+ $(,)?) => {
        /// Every stable `board_*` error code with its HTTP status.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum BoardErrorCode {
            $($(#[$meta])* $variant),+
        }

        impl BoardErrorCode {
            /// Every code, in SPEC order.
            pub const ALL: &'static [BoardErrorCode] = &[$(BoardErrorCode::$variant),+];

            /// The wire value.
            pub fn as_str(self) -> &'static str {
                match self { $(BoardErrorCode::$variant => $wire),+ }
            }

            /// The HTTP status the code answers with.
            pub fn status(self) -> u16 {
                match self { $(BoardErrorCode::$variant => $status),+ }
            }
        }
    };
}

board_codes! {
    AssignmentInProgress => ("board_assignment_in_progress", 409),
    DeployInFlight => ("board_deploy_in_flight", 409),
    UseAgentRouter => ("board_use_agent_router", 403),
    /// 409 on the user router; agent writes answer 403 (see [`BoardError::archived`]).
    Archived => ("board_archived", 409),
    IdempotencyConflict => ("board_idempotency_conflict", 409),
    FeatureNotEnabled => ("board_feature_not_enabled", 403),
    DependencyCycle => ("board_dependency_cycle", 422),
    NotFound => ("board_not_found", 404),
    Disabled => ("board_disabled", 403),
    GateUseDecision => ("board_gate_use_decision", 409),
    SystemForbidden => ("board_system_forbidden", 403),
    AgentMoveDenied => ("board_agent_move_denied", 403),
    DepartmentMismatch => ("board_department_mismatch", 403),
    WrongProject => ("board_wrong_project", 403),
    VersionConflict => ("board_version_conflict", 409),
    WipLimit => ("board_wip_limit", 409),
    ExitRule => ("board_exit_rule", 409),
    NoProject => ("board_no_project", 409),
    Invalid => ("board_invalid", 422),
    RateLimited => ("board_rate_limited", 429),
    /// R5: a blocked card cannot move forward (code added by S1; the SPEC
    /// names the rule but no code).
    Blocked => ("board_blocked", 409),
    /// §6: a conversation is already linked to the card.
    AlreadyLinked => ("board_already_linked", 409),
}

/// A board failure: a stable code, a public message, optional details.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BoardError {
    #[error("{message}")]
    Rule {
        code: BoardErrorCode,
        /// Overrides the code's default status (e.g. `board_archived` 403 on
        /// the agent router).
        status: Option<u16>,
        message: String,
        details: Option<Value>,
    },
    /// An unexpected failure (database, serialization). The message is for
    /// logs only; clients see a generic 500.
    #[error("board internal error: {0}")]
    Internal(String),
}

impl BoardError {
    pub fn new(code: BoardErrorCode, message: impl Into<String>) -> Self {
        Self::Rule {
            code,
            status: None,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(self, details: Value) -> Self {
        match self {
            Self::Rule {
                code, status, message, ..
            } => Self::Rule {
                code,
                status,
                message,
                details: Some(details),
            },
            other => other,
        }
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(BoardErrorCode::NotFound, format!("{what} not found"))
    }

    pub fn invalid(field: &str, message: impl Into<String>) -> Self {
        Self::new(BoardErrorCode::Invalid, message).with_details(serde_json::json!({ "field": field }))
    }

    /// `board_archived`: 409 for the owner, 403 for agents (SPEC §6).
    pub fn archived(for_agent: bool) -> Self {
        Self::Rule {
            code: BoardErrorCode::Archived,
            status: for_agent.then_some(403),
            message: "the project is archived; the board is read-only".into(),
            details: None,
        }
    }

    pub fn code(&self) -> Option<BoardErrorCode> {
        match self {
            Self::Rule { code, .. } => Some(*code),
            Self::Internal(_) => None,
        }
    }

    /// HTTP status of this error.
    pub fn status(&self) -> u16 {
        match self {
            Self::Rule { code, status, .. } => status.unwrap_or_else(|| code.status()),
            Self::Internal(_) => 500,
        }
    }

    pub fn details(&self) -> Option<&Value> {
        match self {
            Self::Rule { details, .. } => details.as_ref(),
            Self::Internal(_) => None,
        }
    }

    /// The message safe to return to clients.
    pub fn public_message(&self) -> String {
        match self {
            Self::Rule { message, .. } => message.clone(),
            Self::Internal(_) => "Internal server error.".into(),
        }
    }
}

impl From<DbError> for BoardError {
    fn from(error: DbError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<serde_json::Error> for BoardError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

pub type BoardResult<T> = Result<T, BoardError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_statuses_match_the_spec_table() {
        let expected = [
            ("board_assignment_in_progress", 409),
            ("board_deploy_in_flight", 409),
            ("board_use_agent_router", 403),
            ("board_archived", 409),
            ("board_idempotency_conflict", 409),
            ("board_feature_not_enabled", 403),
            ("board_dependency_cycle", 422),
            ("board_not_found", 404),
            ("board_disabled", 403),
            ("board_gate_use_decision", 409),
            ("board_system_forbidden", 403),
            ("board_agent_move_denied", 403),
            ("board_department_mismatch", 403),
            ("board_wrong_project", 403),
            ("board_version_conflict", 409),
            ("board_wip_limit", 409),
            ("board_exit_rule", 409),
            ("board_no_project", 409),
            ("board_invalid", 422),
            ("board_rate_limited", 429),
            ("board_blocked", 409),
            ("board_already_linked", 409),
        ];
        assert_eq!(BoardErrorCode::ALL.len(), expected.len());
        for (code, (wire, status)) in BoardErrorCode::ALL.iter().zip(expected) {
            assert_eq!(code.as_str(), wire);
            assert_eq!(code.status(), status, "{wire}");
        }
    }

    #[test]
    fn archived_is_403_for_agents_and_409_for_the_owner() {
        assert_eq!(BoardError::archived(false).status(), 409);
        assert_eq!(BoardError::archived(true).status(), 403);
        assert_eq!(BoardError::archived(true).code(), Some(BoardErrorCode::Archived));
    }

    #[test]
    fn internal_errors_hide_their_cause() {
        let error = BoardError::Internal("no such table: board_cards".into());
        assert_eq!(error.status(), 500);
        assert_eq!(error.public_message(), "Internal server error.");
    }
}
