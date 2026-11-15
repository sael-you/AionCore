//! Market templates come from the data-only catalogue extension
//! (`studio-mcp-catalog`, `contributes.mcpServers[]`, each entry carrying a
//! `market` object). `aionui-mcp` cannot depend on `aionui-extension` (same
//! layer), so the composition layer implements [`IMcpTemplateCatalog`] over
//! the extension registry.

use aionui_api_types::McpMarketTemplate;
use tracing::warn;

use super::error::MarketError;
use super::rules::template_problems;

/// Name of the only extension whose `mcpServers` entries are market templates.
pub const CATALOG_EXTENSION: &str = "studio-mcp-catalog";

/// Source of raw catalogue entries.
#[async_trait::async_trait]
pub trait IMcpTemplateCatalog: Send + Sync {
    /// The `contributes.mcpServers[]` entries of [`CATALOG_EXTENSION`] as
    /// resolved JSON objects (`{id, name, description?, market: {...}}`).
    /// `Err` when the catalogue cannot be read at all.
    async fn entries(&self, user_id: &str) -> Result<Vec<serde_json::Value>, String>;
}

/// A fixed list of entries (tests, and composition without extensions).
pub struct StaticTemplateCatalog {
    entries: Vec<serde_json::Value>,
}

impl StaticTemplateCatalog {
    pub fn new(entries: Vec<serde_json::Value>) -> Self {
        Self { entries }
    }
}

#[async_trait::async_trait]
impl IMcpTemplateCatalog for StaticTemplateCatalog {
    async fn entries(&self, _user_id: &str) -> Result<Vec<serde_json::Value>, String> {
        Ok(self.entries.clone())
    }
}

/// Parse one catalogue entry. The entry's `id`/`name`/`description` win over
/// the same keys inside `market`.
pub fn parse_template(entry: &serde_json::Value) -> Result<McpMarketTemplate, String> {
    let obj = entry.as_object().ok_or("catalogue entry is not an object")?;
    let mut market = obj
        .get("market")
        .and_then(|m| m.as_object())
        .cloned()
        .ok_or("catalogue entry has no market object")?;
    for key in ["id", "name", "description"] {
        if let Some(v) = obj.get(key) {
            market.insert(key.to_owned(), v.clone());
        }
    }
    let template: McpMarketTemplate =
        serde_json::from_value(serde_json::Value::Object(market)).map_err(|e| format!("invalid template: {e}"))?;
    let problems = template_problems(&template);
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    Ok(template)
}

/// Every valid template of the catalogue; invalid entries are skipped with a warning.
pub async fn load_templates(
    catalog: &dyn IMcpTemplateCatalog,
    user_id: &str,
) -> Result<Vec<McpMarketTemplate>, MarketError> {
    let entries = catalog
        .entries(user_id)
        .await
        .map_err(|e| MarketError::Unavailable(format!("MCP catalogue: {e}")))?;
    let mut out: Vec<McpMarketTemplate> = Vec::with_capacity(entries.len());
    for entry in &entries {
        match parse_template(entry) {
            Ok(t) if out.iter().any(|x| x.id == t.id) => {
                warn!(template = %t.id, "mcp market: duplicate template id in catalogue; keeping the first");
            }
            Ok(t) => out.push(t),
            Err(problem) => {
                let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                warn!(template = id, problem = %problem, "mcp market: skipping invalid catalogue template");
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

pub async fn find_template(
    catalog: &dyn IMcpTemplateCatalog,
    user_id: &str,
    template_id: &str,
) -> Result<McpMarketTemplate, MarketError> {
    load_templates(catalog, user_id)
        .await?
        .into_iter()
        .find(|t| t.id == template_id)
        .ok_or_else(|| MarketError::NotFound(format!("template {template_id}")))
}
