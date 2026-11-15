//! What `aioncore mcp exec` may start (R9, R1 #1–#3): the decision is read
//! from the database now, never taken from the caller's arguments. The
//! launcher is a correctness gate, not a security boundary against a root
//! agent (the DB and its key are readable to root; `/proc/<pid>/environ`
//! shows the child's env).

use aionui_db::IMcpMarketRepository;

use super::error::MarketError;
use super::proxy::ToolPolicy;
use super::rules::GLOBAL_IDS;
use super::secrets::decrypt_declared;
use super::stored::StoredInstance;
use crate::secret_mask::is_secret_key;

/// Where the launcher was asked to serve the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchScope<'a> {
    Global,
    Project {
        project_id: &'a str,
        /// `Some` = the AionUi assistant channel; `None` = the native project files.
        assistant_id: Option<&'a str>,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct LaunchRequest<'a> {
    pub user_id: &'a str,
    /// Instance id = `mcp_servers.name`.
    pub instance: &'a str,
    pub scope: LaunchScope<'a>,
    pub cli: &'a str,
}

/// The resolved child process. `env` holds plain values and decrypted
/// secrets: `Debug` prints key names only.
pub struct LaunchPlan {
    pub instance_id: String,
    pub instance_name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub policy: ToolPolicy,
}

impl std::fmt::Debug for LaunchPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("LaunchPlan")
            .field("instance", &self.instance_name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_keys", &keys)
            .finish_non_exhaustive()
    }
}

fn refuse(msg: impl Into<String>) -> MarketError {
    MarketError::Forbidden(msg.into())
}

/// Every check that must hold now: market row, not deleted, `active`,
/// CLI allowed, R3 backstops and the exact assignment.
async fn allowed_instance(
    repo: &dyn IMcpMarketRepository,
    req: &LaunchRequest<'_>,
) -> Result<StoredInstance, MarketError> {
    let row = repo
        .find_by_name_any(req.user_id, req.instance)
        .await?
        .ok_or_else(|| refuse(format!("{} is not an MCP market instance", req.instance)))?;
    if !row.is_market() {
        return Err(refuse(format!("{} is not an MCP market instance", req.instance)));
    }
    if row.deleted_at.is_some() {
        return Err(refuse(format!("{} is deleted", req.instance)));
    }
    let inst = StoredInstance::from_row(row)?;
    if inst.status != "active" {
        return Err(refuse(format!("{} is {}", req.instance, inst.status)));
    }
    if !inst.clis.iter().any(|c| c == req.cli) {
        return Err(refuse(format!("{} is not enabled for {}", req.instance, req.cli)));
    }
    if inst.tier == "write" && req.cli != "claude" {
        return Err(refuse(format!(
            "{} is a write instance (Claude only, R3)",
            req.instance
        )));
    }
    if inst.tier == "write" && inst.environment == "production" && inst.approval_ref.is_none() {
        return Err(refuse(format!("{} has no owner exception record", req.instance)));
    }
    let (project, assistant) = match req.scope {
        LaunchScope::Global => {
            if !GLOBAL_IDS.contains(&req.instance) || inst.tier == "write" {
                return Err(refuse(format!("{} may not run globally (R2)", req.instance)));
            }
            (None, None)
        }
        LaunchScope::Project {
            project_id,
            assistant_id,
        } => (Some(project_id), assistant_id),
    };
    if !repo
        .has_assignment(req.user_id, &inst.row.id, project, assistant, req.cli)
        .await?
    {
        let where_ = match (project, assistant) {
            (None, _) => "globally".to_owned(),
            (Some(p), None) => format!("to project {p}"),
            (Some(p), Some(a)) => format!("to assistant {a} in project {p}"),
        };
        return Err(refuse(format!(
            "{} is not assigned {where_} for {}",
            req.instance, req.cli
        )));
    }
    Ok(inst)
}

