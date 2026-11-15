//! A market row decoded from its JSON columns (migration 904).

use std::collections::BTreeMap;

use aionui_api_types::{McpMarketEnvKind, McpMarketTemplate};
use aionui_db::models::McpMarketRow;

use super::error::MarketError;
use super::rules::InstanceSpec;

/// Command stored in `transport_config` of every market row. Any path that
/// ignores the market columns and spawns it directly gets a launcher that
/// refuses (no `--project`/`--global`, no `--cli`): fail closed.
pub const PLACEHOLDER_COMMAND: &str = "aioncore";

/// `transport_config` of a market row (always stdio, launcher placeholder).
pub fn placeholder_transport_config(name: &str) -> String {
    serde_json::json!({ "command": PLACEHOLDER_COMMAND, "args": ["mcp", "exec", name], "env": {} }).to_string()
}

#[derive(Debug, Clone)]
pub struct StoredInstance {
    pub row: McpMarketRow,
    pub template: McpMarketTemplate,
    pub status: String,
    pub environment: String,
    pub tier: String,
    pub enforcement: String,
    pub clis: Vec<String>,
    pub plain_env: BTreeMap<String, String>,
    /// Template secret key -> ref name.
    pub secret_refs: BTreeMap<String, String>,
    pub tools_allow: Option<Vec<String>>,
    pub tools_block: Vec<String>,
    pub approval_ref: Option<String>,
}

fn json_or<T: serde::de::DeserializeOwned + Default>(value: &Option<String>) -> Result<T, MarketError> {
    match value.as_deref() {
        Some(text) if !text.is_empty() => Ok(serde_json::from_str(text)?),
        _ => Ok(T::default()),
    }
}

impl StoredInstance {
    /// Decode a market row; `Err` for an ordinary row or broken JSON.
    pub fn from_row(row: McpMarketRow) -> Result<Self, MarketError> {
        let status = row
            .market_status
            .clone()
            .ok_or_else(|| MarketError::NotFound(format!("MCP market instance {}", row.id)))?;
        let template: McpMarketTemplate = match row.market_template.as_deref() {
            Some(text) => serde_json::from_str(text)?,
            None => {
                return Err(MarketError::Internal(format!(
                    "instance {} has no template snapshot",
                    row.name
                )));
            }
        };
        Ok(Self {
            status,
            environment: row.market_environment.clone().unwrap_or_default(),
            tier: row.market_tier.clone().unwrap_or_default(),
            enforcement: row.market_enforcement.clone().unwrap_or_default(),
            clis: json_or(&row.market_clis)?,
            plain_env: json_or(&row.market_plain_env)?,
            secret_refs: json_or(&row.market_secret_refs)?,
            tools_allow: match row.market_tools_allow.as_deref() {
                Some(text) if !text.is_empty() => Some(serde_json::from_str(text)?),
                _ => None,
            },
            tools_block: json_or(&row.market_tools_block)?,
            approval_ref: row.market_approval_ref.clone(),
            template,
            row,
        })
    }

    pub fn spec(&self) -> InstanceSpec<'_> {
        InstanceSpec {
            name: &self.row.name,
            environment: &self.environment,
            tier: &self.tier,
            status: &self.status,
            clis: &self.clis,
            plain_env: &self.plain_env,
            tools_allow: self.tools_allow.as_deref(),
            tools_block: &self.tools_block,
            approval_ref: self.approval_ref.as_deref(),
        }
    }

    /// Template keys declared `secret`.
    pub fn secret_keys(&self) -> Vec<String> {
        secret_keys(&self.template)
    }

    /// Tool names seen at the last connection test.
    pub fn tools_seen(&self) -> Vec<String> {
        let Some(text) = self.row.tools.as_deref() else {
            return Vec::new();
        };
        let tools: Vec<serde_json::Value> = serde_json::from_str(text).unwrap_or_default();
        let mut names: Vec<String> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_owned))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

pub fn secret_keys(template: &McpMarketTemplate) -> Vec<String> {
    template
        .env
        .iter()
        .filter(|(_, f)| f.kind == McpMarketEnvKind::Secret)
        .map(|(k, _)| k.clone())
        .collect()
}
