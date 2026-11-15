//! Voice tools (§3.2), executed only by the backend under the user's
//! identity. Read tools answer from conversations, messages and code (results
//! wrapped as untrusted content); draft tools hand the turn to the draft
//! engine; act tools never act on their own — a send needs a confirmed draft.

pub mod files;
pub mod policy;

use std::sync::Arc;

use serde_json::{Value, json};

use crate::conversation_port::{ConversationInfo, ConversationPort, ProjectInfo};
use crate::resolver::{Candidate, Resolution, resolve};
use policy::wrap_untrusted;

/// Tool tiers (§3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Read,
    Draft,
    Act,
}

pub fn tier_of(name: &str) -> Option<Tier> {
    match name {
        "list_projects"
        | "list_conversations"
        | "conversation_status"
        | "last_reply_summary"
        | "search_code"
        | "read_file"
        | "search_messages"
        | "pending_confirmations" => Some(Tier::Read),
        "refine_request" | "edit_draft" => Some(Tier::Draft),
        "send_draft" | "create_conversation" => Some(Tier::Act),
        _ => None,
    }
}

fn function(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": {"type": "object", "properties": properties, "required": required},
    })
}

/// Function tools declared in the session config.
pub fn definitions() -> Vec<Value> {
    let conversation =
        json!({"conversation": {"type": "string", "description": "Conversation name as the user said it"}});
    vec![
        function("list_projects", "List the user's projects.", json!({}), &[]),
        function(
            "list_conversations",
            "List the user's recent conversations.",
            json!({}),
            &[],
        ),
        function(
            "conversation_status",
            "Is a conversation working, idle or waiting for an approval?",
            conversation.clone(),
            &["conversation"],
        ),
        function(
            "last_reply_summary",
            "The latest reply of a conversation (quoted).",
            conversation.clone(),
            &["conversation"],
        ),
        function(
            "pending_confirmations",
            "How many approvals a conversation is waiting for.",
            conversation,
            &["conversation"],
        ),
        function(
            "search_code",
            "Search a project's code for a literal text (max 50 matches).",
            json!({"project": {"type": "string"}, "query": {"type": "string"}}),
            &["project", "query"],
        ),
        function(
            "read_file",
            "Read up to 200 lines of a project file.",
            json!({"project": {"type": "string"}, "path": {"type": "string"}, "start_line": {"type": "integer"}}),
            &["project", "path"],
        ),
        function(
            "search_messages",
            "Search conversation messages.",
            json!({"query": {"type": "string"}}),
            &["query"],
        ),
        function(
            "refine_request",
            "The user asked to send something to a conversation (or to start a new one): build a draft from \
             what they said. The backend reads it back; never claim it was sent.",
            json!({"kind": {"type": "string", "enum": ["message", "new_conversation"]}}),
            &[],
        ),
        function(
            "edit_draft",
            "The user wants to change the pending draft.",
            json!({"change": {"type": "string"}}),
            &["change"],
        ),
        function(
            "send_draft",
            "Only the user's confirmation sends a draft; calling this never sends.",
            json!({"draft_id": {"type": "string"}}),
            &[],
        ),
        function(
            "create_conversation",
            "Only the user's confirmation creates it; use refine_request.",
            json!({}),
            &[],
        ),
    ]
}

/// What a tool call asks the draft engine to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEffect {
    Refine {
        new_conversation: bool,
    },
    Edit {
        change: String,
    },
    /// A model-issued act: rejected unless the user confirmed (audited).
    ActAttempt {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub output: Value,
    pub effect: Option<ToolEffect>,
    /// Identifiers the call resolved (allowed in a draft's References).
    pub resolved: Vec<String>,
    /// Raw text the model saw (for the refiner's injection check).
    pub snippet: Option<String>,
}

impl ToolOutcome {
    fn data(output: Value) -> Self {
        Self {
            output,
            effect: None,
            resolved: Vec::new(),
            snippet: None,
        }
    }

    fn error(message: &str) -> Self {
        Self::data(json!({"error": message}))
    }
}

