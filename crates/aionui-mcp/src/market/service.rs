//! Market instance service: templates, instance CRUD/duplicate/revoke and the
//! owner approval record. Secrets, assignments and status live in sibling
//! modules as further `impl McpMarketService` blocks.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use aionui_api_types::{
    CreateMcpMarketInstanceRequest, McpMarketEnvKind, McpMarketInstanceResponse, McpMarketSecretStatus,
    McpMarketTemplate, UpdateMcpMarketInstanceRequest,
};
use aionui_db::models::McpSecretRow;
use aionui_db::{IMcpMarketRepository, IMcpServerRepository, MarketInstanceParams, UpdateMarketInstanceParams};
use tracing::info;

use super::error::{MarketError, invalid};
use super::rules::{InstanceSpec, instance_problems, secret_ref_name};
use super::stored::{StoredInstance, placeholder_transport_config, secret_keys};
use super::template::{IMcpTemplateCatalog, find_template, load_templates};
use crate::types::McpServerTransport;

/// Applies market changes to assistant MCP bindings in-process (the assistant
/// service lives in a same-layer crate, so composition implements this).
#[async_trait::async_trait]
pub trait IMcpAssistantBinder: Send + Sync {
    /// Append `mcp_server_id` to the assistant's fixed MCP list (other picks
    /// kept; mode switched to `fixed`, R8).
    async fn bind(&self, user_id: &str, assistant_id: &str, mcp_server_id: &str) -> Result<(), String>;
    /// Remove `mcp_server_id` from the assistant's fixed MCP list.
    async fn unbind(&self, user_id: &str, assistant_id: &str, mcp_server_id: &str) -> Result<(), String>;
}

/// The MCP market business service.
#[derive(Clone)]
pub struct McpMarketService {
    pub(crate) repo: Arc<dyn IMcpMarketRepository>,
    pub(crate) servers: Arc<dyn IMcpServerRepository>,
    pub(crate) catalog: Arc<dyn IMcpTemplateCatalog>,
    pub(crate) encryption_key: [u8; 32],
    pub(crate) binder: Option<Arc<dyn IMcpAssistantBinder>>,
}

impl McpMarketService {
    pub fn new(
        repo: Arc<dyn IMcpMarketRepository>,
        servers: Arc<dyn IMcpServerRepository>,
        catalog: Arc<dyn IMcpTemplateCatalog>,
        encryption_key: [u8; 32],
    ) -> Self {
        Self {
            repo,
            servers,
            catalog,
            encryption_key,
            binder: None,
        }
    }

    pub fn with_assistant_binder(mut self, binder: Arc<dyn IMcpAssistantBinder>) -> Self {
        self.binder = Some(binder);
        self
    }

    pub fn repo(&self) -> &Arc<dyn IMcpMarketRepository> {
        &self.repo
    }

    pub async fn list_templates(&self, user_id: &str) -> Result<Vec<McpMarketTemplate>, MarketError> {
        load_templates(self.catalog.as_ref(), user_id).await
    }

    pub(crate) async fn load(&self, user_id: &str, id: &str) -> Result<StoredInstance, MarketError> {
        let row = self
            .repo
            .find_instance(user_id, id)
            .await?
            .ok_or_else(|| MarketError::NotFound(format!("MCP market instance {id}")))?;
        StoredInstance::from_row(row)
    }

    pub async fn list_instances(&self, user_id: &str) -> Result<Vec<McpMarketInstanceResponse>, MarketError> {
        let secrets = self.secrets_by_instance(user_id).await?;
        // A catalogue outage must not hide instances: versions just read as unknown.
        let templates = load_templates(self.catalog.as_ref(), user_id).await.unwrap_or_default();
        let mut out = Vec::new();
        for row in self.repo.list_instances(user_id).await? {
            let inst = StoredInstance::from_row(row)?;
            let current = templates.iter().find(|t| t.id == inst.template.id);
            out.push(to_response(
                &inst,
                secrets.get(&inst.row.id).map(Vec::as_slice).unwrap_or(&[]),
                current,
            ));
        }
        Ok(out)
    }

    pub async fn get_instance(&self, user_id: &str, id: &str) -> Result<McpMarketInstanceResponse, MarketError> {
        let inst = self.load(user_id, id).await?;
        let secrets = self.repo.list_instance_secrets(user_id, id).await?;
        let current = self.current_template(user_id, &inst.template.id).await;
        Ok(to_response(&inst, &secrets, current.as_ref()))
    }

