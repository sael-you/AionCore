//! Shared fixture for the board service tests: in-memory DB, standard
//! projects, conversations, a subscribed event bus and a fake host port.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use aionui_api_types::{
    BoardColumnInput, BoardSettingsDto, BoardTemplate, ColumnCategory, ColumnKind, ColumnResolution,
    CreateBoardCardRequest, UpdateBoardColumnsRequest, UpdateBoardSettingsRequest, WebSocketMessage,
};
use aionui_board::{
    AssignmentConversation, BoardActor, BoardError, BoardHostPort, BoardService, HostError, HostTeam,
    KeeperConversation, MoveCard,
};
use aionui_db::{Database, SqliteBoardStore, SqlitePool, init_database_memory};
use aionui_realtime::BroadcastEventBus;
use tokio::sync::broadcast::Receiver;

pub const USER: &str = "user-1";
pub const OTHER: &str = "user-2";
pub const PROJECT: &str = "proj-1";
pub const PROJECT_B: &str = "proj-2";
pub const CONV: &str = "conv-1";
pub const ASSISTANT: &str = "asst-1";
pub const KEEPER_ASSISTANT: &str = "keeper-asst";

pub struct Env {
    pub db: Database,
    pub service: BoardService,
    pub rx: Receiver<WebSocketMessage<serde_json::Value>>,
    pub bus: Arc<BroadcastEventBus>,
    pub host: Arc<FakeHost>,
}

impl Env {
    pub fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    /// Broadcasts received since the last drain.
    pub fn drain(&mut self) -> Vec<WebSocketMessage<serde_json::Value>> {
        let mut out = Vec::new();
        while let Ok(message) = self.rx.try_recv() {
            out.push(message);
        }
        out
    }

    /// Event kinds broadcast since the last drain.
    pub fn kinds(&mut self) -> Vec<String> {
        self.drain()
            .iter()
            .map(|m| m.data["event"]["kind"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    pub async fn event_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM board_events")
            .fetch_one(self.pool())
            .await
            .unwrap()
    }
}

pub async fn exec(pool: &SqlitePool, sql: &str) {
    sqlx::query(sql).execute(pool).await.unwrap();
}

/// A fixture without a host port (keeper/assign unavailable).
pub async fn env() -> Env {
    env_with_host(false).await
}

pub async fn env_with_host(wired: bool) -> Env {
    let db = init_database_memory().await.unwrap();
    for user in [USER, OTHER] {
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
            .bind(user)
            .bind(user)
            .execute(db.pool())
            .await
            .unwrap();
    }
    project(db.pool(), USER, PROJECT, "standard").await;
    project(db.pool(), USER, PROJECT_B, "standard").await;
    conversation(db.pool(), USER, CONV, Some(PROJECT), r#"{"backend":"claude"}"#).await;
    let bus = Arc::new(BroadcastEventBus::new(512));
    let rx = bus.subscribe();
    let service = BoardService::new(Arc::new(SqliteBoardStore::new(db.pool().clone())), bus.clone());
    let host = Arc::new(FakeHost::new(db.pool().clone()));
    if wired {
        service.set_host_port(host.clone());
    }
    Env {
        db,
        service,
        rx,
        bus,
        host,
    }
}

pub async fn project(pool: &SqlitePool, user: &str, id: &str, kind: &str) {
    sqlx::query(
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, ?, 0, 0)",
    )
    .bind(id)
    .bind(user)
    .bind(format!("Project {id}"))
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
}

pub async fn conversation(pool: &SqlitePool, user: &str, id: &str, project_id: Option<&str>, extra: &str) {
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, project_id, created_at, updated_at) \
         VALUES (?, ?, ?, 'acp', ?, 'finished', ?, 1, 1)",
    )
    .bind(id)
    .bind(user)
    .bind(id)
    .bind(extra)
    .bind(project_id)
    .execute(pool)
    .await
    .unwrap();
}

pub fn owner() -> BoardActor {
    BoardActor::user(USER)
}

pub fn code(error: BoardError) -> &'static str {
    error.code().map(|code| code.as_str()).unwrap_or("internal")
}

pub async fn enable(service: &BoardService, project_id: &str) -> BoardSettingsDto {
    service
        .update_settings(
            &owner(),
            project_id,
            UpdateBoardSettingsRequest {
                version: 0,
                enabled: Some(true),
                template: Some(BoardTemplate::Studio),
                settings: None,
            },
        )
        .await
        .unwrap()
}

pub fn create(title: &str) -> CreateBoardCardRequest {
    CreateBoardCardRequest {
        title: title.into(),
        ..Default::default()
    }
}

pub fn create_in(title: &str, column: &str) -> CreateBoardCardRequest {
    CreateBoardCardRequest {
        title: title.into(),
        column: Some(column.into()),
        ..Default::default()
    }
}

pub fn mv(to: &str, expected: &str) -> MoveCard {
    MoveCard {
        to_column: to.into(),
        expected_column: Some(expected.into()),
        ..Default::default()
    }
}

pub fn forced(to: &str, expected: &str, reason: Option<&str>) -> MoveCard {
    MoveCard {
        force: true,
        reason: reason.map(str::to_owned),
        ..mv(to, expected)
    }
}

pub fn column(key: &str, category: ColumnCategory) -> BoardColumnInput {
    BoardColumnInput {
        key: key.into(),
        name: None,
        category,
        kind: None,
        resolution: None,
        agent_move: None,
        wip_limit: None,
        exit_rule: None,
        gate: None,
    }
}

