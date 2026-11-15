//! Agent caller resolution (R7) and the agent-router reads.

use aionui_api_types::{BoardStatusResponse, BoardTemplate};
use aionui_db::{BoardCallerRow, BoardCardRow};
use serde_json::Value;
use tracing::{debug, warn};

use crate::actor::{AgentCaller, BoardActor, agent_actor_ref};
use crate::error::{BoardError, BoardErrorCode, BoardResult};
use crate::service::{BoardService, disabled};

fn no_project() -> BoardError {
    BoardError::new(
        BoardErrorCode::NoProject,
        "this conversation does not belong to a project with a board",
    )
}

/// The team id marker of a conversation's `extra` (canonical `team_id`
/// first, legacy `teamId`).
fn team_id(extra: &Value) -> Option<&str> {
    ["team_id", "teamId"]
        .into_iter()
        .filter_map(|key| extra.get(key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
}

impl BoardService {
    /// Resolve a validated runtime caller to its project (R7):
    /// `conversations.project_id`; when NULL the conversation service's lazy
    /// bind runs and the row is re-read; then the team's `project_id`; else
    /// `409 board_no_project`. Temp projects have no board.
    pub async fn resolve_agent(&self, user_id: &str, conversation_id: &str) -> BoardResult<AgentCaller> {
        let store = self.store();
        let mut row: BoardCallerRow = store.caller(user_id, conversation_id).await?.ok_or_else(no_project)?;
        if row.project_id.is_none()
            && let Some(port) = self.inner.host.get()
        {
            if let Err(error) = port.bind_project(user_id, conversation_id).await {
                warn!(conversation_id = %conversation_id, error = %error, "board: lazy project bind failed");
            }
            row = store.caller(user_id, conversation_id).await?.ok_or_else(no_project)?;
        }
        let extra: Value = serde_json::from_str(&row.extra).unwrap_or(Value::Null);
        let project_id = match row.project_id.clone() {
            Some(project_id) => project_id,
            None => match team_id(&extra) {
                Some(team) => store.team_project_id(user_id, team).await?.ok_or_else(no_project)?,
                None => return Err(no_project()),
            },
        };
        match store.project(user_id, &project_id).await? {
            Some(project) if project.kind == "standard" => {}
            _ => return Err(no_project()),
        }
        let keeper = store
            .settings(user_id, &project_id)
            .await?
            .and_then(|settings| settings.keeper_conversation_id)
            .is_some_and(|keeper| keeper == conversation_id);
        debug!(conversation_id = %conversation_id, project_id = %project_id, keeper, "board: agent caller resolved");
        Ok(AgentCaller {
            user_id: user_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            project_id,
            actor_ref: agent_actor_ref(row.assistant_id.as_deref(), &extra, &row.conversation_type),
            assistant_id: row.assistant_id.clone(),
            keeper,
        })
    }

    /// `status`: always answers for an authenticated caller — no project or
    /// no board is `{enabled: false, project_id: null|…, template: null}`.
    pub async fn agent_status(&self, user_id: &str, conversation_id: &str) -> BoardResult<BoardStatusResponse> {
        let caller = match self.resolve_agent(user_id, conversation_id).await {
            Ok(caller) => caller,
            Err(error) if error.code() == Some(BoardErrorCode::NoProject) => {
                return Ok(BoardStatusResponse {
                    enabled: false,
                    project_id: None,
                    template: None,
                    keeper: false,
                });
            }
            Err(error) => return Err(error),
        };
        let settings = self.store().settings(user_id, &caller.project_id).await?;
        Ok(BoardStatusResponse {
            enabled: settings.as_ref().is_some_and(|s| s.enabled && s.archived_at.is_none()),
            template: settings.as_ref().and_then(|s| BoardTemplate::parse(&s.template)),
            project_id: Some(caller.project_id),
            keeper: caller.keeper,
        })
    }

    /// R1 for agent reads and writes: the board must exist and be enabled.
    /// Reads nothing but settings and writes nothing.
    pub async fn require_enabled(&self, actor: &BoardActor) -> BoardResult<()> {
        let Some(agent) = actor.as_agent() else {
            return Ok(());
        };
        match self.store().settings(&agent.user_id, &agent.project_id).await? {
            Some(settings) if settings.enabled => Ok(()),
            _ => Err(disabled()),
        }
    }

    /// The caller-project card with this number (agents address cards by
    /// number within their project).
    pub async fn agent_card(&self, caller: &AgentCaller, number: i64) -> BoardResult<BoardCardRow> {
        self.store()
            .card_by_number(&caller.user_id, &caller.project_id, number)
            .await?
            .ok_or_else(|| BoardError::new(BoardErrorCode::NotFound, format!("TASK-{number} not found")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn team_marker_prefers_the_canonical_key() {
        assert_eq!(team_id(&json!({ "team_id": "t1", "teamId": "t2" })), Some("t1"));
        assert_eq!(team_id(&json!({ "teamId": "t2" })), Some("t2"));
        assert_eq!(team_id(&json!({ "team_id": " " })), None);
    }
}