    pub(crate) async fn secrets_by_instance(
        &self,
        user_id: &str,
    ) -> Result<HashMap<String, Vec<McpSecretRow>>, MarketError> {
        let mut map: HashMap<String, Vec<McpSecretRow>> = HashMap::new();
        for secret in self.repo.list_secrets(user_id).await? {
            map.entry(secret.instance_id.clone()).or_default().push(secret);
        }
        Ok(map)
    }

    /// Reject names used by builtin or ordinary rows; return the soft-deleted
    /// row to revive, if any (R1 #10).
    async fn claim_name(&self, user_id: &str, name: &str) -> Result<Option<String>, MarketError> {
        let Some(existing) = self.repo.find_by_name_any(user_id, name).await? else {
            return Ok(None);
        };
        if existing.deleted_at.is_some() {
            let builtin = self
                .servers
                .find_by_id_any(user_id, &existing.id)
                .await?
                .is_some_and(|row| row.builtin);
            if builtin {
                return Err(MarketError::Conflict(format!(
                    "builtin MCP server name '{name}' is reserved"
                )));
            }
            return Ok(Some(existing.id));
        }
        Err(MarketError::Conflict(format!(
            "MCP server name '{name}' already exists"
        )))
    }

    pub async fn create_instance(
        &self,
        user_id: &str,
        req: CreateMcpMarketInstanceRequest,
    ) -> Result<McpMarketInstanceResponse, MarketError> {
        let template = find_template(self.catalog.as_ref(), user_id, &req.template_id).await?;
        let mut plain_env = req.plain_env;
        apply_defaults(&template, &req.tier, &mut plain_env);
        let status = req.status.unwrap_or_else(|| "dormant".to_owned());
        let spec = InstanceSpec {
            name: &req.name,
            environment: &req.environment,
            tier: &req.tier,
            status: &status,
            clis: &req.clis,
            plain_env: &plain_env,
            tools_allow: req.tools_allow.as_deref(),
            tools_block: &req.tools_block,
            approval_ref: None,
        };
        // A new instance has no secrets yet, so it can only start dormant/revoked
        // when the template requires any.
        reject(instance_problems(&spec, &template, &[]))?;
        let revive = self.claim_name(user_id, &req.name).await?;

        let secret_refs: BTreeMap<String, String> = secret_keys(&template)
            .into_iter()
            .map(|k| {
                let name = secret_ref_name(&req.name, &k);
                (k, name)
            })
            .collect();
        let row = self
            .write_new(
                user_id,
                revive.as_deref(),
                NewInstance {
                    name: &req.name,
                    description: req.description.as_deref(),
                    template: &template,
                    status: &status,
                    environment: &req.environment,
                    tier: &req.tier,
                    clis: &req.clis,
                    plain_env: &plain_env,
                    secret_refs: &secret_refs,
                    tools_allow: req.tools_allow.as_deref(),
                    tools_block: &req.tools_block,
                },
            )
            .await?;
        info!(instance = %req.name, template = %template.id, tier = %req.tier, status = %status, revived = revive.is_some(), "mcp market: instance created");
        let inst = StoredInstance::from_row(row)?;
        Ok(to_response(&inst, &[], Some(&template)))
    }

    async fn write_new(
        &self,
        user_id: &str,
        revive: Option<&str>,
        n: NewInstance<'_>,
    ) -> Result<aionui_db::models::McpMarketRow, MarketError> {
        let template_json = serde_json::to_string(n.template)?;
        let clis_json = serde_json::to_string(n.clis)?;
        let plain_env_json = serde_json::to_string(n.plain_env)?;
        let secret_refs_json = serde_json::to_string(n.secret_refs)?;
        let tools_allow_json = n.tools_allow.map(serde_json::to_string).transpose()?;
        let tools_block_json = serde_json::to_string(n.tools_block)?;
        let transport_config = placeholder_transport_config(n.name);
        let params = MarketInstanceParams {
            user_id,
            name: n.name,
            description: n.description,
            transport_type: "stdio",
            transport_config: &transport_config,
            status: n.status,
            template_id: &n.template.id,
            template_json: &template_json,
            environment: n.environment,
            tier: n.tier,
            enforcement: &n.template.enforcement,
            clis_json: &clis_json,
            plain_env_json: &plain_env_json,
            secret_refs_json: &secret_refs_json,
            tools_allow_json: tools_allow_json.as_deref(),
            tools_block_json: &tools_block_json,
            approval_ref: None,
        };
        Ok(match revive {
            Some(id) => self.repo.revive_instance(user_id, id, &params).await?,
            None => self.repo.create_instance(&params).await?,
        })
    }

