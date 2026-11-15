//! Market assignments: instance -> global | project (whole project or one
//! assistant) -> CLIs. Rules R2, R3, R7 at write time; assistant rows update
//! the assistant's fixed MCP list in-process (R8, R1 #4).

use aionui_api_types::{CreateMcpMarketAssignmentRequest, McpMarketAssignmentResponse};
use aionui_db::CreateAssignmentParams;
use aionui_db::models::McpAssignmentRow;
use tracing::{info, warn};

use super::error::{MarketError, invalid};
use super::rules::{CLIS, GLOBAL_IDS};
use super::service::McpMarketService;

impl McpMarketService {
    pub async fn list_assignments(
        &self,
        user_id: &str,
        project_id: Option<&str>,
        instance_id: Option<&str>,
    ) -> Result<Vec<McpMarketAssignmentResponse>, MarketError> {
        let names = self.instance_names(user_id).await?;
        Ok(self
            .repo
            .list_assignments(user_id, instance_id)
            .await?
            .into_iter()
            .filter(|a| project_id.is_none_or(|p| a.project_id.as_deref() == Some(p)))
            .map(|a| to_response(a, &names))
            .collect())
    }

    async fn instance_names(&self, user_id: &str) -> Result<std::collections::HashMap<String, String>, MarketError> {
        Ok(self
            .repo
            .list_instances(user_id)
            .await?
            .into_iter()
            .map(|r| (r.id, r.name))
            .collect())
    }

    /// Create one assignment per requested CLI (all or nothing for validation;
    /// rows are inserted in order and a duplicate stops with 409).
    pub async fn create_assignments(
        &self,
        user_id: &str,
        req: CreateMcpMarketAssignmentRequest,
    ) -> Result<Vec<McpMarketAssignmentResponse>, MarketError> {
        let inst = self.load(user_id, &req.instance_id).await?;
        let project = req.project_id.as_deref().map(str::trim).filter(|p| !p.is_empty());
        let assistant = req.assistant_id.as_deref().map(str::trim).filter(|a| !a.is_empty());
        if req.clis.is_empty() {
            return Err(invalid("clis must not be empty"));
        }
        let name = &inst.row.name;
        let mut problems = Vec::new();
        match project {
            None => {
                if assistant.is_some() {
                    problems.push("an assistant assignment needs a project".to_owned());
                }
                if !GLOBAL_IDS.contains(&name.as_str()) {
                    problems.push(format!("only {} may be global (R2)", GLOBAL_IDS.join(", ")));
                }
                if inst.tier == "write" {
                    problems.push(format!("write instance \"{name}\" is never global (R3)"));
                }
            }
            Some(p) => {
                if !self.repo.project_exists(p).await? {
                    return Err(MarketError::NotFound(format!("project {p}")));
                }
            }
        }
        for cli in &req.clis {
            if !CLIS.contains(&cli.as_str()) {
                problems.push(format!("cli \"{cli}\" is not one of {}", CLIS.join(",")));
            } else if !inst.clis.contains(cli) {
                problems.push(format!("instance \"{name}\" is not enabled for {cli}"));
            }
            if inst.tier == "write" && cli != "claude" {
                problems.push(format!("write instance \"{name}\" is Claude-only (R3)"));
            }
        }
        // R7: one delivery channel per (instance, project, cli).
        if let Some(p) = project {
            let existing = self.repo.list_assignments(user_id, Some(&inst.row.id)).await?;
            for cli in &req.clis {
                let clash = existing.iter().any(|a| {
                    a.project_id.as_deref() == Some(p)
                        && &a.cli == cli
                        && a.assistant_id.is_some() != assistant.is_some()
                });
                if clash {
                    problems.push(format!(
                        "\"{name}\" already reaches {cli} in project {p} through the other channel (R7: native file or assistant binding, never both)"
                    ));
                }
            }
        }
        super::service::reject(problems)?;

        if let (Some(assistant_id), Some(binder)) = (assistant, &self.binder) {
            binder
                .bind(user_id, assistant_id, &inst.row.id)
                .await
                .map_err(|e| invalid(format!("assistant binding failed: {e}")))?;
        }
        let mut created = Vec::new();
        for cli in &req.clis {
            let row = self
                .repo
                .create_assignment(&CreateAssignmentParams {
                    user_id,
                    instance_id: &inst.row.id,
                    project_id: project,
                    assistant_id: assistant,
                    cli,
                })
                .await?;
            created.push(row);
        }
        info!(instance = %name, project = project.unwrap_or("global"), assistant = assistant.unwrap_or("-"), count = created.len(), "mcp market: assignments created");
        let names = self.instance_names(user_id).await?;
        Ok(created.into_iter().map(|a| to_response(a, &names)).collect())
    }

    pub async fn delete_assignment(&self, user_id: &str, id: &str) -> Result<(), MarketError> {
        let row = self
            .repo
            .find_assignment(user_id, id)
            .await?
            .ok_or_else(|| MarketError::NotFound(format!("assignment {id}")))?;
        if let Some(assistant) = row.assistant_id.as_deref() {
            self.unbind_if_last(user_id, &row.mcp_server_id, assistant, Some(&row.id))
                .await?;
        }
        self.repo.delete_assignment(user_id, id).await?;
        info!(assignment = %id, "mcp market: assignment deleted");
        Ok(())
    }

    /// Remove the instance from the assistant's fixed list unless another
    /// assignment (other than `except`) still binds it to that assistant.
    pub(crate) async fn unbind_if_last(
        &self,
        user_id: &str,
        instance_id: &str,
        assistant_id: &str,
        except: Option<&str>,
    ) -> Result<(), MarketError> {
        let Some(binder) = &self.binder else {
            return Ok(());
        };
        let still = self
            .repo
            .list_assignments(user_id, Some(instance_id))
            .await?
            .into_iter()
            .any(|a| a.assistant_id.as_deref() == Some(assistant_id) && Some(a.id.as_str()) != except);
        if !still && let Err(e) = binder.unbind(user_id, assistant_id, instance_id).await {
            // The gate drops unassigned market rows anyway; keep going.
            warn!(assistant = %assistant_id, error = %e, "mcp market: assistant unbind failed");
        }
        Ok(())
    }
}

fn to_response(a: McpAssignmentRow, names: &std::collections::HashMap<String, String>) -> McpMarketAssignmentResponse {
    McpMarketAssignmentResponse {
        instance_name: names.get(&a.mcp_server_id).cloned().unwrap_or_default(),
        id: a.id,
        instance_id: a.mcp_server_id,
        project_id: a.project_id,
        assistant_id: a.assistant_id,
        cli: a.cli,
        created_at: a.created_at,
    }
}
