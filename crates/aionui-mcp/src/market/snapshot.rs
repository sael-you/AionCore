//! Market status and the kit-lock snapshot `studio sync --snapshot -` reads
//! (shape: `clauterm-studio/tools/studio/lib/schema.js` `validate()`; refs
//! only, never a value).

use std::collections::BTreeMap;

use aionui_api_types::{McpMarketEnvKind, McpMarketInstanceStatus, McpMarketStatusResponse};
use serde_json::{Value, json};

use super::error::{MarketError, invalid};
use super::rules::{GLOBAL_IDS, instance_problems};
use super::service::McpMarketService;
use super::params::{substitute, substitute_command};
use super::stored::StoredInstance;
use crate::secret_mask::is_secret_key;

impl McpMarketService {
    pub async fn status(&self, user_id: &str) -> Result<McpMarketStatusResponse, MarketError> {
        let assignments = self.repo.list_assignments(user_id, None).await?;
        let mut instances = Vec::new();
        for row in self.repo.list_instances(user_id).await? {
            let inst = StoredInstance::from_row(row)?;
            let secrets = self.set_secret_keys(user_id, &inst).await?;
            // Judge the row as if it were active: dormant rows show what blocks activation.
            let mut spec = inst.spec();
            spec.status = "active";
            let mut problems = instance_problems(&spec, &inst.template, &secrets);
            if inst.tier == "read" && inst.tools_allow.as_ref().is_some_and(Vec::is_empty) {
                problems.push("allowlist is empty: no tool is exposed".to_owned());
            }
            instances.push(McpMarketInstanceStatus {
                deliverable: inst.status == "active" && problems.is_empty(),
                id: inst.row.id.clone(),
                name: inst.row.name.clone(),
                status: inst.status.clone(),
                problems,
                assignments: assignments.iter().filter(|a| a.mcp_server_id == inst.row.id).count(),
            });
        }
        Ok(McpMarketStatusResponse {
            instances,
            drift: None,
            drift_available: false,
        })
    }

    /// Kit-lock snapshot for one project (whole-project assignments; assistant
    /// rows are the AionUi channel) or for the global targets.
    pub async fn snapshot(&self, user_id: &str, project_id: Option<&str>, global: bool) -> Result<Value, MarketError> {
        if global == project_id.is_some() {
            return Err(invalid("pass exactly one of project_id or global=true"));
        }
        let assignments = self.repo.list_assignments(user_id, None).await?;
        let mut out = Vec::new();
        for row in self.repo.list_instances(user_id).await? {
            let inst = StoredInstance::from_row(row)?;
            let mut clis: Vec<String> = assignments
                .iter()
                .filter(|a| a.mcp_server_id == inst.row.id && a.assistant_id.is_none())
                .filter(|a| match project_id {
                    Some(p) => a.project_id.as_deref() == Some(p),
                    None => a.project_id.is_none(),
                })
                .map(|a| a.cli.clone())
                .filter(|c| inst.clis.contains(c))
                .collect();
            clis.sort();
            clis.dedup();
            if clis.is_empty() || (global && !GLOBAL_IDS.contains(&inst.row.name.as_str())) {
                continue;
            }
            out.push(lock_instance(&inst, clis));
        }
        out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        let mut lock = json!({ "version": 1, "scope": if global { "global" } else { "project" }, "instances": out });
        if let Some(p) = project_id {
            lock["project"] = json!({ "id": p });
        }
        Ok(lock)
    }
}

fn lock_instance(inst: &StoredInstance, clis: Vec<String>) -> Value {
    let t = &inst.template;
    let header_keys: Vec<&String> = t.headers.values().collect();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut plain_keys = Vec::new();
    for (key, field) in &t.env {
        match field.kind {
            McpMarketEnvKind::Plain => {
                if let Some(v) = inst.plain_env.get(key) {
                    if !header_keys.contains(&key) {
                        env.insert(key.clone(), v.clone());
                    }
                    if is_secret_key(key) {
                        plain_keys.push(key.clone());
                    }
                }
            }
            McpMarketEnvKind::Secret => {
                if let Some(name) = inst.secret_refs.get(key)
                    && !header_keys.contains(&key)
                {
                    env.insert(key.clone(), format!("${{{name}}}"));
                }
            }
        }
    }
    let mut tools = serde_json::Map::new();
    if inst.tier == "read" {
        tools.insert("allow".into(), json!(inst.tools_allow.clone().unwrap_or_default()));
    }
    tools.insert("block".into(), json!(inst.tools_block));
    tools.insert("seen".into(), json!(inst.tools_seen()));
    let mut v = json!({
        "id": inst.row.name,
        "template": t.id,
        "environment": inst.environment,
        "tier": inst.tier,
        "status": inst.status,
        "enforcement": inst.enforcement,
        "transport": t.transport,
        "clis": clis,
        "env": env,
        "plain_keys": plain_keys,
        "tools": tools,
    });
    if let Some(r) = &inst.approval_ref {
        v["approval_ref"] = json!(r);
    }
    if t.transport == "stdio" {
        if let Some(cmd) = &t.command {
            // G1: launch parameters resolved (a missing one stays a `${KEY}` ref).
            let cmd = substitute_command(cmd, t, &inst.plain_env).unwrap_or_else(|_| cmd.clone());
            v["command"] = json!(cmd.command);
            v["args"] = json!(cmd.args);
        }
    } else {
        let url = t
            .url
            .as_deref()
            .map(|u| substitute(u, t, &inst.plain_env).unwrap_or_else(|_| u.to_owned()));
        v["url"] = json!(url);
        let mut headers = BTreeMap::new();
        for (header, key) in &t.headers {
            let value = match t.env.get(key).map(|f| f.kind) {
                Some(McpMarketEnvKind::Secret) => inst.secret_refs.get(key).map(|n| format!("${{{n}}}")),
                Some(McpMarketEnvKind::Plain) => inst.plain_env.get(key).cloned(),
                None => None,
            };
            if let Some(value) = value {
                headers.insert(header.clone(), value);
            }
        }
        v["headers"] = json!(headers);
    }
    v
}