/// Executes read tools; draft and act tools become [`ToolEffect`]s.
pub struct ToolRunner {
    pub conversations: Arc<dyn ConversationPort>,
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

impl ToolRunner {
    pub async fn execute(&self, user_id: &str, name: &str, args: &Value) -> ToolOutcome {
        match name {
            "list_projects" => match self.conversations.list_projects(user_id).await {
                Ok(projects) => {
                    let names: Vec<_> = projects.iter().map(|p| p.name.clone()).collect();
                    ToolOutcome {
                        resolved: names.clone(),
                        ..ToolOutcome::data(json!({"projects": names}))
                    }
                }
                Err(_) => ToolOutcome::error("projects unavailable"),
            },
            "list_conversations" => match self.conversations.list_conversations(user_id).await {
                Ok(list) => {
                    let names: Vec<_> = list.iter().take(30).map(|c| c.name.clone()).collect();
                    ToolOutcome {
                        resolved: names.clone(),
                        ..ToolOutcome::data(json!({"conversations": names}))
                    }
                }
                Err(_) => ToolOutcome::error("conversations unavailable"),
            },
            "conversation_status" | "pending_confirmations" | "last_reply_summary" => {
                let Some(spoken) = arg(args, "conversation") else {
                    return ToolOutcome::error("which conversation?");
                };
                let conversation = match self.find_conversation(user_id, spoken).await {
                    Ok(c) => c,
                    Err(outcome) => return outcome,
                };
                self.conversation_read(user_id, name, &conversation).await
            }
            "search_messages" => {
                let Some(query) = arg(args, "query") else {
                    return ToolOutcome::error("query required");
                };
                match self.conversations.search_messages(user_id, query, 10).await {
                    Ok(hits) => {
                        let text = hits
                            .iter()
                            .map(|h| format!("{}: {}", h.conversation_name, h.snippet))
                            .collect::<Vec<_>>()
                            .join("\n");
                        ToolOutcome {
                            output: wrap_untrusted(name, &text),
                            resolved: hits.iter().map(|h| h.conversation_name.clone()).collect(),
                            snippet: Some(text),
                            effect: None,
                        }
                    }
                    Err(_) => ToolOutcome::error("search unavailable"),
                }
            }
            "search_code" | "read_file" => self.code_read(user_id, name, args).await,
            "refine_request" => ToolOutcome {
                effect: Some(ToolEffect::Refine {
                    new_conversation: arg(args, "kind") == Some("new_conversation"),
                }),
                ..ToolOutcome::data(
                    json!({"status": "drafting", "note": "Jarvis reads the draft back; wait for the user."}),
                )
            },
            "edit_draft" => ToolOutcome {
                effect: Some(ToolEffect::Edit {
                    change: arg(args, "change").unwrap_or_default().to_owned(),
                }),
                ..ToolOutcome::data(json!({"status": "editing"}))
            },
            "send_draft" | "create_conversation" => ToolOutcome {
                effect: Some(ToolEffect::ActAttempt { name: name.to_owned() }),
                ..ToolOutcome::data(json!({
                    "status": "rejected",
                    "reason": "Only the user's spoken or tapped confirmation sends a draft.",
                }))
            },
            _ => ToolOutcome::error("unknown tool"),
        }
    }

    /// Read tools may name a conversation loosely; a unique match is required.
    async fn find_conversation(&self, user_id: &str, spoken: &str) -> Result<ConversationInfo, ToolOutcome> {
        let list = self
            .conversations
            .list_conversations(user_id)
            .await
            .map_err(|_| ToolOutcome::error("conversations unavailable"))?;
        let candidates: Vec<Candidate> = list
            .iter()
            .map(|c| Candidate {
                id: c.id.clone(),
                name: c.name.clone(),
                project_name: None,
            })
            .collect();
        match resolve(spoken, &candidates, None) {
            Resolution::Unique { id, .. } => Ok(list.into_iter().find(|c| c.id == id).expect("resolved from list")),
            Resolution::Ambiguous { options } => Err(ToolOutcome::data(json!({
                "ambiguous": options.into_iter().map(|(_, name)| name).collect::<Vec<_>>(),
            }))),
            _ => Err(ToolOutcome::error("no conversation with that name")),
        }
    }