    pub async fn update_instance(
        &self,
        user_id: &str,
        id: &str,
        req: UpdateMcpMarketInstanceRequest,
    ) -> Result<McpMarketInstanceResponse, MarketError> {
        let mut inst = self.load(user_id, id).await?;
        // Only what the request changes is written (TASK-71); the checks below
        // use the loaded row, so the write is also conditional on it being
        // unchanged (409 otherwise).
        let tier_changed = req.tier.as_ref().is_some_and(|t| *t != inst.tier);
        let write_plain_env = req.plain_env.is_some() || tier_changed;
        let write_tools_allow = req.tools_allow.is_some() || (tier_changed && req.tier.as_deref() == Some("write"));
        if let Some(v) = req.environment.clone() {
            inst.environment = v;
        }
        if let Some(v) = req.tier.clone() {
            inst.tier = v;
        }
        if let Some(v) = req.status.clone() {
            inst.status = v;
        }
        if let Some(v) = req.clis.clone() {
            inst.clis = v;
        }
        if let Some(v) = req.plain_env.clone() {
            inst.plain_env = v;
        }
        apply_defaults(&inst.template, &inst.tier.clone(), &mut inst.plain_env);
        if let Some(v) = req.tools_allow.clone() {
            inst.tools_allow = Some(v);
        } else if inst.tier == "write" {
            // A tier change to write drops the allowlist (write = blocklist, R6).
            inst.tools_allow = None;
        }
        if let Some(v) = req.tools_block.clone() {
            inst.tools_block = v;
        }
        let secrets = self.set_secret_keys(user_id, &inst).await?;
        reject(instance_problems(&inst.spec(), &inst.template, &secrets))?;
        // Assignments must stay valid for the new CLIs/tier (R2, R3).
        for a in self.repo.list_assignments(user_id, Some(id)).await? {
            if !inst.clis.contains(&a.cli) {
                return Err(invalid(format!(
                    "instance \"{}\" is assigned to {}; remove that assignment before dropping the CLI",
                    inst.row.name, a.cli
                )));
            }
        }
        let clis_json = serde_json::to_string(&inst.clis)?;
        let plain_env_json = serde_json::to_string(&inst.plain_env)?;
        let tools_allow_json = inst.tools_allow.as_ref().map(serde_json::to_string).transpose()?;
        let tools_block_json = serde_json::to_string(&inst.tools_block)?;
        let row = self
            .repo
            .update_instance(
                user_id,
                id,
                &UpdateMarketInstanceParams {
                    description: req.description.as_deref().map(Some),
                    status: req.status.is_some().then_some(inst.status.as_str()),
                    environment: req.environment.is_some().then_some(inst.environment.as_str()),
                    tier: req.tier.is_some().then_some(inst.tier.as_str()),
                    clis_json: req.clis.is_some().then_some(clis_json.as_str()),
                    plain_env_json: write_plain_env.then_some(plain_env_json.as_str()),
                    tools_allow_json: write_tools_allow.then_some(tools_allow_json.as_deref()),
                    tools_block_json: req.tools_block.is_some().then_some(tools_block_json.as_str()),
                    expected_updated_at: Some(inst.row.updated_at),
                    ..Default::default()
                },
            )
            .await?;
        info!(instance = %inst.row.name, status = %inst.status, tier = %inst.tier, "mcp market: instance updated");
        let inst = StoredInstance::from_row(row)?;
        let secrets = self.repo.list_instance_secrets(user_id, id).await?;
        let current = self.current_template(user_id, &inst.template.id).await;
        Ok(to_response(&inst, &secrets, current.as_ref()))
    }

    /// Template secret keys of `inst` that hold a value.
    pub(crate) async fn set_secret_keys(
        &self,
        user_id: &str,
        inst: &StoredInstance,
    ) -> Result<Vec<String>, MarketError> {
        let rows = self.repo.list_instance_secrets(user_id, &inst.row.id).await?;
        Ok(inst
            .secret_refs
            .iter()
            .filter(|(_, name)| rows.iter().any(|r| &r.name == *name))
            .map(|(key, _)| key.clone())
            .collect())
    }