pub fn open_column(key: &str, category: ColumnCategory) -> BoardColumnInput {
    BoardColumnInput {
        agent_move: Some(aionui_api_types::AgentMove::Allow),
        ..column(key, category)
    }
}

pub fn terminal(key: &str, resolution: ColumnResolution) -> BoardColumnInput {
    BoardColumnInput {
        kind: Some(ColumnKind::Terminal),
        resolution: Some(resolution),
        ..column(key, ColumnCategory::Done)
    }
}

/// Replace the columns of an enabled board.
pub async fn set_columns(service: &BoardService, project_id: &str, columns: Vec<BoardColumnInput>) {
    let version = service
        .board_view(&owner(), project_id, false)
        .await
        .unwrap()
        .settings
        .version;
    service
        .update_columns(&owner(), project_id, UpdateBoardColumnsRequest { version, columns })
        .await
        .unwrap();
}

/// A host that writes real conversation/message rows so the board's own
/// reads (assignment lookup, first user message, link scope) see them.
pub struct FakeHost {
    pool: SqlitePool,
    pub sent: Mutex<Vec<(String, String)>>,
    pub team_messages: Mutex<Vec<(String, String)>>,
    pub fail_send: Mutex<bool>,
    pub deleted: Mutex<Vec<String>>,
    pub teams: Mutex<Vec<HostTeam>>,
    counter: Mutex<u32>,
}

impl FakeHost {
    fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            sent: Mutex::new(Vec::new()),
            team_messages: Mutex::new(Vec::new()),
            fail_send: Mutex::new(false),
            deleted: Mutex::new(Vec::new()),
            teams: Mutex::new(Vec::new()),
            counter: Mutex::new(0),
        }
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;
        format!("{prefix}-{counter}")
    }

    async fn project_of_workspace(&self, user_id: &str) -> Option<String> {
        // Every fake workspace belongs to PROJECT.
        let _ = user_id;
        Some(PROJECT.to_owned())
    }
}

#[async_trait::async_trait]
impl BoardHostPort for FakeHost {
    async fn bind_project(&self, _user_id: &str, _conversation_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn project_workspace(&self, _user_id: &str, _project_id: &str) -> Result<Option<String>, String> {
        Ok(Some("/repo".into()))
    }

    async fn assistant_name(&self, _user_id: &str, assistant_id: &str) -> Result<Option<String>, String> {
        Ok(matches!(assistant_id, ASSISTANT | KEEPER_ASSISTANT).then(|| format!("name of {assistant_id}")))
    }

    async fn user_skill_names(&self, _user_id: &str) -> Result<Vec<String>, String> {
        Ok(vec!["mermaid".into()])
    }

    async fn create_assignment_conversation(
        &self,
        user_id: &str,
        spec: &AssignmentConversation,
    ) -> Result<String, HostError> {
        let id = self.next_id("assigned");
        let extra = serde_json::json!({
            "workspace": spec.workspace,
            "board_assignment_id": spec.assignment_id,
            "preset_enabled_skills": spec.skills,
            "report_to_parent": false,
        });
        let project = self.project_of_workspace(user_id).await;
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, extra, status, project_id, parent_conversation_id, \
             created_at, updated_at) VALUES (?, ?, ?, 'acp', ?, 'pending', ?, ?, 1, 1)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(&spec.name)
        .bind(extra.to_string())
        .bind(project)
        .bind(&spec.parent_conversation_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if e.to_string().to_lowercase().contains("unique") {
                HostError::Duplicate
            } else {
                HostError::Failed(e.to_string())
            }
        })?;
        Ok(id)
    }

    async fn delete_conversation(&self, _user_id: &str, conversation_id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(conversation_id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        self.deleted.lock().unwrap().push(conversation_id.to_owned());
        Ok(())
    }

    async fn send_message(&self, _user_id: &str, conversation_id: &str, content: &str) -> Result<String, String> {
        if *self.fail_send.lock().unwrap() {
            return Err("agent unavailable".into());
        }
        let id = self.next_id("msg");
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, msg_id, type, content, position, status, created_at) \
             VALUES (?, ?, ?, 'text', '{}', 'right', 'finish', 1)",
        )
        .bind(&id)
        .bind(conversation_id)
        .bind(&id)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        self.sent
            .lock()
            .unwrap()
            .push((conversation_id.to_owned(), content.to_owned()));
        Ok(id)
    }

    async fn team(&self, _user_id: &str, team_id: &str) -> Result<Option<HostTeam>, String> {
        Ok(self
            .teams
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.team_id == team_id)
            .cloned())
    }

    async fn send_team_message(&self, _user_id: &str, team_id: &str, content: &str) -> Result<(), String> {
        self.team_messages
            .lock()
            .unwrap()
            .push((team_id.to_owned(), content.to_owned()));
        Ok(())
    }

    async fn assignee_skills(&self, _user_id: &str, kind: &str, assignee: &str) -> Result<Vec<String>, String> {
        Ok(vec![format!("{kind}:{assignee}"), "mermaid".into()])
    }

    async fn ensure_keeper_assistant(&self, _user_id: &str) -> Result<String, String> {
        Ok(KEEPER_ASSISTANT.into())
    }

    async fn create_keeper_conversation(&self, user_id: &str, spec: &KeeperConversation) -> Result<String, String> {
        let id = self.next_id("keeper");
        let extra = serde_json::json!({ "workspace": spec.workspace, "pinned": true, "board_keeper": true });
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, extra, status, project_id, created_at, updated_at) \
             VALUES (?, ?, ?, 'acp', ?, 'pending', ?, 1, 1)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(&spec.name)
        .bind(extra.to_string())
        .bind(PROJECT)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(id)
    }
}