    async fn conversation_read(&self, user_id: &str, name: &str, conversation: &ConversationInfo) -> ToolOutcome {
        let resolved = vec![conversation.name.clone()];
        match name {
            "last_reply_summary" => match self.conversations.last_reply(user_id, &conversation.id).await {
                Ok(Some(text)) => ToolOutcome {
                    output: wrap_untrusted(name, &text),
                    snippet: Some(text),
                    resolved,
                    effect: None,
                },
                Ok(None) => ToolOutcome::data(json!({"conversation": conversation.name, "reply": null})),
                Err(_) => ToolOutcome::error("reply unavailable"),
            },
            _ => match self.conversations.status(user_id, &conversation.id).await {
                Ok(Some(status)) => {
                    let state = if status.pending_confirmations > 0 {
                        "waiting for an approval"
                    } else if status.is_processing {
                        "working"
                    } else {
                        "idle"
                    };
                    ToolOutcome {
                        resolved,
                        ..ToolOutcome::data(json!({
                            "conversation": conversation.name,
                            "state": state,
                            "pending_confirmations": status.pending_confirmations,
                        }))
                    }
                }
                _ => ToolOutcome::error("status unavailable"),
            },
        }
    }

    async fn find_project(&self, user_id: &str, spoken: &str) -> Result<ProjectInfo, ToolOutcome> {
        let projects = self
            .conversations
            .list_projects(user_id)
            .await
            .map_err(|_| ToolOutcome::error("projects unavailable"))?;
        let candidates: Vec<Candidate> = projects
            .iter()
            .map(|p| Candidate {
                id: p.id.clone(),
                name: p.name.clone(),
                project_name: None,
            })
            .collect();
        match resolve(spoken, &candidates, None) {
            Resolution::Unique { id, .. } => Ok(projects.into_iter().find(|p| p.id == id).expect("resolved")),
            Resolution::Ambiguous { options } => Err(ToolOutcome::data(json!({
                "ambiguous": options.into_iter().map(|(_, name)| name).collect::<Vec<_>>(),
            }))),
            _ => Err(ToolOutcome::error("no project with that name")),
        }
    }

    async fn code_read(&self, user_id: &str, name: &str, args: &Value) -> ToolOutcome {
        let Some(project_name) = arg(args, "project") else {
            return ToolOutcome::error("which project?");
        };
        let project = match self.find_project(user_id, project_name).await {
            Ok(p) => p,
            Err(outcome) => return outcome,
        };
        let Some(root) = project.root.as_deref() else {
            return ToolOutcome::error("the project has no workspace folder");
        };
        let root = match files::project_root(root) {
            Ok(root) => root,
            Err(err) => return ToolOutcome::error(&err.to_string()),
        };
        let label = name.to_owned();
        let is_read = name == "read_file";
        let args = args.clone();
        let result = tokio::task::spawn_blocking(move || {
            if is_read {
                let path = arg(&args, "path").unwrap_or_default().to_owned();
                let start = args.get("start_line").and_then(Value::as_u64).unwrap_or(1) as usize;
                files::read_lines(&root, &path, start)
                    .map(|text| (text, vec![path]))
                    .map_err(|e| e.to_string())
            } else {
                let query = arg(&args, "query").unwrap_or_default().to_owned();
                let hits = files::search(&root, &query);
                let paths: Vec<String> = hits.iter().map(|h| h.path.clone()).collect();
                let text = hits
                    .iter()
                    .map(|h| format!("{}:{}: {}", h.path, h.line, h.text))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok((text, paths))
            }
        })
        .await;
        match result {
            Ok(Ok((text, mut resolved))) => {
                resolved.push(project.name);
                resolved.sort();
                resolved.dedup();
                ToolOutcome {
                    output: wrap_untrusted(&label, &text),
                    snippet: Some(text),
                    resolved,
                    effect: None,
                }
            }
            Ok(Err(message)) => ToolOutcome::error(&message),
            Err(_) => ToolOutcome::error("search failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_definition_has_a_tier() {
        for def in definitions() {
            let name = def["name"].as_str().unwrap();
            assert!(tier_of(name).is_some(), "{name}");
        }
        assert_eq!(tier_of("send_draft"), Some(Tier::Act));
        assert_eq!(tier_of("delete_everything"), None);
    }
}