    /// Duplicate = a new dormant instance with the same template, environment,
    /// tier, CLIs, plain env and tool lists; secrets are NOT copied (each
    /// secret belongs to one instance, R1 #1).
    pub async fn duplicate_instance(
        &self,
        user_id: &str,
        id: &str,
        name: &str,
    ) -> Result<McpMarketInstanceResponse, MarketError> {
        let src = self.load(user_id, id).await?;
        self.create_instance(
            user_id,
            CreateMcpMarketInstanceRequest {
                name: name.to_owned(),
                template_id: src.template.id.clone(),
                description: src.row.description.clone(),
                environment: src.environment.clone(),
                tier: src.tier.clone(),
                status: Some("dormant".to_owned()),
                clis: src.clis.clone(),
                plain_env: src.plain_env.clone(),
                tools_allow: src.tools_allow.clone(),
                tools_block: src.tools_block.clone(),
            },
        )
        .await
    }

    /// Status `revoked`: the gate and the launcher refuse it from now on
    /// (running launchers stop at their next check). Assignments are kept so
    /// the owner can re-activate.
    pub async fn revoke_instance(&self, user_id: &str, id: &str) -> Result<McpMarketInstanceResponse, MarketError> {
        // One column, unconditional: a revoke always wins and never rewrites
        // anything else (TASK-71).
        let row = self
            .repo
            .update_instance(
                user_id,
                id,
                &UpdateMarketInstanceParams {
                    status: Some("revoked"),
                    ..Default::default()
                },
            )
            .await?;
        let inst = StoredInstance::from_row(row)?;
        info!(instance = %inst.row.name, "mcp market: instance revoked");
        let secrets = self.repo.list_instance_secrets(user_id, id).await?;
        let current = self.current_template(user_id, &inst.template.id).await;
        Ok(to_response(&inst, &secrets, current.as_ref()))
    }

    /// Soft-delete: assistant bindings are removed, then the row's assignments
    /// and secrets are deleted with it.
    pub async fn delete_instance(&self, user_id: &str, id: &str) -> Result<(), MarketError> {
        let inst = self.load(user_id, id).await?;
        for a in self.repo.list_assignments(user_id, Some(id)).await? {
            if let Some(assistant) = a.assistant_id.as_deref() {
                self.unbind_if_last(user_id, &a.mcp_server_id, assistant, Some(&a.id))
                    .await?;
            }
        }
        self.repo.delete_instance(user_id, id).await?;
        info!(instance = %inst.row.name, "mcp market: instance deleted");
        Ok(())
    }

    /// Record (or clear) the owner's written exception for a production write instance.
    pub async fn set_approval(
        &self,
        user_id: &str,
        id: &str,
        approval_ref: Option<String>,
    ) -> Result<McpMarketInstanceResponse, MarketError> {
        let mut inst = self.load(user_id, id).await?;
        inst.approval_ref = approval_ref;
        if inst.approval_ref.is_none()
            && inst.tier == "write"
            && inst.environment == "production"
            && inst.status == "active"
        {
            // Clearing the record of an active production write instance makes it dormant.
            inst.status = "dormant".to_owned();
        }
        let secrets = self.set_secret_keys(user_id, &inst).await?;
        reject(instance_problems(&inst.spec(), &inst.template, &secrets))?;
        let row = self
            .repo
            .update_instance(
                user_id,
                id,
                &UpdateMarketInstanceParams {
                    approval_ref: Some(inst.approval_ref.as_deref()),
                    status: Some(&inst.status),
                    expected_updated_at: Some(inst.row.updated_at),
                    ..Default::default()
                },
            )
            .await?;
        info!(instance = %inst.row.name, recorded = inst.approval_ref.is_some(), "mcp market: approval record changed");
        let inst = StoredInstance::from_row(row)?;
        let secrets = self.repo.list_instance_secrets(user_id, id).await?;
        let current = self.current_template(user_id, &inst.template.id).await;
        Ok(to_response(&inst, &secrets, current.as_ref()))
    }

