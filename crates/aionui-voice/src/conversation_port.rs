//! Voice-owned port onto conversations and projects (§3 architecture:
//! ConversationPort, MessageSearchPort, ProjectFilesPort roots). `aionui-app`
//! implements it over the conversation service; voice never touches that
//! domain directly. Every method is scoped to the calling user.

/// A conversation the user can address by voice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationInfo {
    pub id: String,
    pub name: String,
    pub project_id: Option<String>,
    pub updated_at: i64,
}

/// A project (for grounding and code search).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    /// Workspace folder URI (`file:` canonical) or absolute path; `None` when
    /// the project has no workspace folder.
    pub root: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationStatus {
    pub is_processing: bool,
    pub pending_confirmations: usize,
    pub active_turn_id: Option<String>,
    /// A message sent now joins the running turn (mid-turn delivery).
    pub can_steer: bool,
}

/// Result of a keyed send (`ConversationService::send_message_idempotent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Delivered {
        msg_id: String,
        turn_id: String,
        /// Steered into the running turn (B5 mid-turn delivery).
        midturn: bool,
    },
    /// 409: the conversation is running a turn it cannot be steered into.
    Busy,
    NotFound,
    Rejected(String),
}

/// What happened to a delivered message since it was sent (§3.3.6 outcomes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendProbe {
    /// A turn is running in the conversation now.
    pub turn_active: bool,
    /// An assistant reply newer than the message exists (failure tips excluded).
    pub replied: bool,
    /// A failure tip newer than the message exists.
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageHit {
    pub conversation_id: String,
    pub conversation_name: String,
    pub snippet: String,
}

#[async_trait::async_trait]
pub trait ConversationPort: Send + Sync {
    async fn list_conversations(&self, user_id: &str) -> Result<Vec<ConversationInfo>, String>;

    async fn list_projects(&self, user_id: &str) -> Result<Vec<ProjectInfo>, String>;

    /// `None` when the conversation does not exist for this user.
    async fn status(&self, user_id: &str, conversation_id: &str) -> Result<Option<ConversationStatus>, String>;

    /// Text of the latest assistant reply, if any.
    async fn last_reply(&self, user_id: &str, conversation_id: &str) -> Result<Option<String>, String>;

    async fn search_messages(&self, user_id: &str, keyword: &str, limit: usize) -> Result<Vec<MessageHit>, String>;

    /// Send `text` with an idempotency key (`draft_id:version`).
    async fn send_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        text: &str,
        idempotency_key: &str,
    ) -> SendOutcome;

    async fn probe_send(&self, user_id: &str, conversation_id: &str, msg_id: &str) -> Result<SendProbe, String>;

    /// Create a conversation in `project_id`, idempotently: a conversation
    /// whose `extra.voice_draft_key` equals `draft_key` is returned instead
    /// of creating a second one. Returns the conversation id.
    async fn create_conversation(
        &self,
        user_id: &str,
        project_id: Option<&str>,
        title: &str,
        draft_key: &str,
    ) -> Result<String, String>;
}