/// Resolve the child process for a launch, or refuse with a reason (names
/// only, no values).
pub async fn resolve_launch(
    repo: &dyn IMcpMarketRepository,
    encryption_key: &[u8; 32],
    req: &LaunchRequest<'_>,
) -> Result<LaunchPlan, MarketError> {
    let inst = allowed_instance(repo, req).await?;
    let t = &inst.template;
    let cmd = match t.transport.as_str() {
        "stdio" => t.command.clone(),
        // HTTP instances run through the template's pinned stdio bridge, which
        // reads URL/headers from its env (PLAN §3; P4 not yet verified).
        _ => t.bridge.clone(),
    }
    .ok_or_else(|| refuse(format!("template {} has no stdio command for the launcher", t.id)))?;

    let mut env: Vec<(String, String)> = Vec::new();
    for (key, value) in &inst.plain_env {
        if t.env.contains_key(key) {
            env.push((key.clone(), value.clone()));
        }
    }
    let secrets = decrypt_declared(repo, encryption_key, req.user_id, &inst).await?;
    for (key, field) in &t.env {
        if field.required
            && field.kind == aionui_api_types::McpMarketEnvKind::Secret
            && !secrets.iter().any(|(k, _)| k == key)
        {
            return Err(refuse(format!("{} is missing required secret {key}", req.instance)));
        }
    }
    env.extend(secrets);
    if t.transport == "http"
        && let Some(url) = &t.url
    {
        env.push(("MCP_SERVER_URL".to_owned(), url.clone()));
    }
    let policy = ToolPolicy::for_tier(&inst.tier, inst.tools_allow.as_deref(), &inst.tools_block);
    Ok(LaunchPlan {
        instance_id: inst.row.id.clone(),
        instance_name: inst.row.name.clone(),
        command: cmd.command,
        args: cmd.args,
        env,
        policy,
    })
}

/// Re-check that the launch is still allowed (status, deletion, assignment).
/// Used by the proxy before tool calls and periodically.
pub async fn still_allowed(repo: &dyn IMcpMarketRepository, req: &LaunchRequest<'_>) -> bool {
    allowed_instance(repo, req).await.is_ok()
}

/// Parent env names the MCP child may inherit (R1 #3). Everything else is
/// dropped, including every `AIONUI_*` runtime value and the conversation
/// runtime token.
const ENV_ALLOW: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
];
const ENV_ALLOW_PREFIXES: &[&str] = &["LC_", "npm_config_", "NPM_CONFIG_"];

/// Whether a parent env var may pass to the MCP child.
pub fn inherit_env_allowed(key: &str) -> bool {
    if key.starts_with("AIONUI_") || is_secret_key(key) {
        return false;
    }
    ENV_ALLOW.contains(&key) || ENV_ALLOW_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// The child's complete env: allowlisted parent vars, then the runtime's own
/// resolution env, then the plan's plain values and secrets (last wins).
pub fn child_env<I>(parent: I, runtime: &[(String, String)], plan: &LaunchPlan) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    child_env_with(parent, runtime, &plan.env)
}

/// [`child_env`] for an explicit instance env (the market connection test
/// spawns the same env as the launcher, TASK-70).
pub fn child_env_with<I>(parent: I, runtime: &[(String, String)], env: &[(String, String)]) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut out: Vec<(String, String)> = parent.into_iter().filter(|(k, _)| inherit_env_allowed(k)).collect();
    for (k, v) in runtime.iter().chain(env.iter()) {
        out.retain(|(existing, _)| existing != k);
        out.push((k.clone(), v.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_allowlist_drops_runtime_secrets() {
        for key in [
            "AIONUI_RUNTIME_TOKEN",
            "AIONUI_USER_ID",
            "AIONUI_ENCRYPTION_SECRET",
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "npm_config__auth",
            "NPM_CONFIG_TOKEN",
            "JWT_SECRET",
            "RANDOM_VAR",
        ] {
            assert!(!inherit_env_allowed(key), "{key}");
        }
        for key in ["PATH", "HOME", "LC_ALL", "npm_config_cache", "HTTPS_PROXY"] {
            assert!(inherit_env_allowed(key), "{key}");
        }
    }
}