    /// Re-apply the catalogue's current version of the instance's template
    /// (R10). Secrets and assignments are kept; plain values of keys the new
    /// template no longer declares are dropped, new plain defaults and the
    /// tier's forced env are applied, and new secret keys get their refs.
    /// Refused (instance unchanged) when a required secret of the new version
    /// is not set, or when the instance would break a rule under it.
    pub async fn refresh_template(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<aionui_api_types::McpMarketTemplateRefreshResponse, MarketError> {
        let mut inst = self.load(user_id, id).await?;
        let current = find_template(self.catalog.as_ref(), user_id, &inst.template.id).await?;
        let old = std::mem::replace(&mut inst.template, current.clone());
        let added_keys: Vec<String> = current
            .env
            .keys()
            .filter(|k| !old.env.contains_key(*k))
            .cloned()
            .collect();
        let removed_keys: Vec<String> = old
            .env
            .keys()
            .filter(|k| !current.env.contains_key(*k))
            .cloned()
            .collect();
        let changed = old != current;

        // Plain env: only keys the new template declares plain; then defaults + forced tier env.
        inst.plain_env
            .retain(|k, _| current.env.get(k).is_some_and(|f| f.kind == McpMarketEnvKind::Plain));
        let tier = inst.tier.clone();
        apply_defaults(&current, &tier, &mut inst.plain_env);
        // Secret refs: existing names kept, new secret keys get derived names.
        inst.secret_refs = secret_keys(&current)
            .into_iter()
            .map(|k| {
                let name = inst
                    .secret_refs
                    .get(&k)
                    .cloned()
                    .unwrap_or_else(|| secret_ref_name(&inst.row.name, &k));
                (k, name)
            })
            .collect();
        let stored = self.repo.list_instance_secrets(user_id, id).await?;
        let missing: Vec<&String> = current
            .env
            .iter()
            .filter(|(_, f)| f.kind == McpMarketEnvKind::Secret && f.required)
            .map(|(k, _)| k)
            .filter(|k| {
                !inst
                    .secret_refs
                    .get(*k)
                    .is_some_and(|n| stored.iter().any(|s| &s.name == n))
            })
            .collect();
        if !missing.is_empty() {
            return Err(MarketError::TemplateSecretMissing(format!(
                "template {} {} needs secret {} first; the instance stays on its current version",
                current.id,
                current.version.as_deref().unwrap_or("(unversioned)"),
                missing.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        let secrets_set: Vec<String> = inst
            .secret_refs
            .iter()
            .filter(|(_, n)| stored.iter().any(|s| &s.name == *n))
            .map(|(k, _)| k.clone())
            .collect();
        reject(instance_problems(&inst.spec(), &current, &secrets_set))?;

        let template_json = serde_json::to_string(&current)?;
        let plain_env_json = serde_json::to_string(&inst.plain_env)?;
        let secret_refs_json = serde_json::to_string(&inst.secret_refs)?;
        let row = self
            .repo
            .update_instance(
                user_id,
                id,
                &UpdateMarketInstanceParams {
                    template_json: Some(&template_json),
                    plain_env_json: Some(&plain_env_json),
                    secret_refs_json: Some(&secret_refs_json),
                    enforcement: Some(&current.enforcement),
                    expected_updated_at: Some(inst.row.updated_at),
                    ..Default::default()
                },
            )
            .await?;
        info!(instance = %inst.row.name, template = %current.id, changed, added = added_keys.len(), removed = removed_keys.len(), "mcp market: template refreshed");
        let inst = StoredInstance::from_row(row)?;
        let secrets = self.repo.list_instance_secrets(user_id, id).await?;
        Ok(aionui_api_types::McpMarketTemplateRefreshResponse {
            instance: to_response(&inst, &secrets, Some(&current)),
            from_version: old.version,
            to_version: current.version,
            added_keys,
            removed_keys,
            changed,
        })
    }

    /// The catalogue's current version of `template_id`, if it can be read.
    pub(crate) async fn current_template(&self, user_id: &str, template_id: &str) -> Option<McpMarketTemplate> {
        find_template(self.catalog.as_ref(), user_id, template_id).await.ok()
    }

    /// Whether `id` is a (non-deleted) market row.
    pub async fn is_market_row(&self, user_id: &str, id: &str) -> Result<bool, MarketError> {
        Ok(self.repo.find_instance(user_id, id).await?.is_some())
    }

    /// The real transport of a market row for `POST /api/mcp/test-connection
    /// {id}` (R1 #12): built server-side from the template snapshot, the plain
    /// env and the decrypted secrets; the client's transport is ignored.
    pub async fn test_transport(&self, user_id: &str, id: &str) -> Result<McpServerTransport, MarketError> {
        let inst = self.load(user_id, id).await?;
        // A revoked instance never starts, not even for a test (TASK-70).
        if inst.status == "revoked" {
            return Err(MarketError::Forbidden(format!(
                "\"{}\" is revoked; re-activate it before testing",
                inst.row.name
            )));
        }
        let mut env: HashMap<String, String> = inst.plain_env.clone().into_iter().collect();
        for (key, value) in self.decrypt_instance_secrets(user_id, &inst).await? {
            env.insert(key, value);
        }
        let t = &inst.template;
        match (t.transport.as_str(), &t.command) {
            ("stdio", Some(cmd)) => Ok(McpServerTransport::Stdio {
                command: cmd.command.clone(),
                args: cmd.args.clone(),
                env,
            }),
            ("http", _) => {
                let url = t.url.clone().ok_or_else(|| invalid("template has no url"))?;
                let headers = t
                    .headers
                    .iter()
                    .filter_map(|(header, key)| env.get(key).map(|v| (header.clone(), v.clone())))
                    .collect();
                Ok(McpServerTransport::Http { url, headers })
            }
            _ => Err(invalid("template has no launch command")),
        }
    }
}

struct NewInstance<'a> {
    name: &'a str,
    description: Option<&'a str>,
    template: &'a McpMarketTemplate,
    status: &'a str,
    environment: &'a str,
    tier: &'a str,
    clis: &'a [String],
    plain_env: &'a BTreeMap<String, String>,
    secret_refs: &'a BTreeMap<String, String>,
    tools_allow: Option<&'a [String]>,
    tools_block: &'a [String],
}

/// Fill plain defaults and the tier's forced env (a forced key always wins).
/// Keys only another tier forces are dropped first, so a tier change never
/// keeps the old tier's forced value (e.g. a write flag on a read instance,
/// TASK-72); a declared default then applies again.
fn apply_defaults(template: &McpMarketTemplate, tier: &str, plain_env: &mut BTreeMap<String, String>) {
    let current = template.tier_env.get(tier);
    for (other, forced) in &template.tier_env {
        if other == tier {
            continue;
        }
        for key in forced.keys() {
            if current.is_none_or(|c| !c.contains_key(key)) {
                plain_env.remove(key);
            }
        }
    }
    for (key, field) in &template.env {
        if field.kind == McpMarketEnvKind::Plain
            && let Some(default) = &field.default
        {
            plain_env.entry(key.clone()).or_insert_with(|| default.clone());
        }
    }
    if let Some(forced) = template.tier_env.get(tier) {
        for (key, value) in forced {
            plain_env.insert(key.clone(), value.clone());
        }
    }
}

pub(crate) fn reject(problems: Vec<String>) -> Result<(), MarketError> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(MarketError::Invalid(problems.join("; ")))
    }
}

/// Response mapping: refs, `set` flags and missing keys; never a value.
pub(crate) fn to_response(
    inst: &StoredInstance,
    secrets: &[McpSecretRow],
    current: Option<&McpMarketTemplate>,
) -> McpMarketInstanceResponse {
    let mut statuses = Vec::new();
    let mut missing = Vec::new();
    for (key, field) in &inst.template.env {
        match field.kind {
            McpMarketEnvKind::Secret => {
                let name = inst
                    .secret_refs
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| secret_ref_name(&inst.row.name, key));
                let row = secrets.iter().find(|s| s.name == name);
                if field.required && row.is_none() {
                    missing.push(key.clone());
                }
                statuses.push(McpMarketSecretStatus {
                    key: key.clone(),
                    name,
                    instance_id: inst.row.id.clone(),
                    instance_name: inst.row.name.clone(),
                    required: field.required,
                    set: row.is_some(),
                    updated_at: row.map(|r| r.updated_at),
                });
            }
            McpMarketEnvKind::Plain => {
                if field.required && !inst.plain_env.contains_key(key) {
                    missing.push(key.clone());
                }
            }
        }
    }
    McpMarketInstanceResponse {
        id: inst.row.id.clone(),
        name: inst.row.name.clone(),
        description: inst.row.description.clone(),
        template_id: inst.template.id.clone(),
        template_version: inst.template.version.clone(),
        template_current_version: current.and_then(|t| t.version.clone()),
        template_in_catalogue: current.is_some(),
        template_update_available: current.is_some_and(|t| *t != inst.template),
        transport: inst.template.transport.clone(),
        environment: inst.environment.clone(),
        tier: inst.tier.clone(),
        enforcement: inst.enforcement.clone(),
        status: inst.status.clone(),
        clis: inst.clis.clone(),
        plain_env: inst.plain_env.clone(),
        secrets: statuses,
        missing,
        tools_allow: inst.tools_allow.clone(),
        tools_block: inst.tools_block.clone(),
        tools_seen: inst.tools_seen(),
        approval_ref: inst.approval_ref.clone(),
        last_test_status: inst.row.last_test_status.clone(),
        created_at: inst.row.created_at,
        updated_at: inst.row.updated_at,
    }
}
