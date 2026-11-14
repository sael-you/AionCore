//! The board's seam to the host application (same-layer interaction through
//! traits only): conversations, teams, assistants and project folders live in
//! other domain crates; `aionui-app` implements this trait over them.

use async_trait::async_trait;

/// A conversation assign creates for a card (SPEC §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentConversation {
    pub assistant_id: String,
    /// `TASK-N · <department>`.
    pub name: String,
    /// The project's `role=workspace` folder path (exact).
    pub workspace: String,
    /// The card's lead conversation when eligible.
    pub parent_conversation_id: Option<String>,
    /// Stored as `extra.board_assignment_id` (find-or-create key).
    pub assignment_id: String,
    /// Tagged skills the user has (`extra.preset_enabled_skills`, merged with
    /// the assistant's own).
    pub skills: Vec<String>,
}

/// The Board Keeper conversation of a project (D40).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeeperConversation {
    pub assistant_id: String,
    /// `<project> · Board`.
    pub name: String,
    pub workspace: String,
}

/// A team as assign sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTeam {
    pub team_id: String,
    pub name: String,
    pub lead_conversation_id: Option<String>,
}

/// Why creating a conversation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// The unique `board_assignment_id` index fired: an earlier attempt
    /// created the conversation (find it and resume).
    Duplicate,
    /// The referenced assistant does not exist or is disabled.
    AssistantUnavailable(String),
    Failed(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate => f.write_str("a conversation already exists for this assignment"),
            Self::AssistantUnavailable(id) => write!(f, "assistant `{id}` is not available"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

#[async_trait]
pub trait BoardHostPort: Send + Sync {
    /// Run the conversation service's lazy project bind (R7).
    async fn bind_project(&self, user_id: &str, conversation_id: &str) -> Result<(), String>;

    /// Path of the project's `role=workspace` folder.
    async fn project_workspace(&self, user_id: &str, project_id: &str) -> Result<Option<String>, String>;

    /// Display name of an enabled assistant of the user; `None` = unknown.
    async fn assistant_name(&self, user_id: &str, assistant_id: &str) -> Result<Option<String>, String>;

    /// Names of the skills the user has installed.
    async fn user_skill_names(&self, user_id: &str) -> Result<Vec<String>, String>;

    async fn create_assignment_conversation(
        &self,
        user_id: &str,
        spec: &AssignmentConversation,
    ) -> Result<String, HostError>;

    async fn delete_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), String>;

    /// Send `content` as a user message of the conversation (starts its
    /// turn); returns the message id. The board calls it only when the
    /// conversation has no user message yet (insert-or-return-existing).
    async fn send_message(&self, user_id: &str, conversation_id: &str, content: &str) -> Result<String, String>;

    async fn team(&self, user_id: &str, team_id: &str) -> Result<Option<HostTeam>, String>;

    /// Deliver `content` to the team's lead as a user message (the path
    /// behind `POST /api/teams/{id}/messages`).
    async fn send_team_message(&self, user_id: &str, team_id: &str, content: &str) -> Result<(), String>;

    /// The resolved skill set of an assignee (`assistant` or `team`).
    async fn assignee_skills(&self, user_id: &str, kind: &str, assignee_ref: &str) -> Result<Vec<String>, String>;

    /// The user's "Board Keeper" assistant (Claude backend), created on first
    /// use. Returns its assistant id.
    async fn ensure_keeper_assistant(&self, user_id: &str) -> Result<String, String>;

    /// Create the pinned keeper conversation; returns its id.
    async fn create_keeper_conversation(&self, user_id: &str, spec: &KeeperConversation) -> Result<String, String>;
}
