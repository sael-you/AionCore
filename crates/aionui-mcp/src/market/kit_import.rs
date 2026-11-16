//! "Import keys from this project's .mcp.json" (card C28 §5, owner
//! 2026-11-16). Only CREDENTIALS are imported: the server always comes from
//! the AionUi catalogue template (its pinned command / URL); the file's
//! command, args, package version and URL are only used to recognise the
//! server and are never stored. The preview never returns a value, a masked
//! value or a command.
//!
//! Nothing here derives `Debug` on a type that holds a file value.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use aionui_api_types::{
    CreateKitRequest, CreateMcpMarketInstanceRequest, KitErrorCode, KitImportMatch, KitImportPreviewResponse,
    KitImportPreviewServer, KitImportRequest, KitImportResponse, KitImportSecret, KitImportServerResult,
    McpMarketEnvKind, McpMarketTemplate,
};
use tracing::{info, warn};

use super::error::{MarketError, kit_error};
use super::rules::NOT_IN_MARKET;
use super::service::McpMarketService;
use super::template::load_templates;

/// Largest `.mcp.json` read (it is a small config file).
const MAX_FILE: u64 = 1024 * 1024;
/// Servers dropped by D50 (besides R11's `NOT_IN_MARKET`): recognised as
/// `none`, never as `custom`.
const DROPPED_D50: &[&str] = &[
    "tavily",
    "notion",
    "figma",
    "postgres",
    "clarity",
    "dataforseo",
    "ref-tools",
    "memory",
    "docker",
];
/// Launchers whose first positional argument is the package.
const PACKAGE_RUNNERS: &[&str] = &["npx", "bunx", "uvx", "pnpx", "pipx"];

/// One server entry of `.mcp.json` (`mcpServers.<name>`). Values stay here.
pub(crate) struct FileServer {
    pub name: String,
    command: Option<String>,
    args: Vec<String>,
    url: Option<String>,
    env: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
}

fn string_map(v: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    v.and_then(|m| m.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the `mcpServers` object of a `.mcp.json`; `None` when malformed.
pub(crate) fn parse_mcp_json(bytes: &[u8]) -> Option<Vec<FileServer>> {
    let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let servers = root.get("mcpServers")?.as_object()?;
    Some(
        servers
            .iter()
            .filter_map(|(name, entry)| {
                let e = entry.as_object()?;
                Some(FileServer {
                    name: name.clone(),
                    command: e.get("command").and_then(|v| v.as_str()).map(str::to_owned),
                    args: e
                        .get("args")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                        .unwrap_or_default(),
                    url: e
                        .get("url")
                        .or_else(|| e.get("serverUrl"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    env: string_map(e.get("env")),
                    headers: string_map(e.get("headers")),
                })
            })
            .collect(),
    )
}

/// Package name without version (`@scope/name@1.2.3` → `@scope/name`,
/// `name==1.0` → `name`), lowercased.
fn strip_version(spec: &str) -> String {
    let spec = spec.split("==").next().unwrap_or(spec);
    let cut = match spec.rfind('@') {
        Some(i) if i > 0 => &spec[..i],
        _ => spec,
    };
    cut.to_ascii_lowercase()
}

/// The package a runner command starts (`npx -y pkg@v ...` → `pkg`).
fn package_identity(command: &str, args: &[String]) -> Option<String> {
    let runner = std::path::Path::new(command)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(command);
    if !PACKAGE_RUNNERS.contains(&runner) {
        return None;
    }
    args.iter()
        .find(|a| !a.starts_with('-') && *a != "dlx" && *a != "run")
        .map(|a| strip_version(a))
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

impl FileServer {
    fn package(&self) -> Option<String> {
        self.command.as_deref().and_then(|c| package_identity(c, &self.args))
    }

    /// Remote URLs of the entry: `url` and any http(s) arg (`mcp-remote <url>`).
    fn hosts(&self) -> BTreeSet<String> {
        self.url
            .iter()
            .chain(
                self.args
                    .iter()
                    .filter(|a| a.starts_with("http://") || a.starts_with("https://")),
            )
            .filter_map(|u| host_of(u))
            .collect()
    }

    fn is_remote(&self) -> bool {
        !self.hosts().is_empty()
    }

    /// Header values: `headers` plus `--header "Name:Value"` args.
    fn header(&self, name: &str) -> Option<String> {
        if let Some((_, v)) = self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            return Some(v.clone());
        }
        let mut it = self.args.iter();
        while let Some(arg) = it.next() {
            if arg == "--header"
                && let Some(value) = it.next()
                && let Some((k, v)) = value.split_once(':')
                && k.trim().eq_ignore_ascii_case(name)
            {
                return Some(v.trim().to_owned());
            }
        }
        None
    }

    fn has_any_credential(&self) -> bool {
        !self.env.is_empty() || !self.headers.is_empty() || self.args.iter().any(|a| a == "--header")
    }

    fn name_tokens(&self) -> Vec<String> {
        self.name
            .to_ascii_lowercase()
            .split(['-', '_', '.', ' '])
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// A usable credential: non-empty and not an unresolved `${VAR}` reference.
fn usable(value: &str) -> Option<String> {
    let v = value.trim();
    (!v.is_empty() && !v.contains("${")).then(|| v.to_owned())
}

fn strip_bearer(value: &str) -> &str {
    let v = value.trim();
    if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
        v[7..].trim()
    } else {
        v
    }
}

/// The file's value for a template secret field: its env under the field
/// name or an alias, or (http templates) the header the template maps onto
/// that field, without a `Bearer ` prefix (the bridge adds it back).
fn secret_value(server: &FileServer, t: &McpMarketTemplate, field: &str, aliases: &[String]) -> Option<String> {
    for key in std::iter::once(field).chain(aliases.iter().map(String::as_str)) {
        if let Some(v) = server.env.get(key).and_then(|v| usable(strip_bearer(v))) {
            return Some(v);
        }
    }
    t.headers
        .iter()
        .filter(|(_, env_key)| env_key.as_str() == field)
        .find_map(|(header, _)| server.header(header))
        .and_then(|v| usable(strip_bearer(&v)))
}

fn template_package(t: &McpMarketTemplate) -> Option<String> {
    t.command.as_ref().and_then(|c| package_identity(&c.command, &c.args))
}

fn template_host(t: &McpMarketTemplate) -> Option<String> {
    t.url.as_deref().and_then(host_of)
}

/// Template id tokens appear contiguously in the server name tokens.
fn name_matches(name_tokens: &[String], id: &str) -> bool {
    let id_tokens: Vec<&str> = id.split('-').collect();
    name_tokens
        .windows(id_tokens.len())
        .any(|w| w.iter().map(String::as_str).eq(id_tokens.iter().copied()))
}

fn dropped(server: &FileServer) -> bool {
    let tokens = server.name_tokens();
    let pkg = server.package().unwrap_or_default();
    let lower = server.name.to_ascii_lowercase();
    NOT_IN_MARKET.iter().any(|p| lower.starts_with(p) || pkg.contains(p))
        || DROPPED_D50
            .iter()
            .any(|d| tokens.iter().any(|t| t == d) || pkg.contains(d))
}

/// Catalogue template by package/command identity or URL host, then by name
/// (longest template id wins); else `none` (R11/D50) or `custom`.
pub(crate) fn match_server<'t>(
    server: &FileServer,
    templates: &'t [McpMarketTemplate],
) -> (KitImportMatch, Option<&'t McpMarketTemplate>) {
    if let Some(pkg) = server.package()
        && let Some(t) = templates
            .iter()
            .find(|t| template_package(t).as_deref() == Some(pkg.as_str()))
    {
        return (KitImportMatch::Catalogue, Some(t));
    }
    let hosts = server.hosts();
    if let Some(t) = templates
        .iter()
        .find(|t| template_host(t).is_some_and(|h| hosts.contains(&h)))
    {
        return (KitImportMatch::Catalogue, Some(t));
    }
    let tokens = server.name_tokens();
    if let Some(t) = templates
        .iter()
        .filter(|t| name_matches(&tokens, &t.id))
        .max_by_key(|t| t.id.len())
    {
        return (KitImportMatch::Catalogue, Some(t));
    }
    if dropped(server) {
        return (KitImportMatch::None, None);
    }
    (KitImportMatch::Custom, None)
}

fn secret_fields(t: &McpMarketTemplate) -> Vec<(&String, bool)> {
    t.env
        .iter()
        .filter(|(_, f)| f.kind == McpMarketEnvKind::Secret)
        .map(|(k, f)| (k, f.required))
        .collect()
}

/// `import_aliases` of the raw catalogue entries: template id → field → old names.
type Aliases = HashMap<String, HashMap<String, Vec<String>>>;

fn aliases_of(entries: &[serde_json::Value]) -> Aliases {
    let mut out: Aliases = HashMap::new();
    for e in entries {
        let (Some(id), Some(map)) = (
            e.get("id").and_then(|v| v.as_str()),
            e.get("import_aliases").and_then(|v| v.as_object()),
        ) else {
            continue;
        };
        let fields = out.entry(id.to_owned()).or_default();
        for (field, names) in map {
            if let Some(names) = names.as_array() {
                fields
                    .entry(field.clone())
                    .or_default()
                    .extend(names.iter().filter_map(|n| n.as_str().map(str::to_owned)));
            }
        }
    }
    out
}

fn field_aliases<'a>(aliases: &'a Aliases, template: &str, field: &str) -> &'a [String] {
    aliases
        .get(template)
        .and_then(|f| f.get(field))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Stable code of a market refusal for the per-server result.
fn market_code(err: &MarketError) -> &'static str {
    match err {
        MarketError::Invalid(_) => "MCP_MARKET_INVALID",
        MarketError::TemplateSecretMissing(_) => "MCP_TEMPLATE_SECRET_MISSING",
        MarketError::NotFound(_) => "MCP_MARKET_NOT_FOUND",
        MarketError::Conflict(_) => "MCP_MARKET_CONFLICT",
        MarketError::Forbidden(_) => "MCP_MARKET_FORBIDDEN",
        MarketError::Unavailable(_) => "MCP_MARKET_UNAVAILABLE",
        MarketError::Internal(_) => "MCP_MARKET_INTERNAL",
        MarketError::Kit(code, _) => code.as_str(),
    }
}

fn failed(name: &str, code: &str, message: impl Into<String>) -> KitImportServerResult {
    KitImportServerResult {
        name: name.to_owned(),
        instance_id: None,
        error_code: Some(code.to_owned()),
        message: Some(message.into()),
        missing_secrets: Vec::new(),
    }
}

impl McpMarketService {
    /// The project's `<workspace>/.mcp.json` servers, or `None` when the
    /// project has no workspace or no readable/valid file. 404 for an
    /// unknown project.
    async fn project_file(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<(String, Option<Vec<FileServer>>), MarketError> {
        let info = self
            .kits()?
            .project_info(user_id, project_id)
            .await?
            .ok_or_else(|| kit_error(KitErrorCode::UnknownTarget, format!("project {project_id} not found")))?;
        let Some(dir) = info
            .workspace_uri
            .as_deref()
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| u.to_file_path().ok())
        else {
            return Ok((info.name, None));
        };
        let path = dir.join(".mcp.json");
        let servers = match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE => match tokio::fs::read(&path).await {
                Ok(bytes) => parse_mcp_json(&bytes),
                Err(_) => None,
            },
            _ => None,
        };
        Ok((info.name, servers))
    }

    async fn templates_and_aliases(&self, user_id: &str) -> Result<(Vec<McpMarketTemplate>, Aliases), MarketError> {
        let templates = load_templates(self.catalog.as_ref(), user_id).await?;
        let aliases = match self.catalog.entries(user_id).await {
            Ok(entries) => aliases_of(&entries),
            Err(e) => {
                warn!(error = %e, "kits import: catalogue aliases unavailable");
                Aliases::new()
            }
        };
        Ok((templates, aliases))
    }

    /// `GET /api/projects/{project_id}/kit/import-preview`.
    pub async fn kit_import_preview(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<KitImportPreviewResponse, MarketError> {
        let (_, servers) = self.project_file(user_id, project_id).await?;
        let Some(servers) = servers else {
            return Ok(KitImportPreviewResponse {
                project_id: project_id.to_owned(),
                file_found: false,
                servers: Vec::new(),
            });
        };
        let (templates, aliases) = self.templates_and_aliases(user_id).await?;
        let mut out: Vec<KitImportPreviewServer> = servers
            .iter()
            .map(|server| {
                let (match_kind, template) = match_server(server, &templates);
                let secrets: Vec<KitImportSecret> = template
                    .map(|t| {
                        secret_fields(t)
                            .into_iter()
                            .map(|(field, _)| KitImportSecret {
                                field: field.clone(),
                                found: secret_value(server, t, field, field_aliases(&aliases, &t.id, field)).is_some(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                KitImportPreviewServer {
                    name: server.name.clone(),
                    template_id: template.map(|t| t.id.clone()),
                    match_kind,
                    oauth_only: server.is_remote() && !server.has_any_credential(),
                    secrets,
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(KitImportPreviewResponse {
            project_id: project_id.to_owned(),
            file_found: true,
            servers: out,
        })
    }

    /// `POST /api/projects/{project_id}/kit/import`: per server, a dormant
    /// market instance from the catalogue template (market create rules) with
    /// the found secrets copied server-side; all created instances are then
    /// added to the project kit (created when missing) in one transaction.
    /// A server whose secrets cannot be stored is rolled back (instance deleted).
    pub async fn kit_import(
        &self,
        user_id: &str,
        project_id: &str,
        req: KitImportRequest,
    ) -> Result<KitImportResponse, MarketError> {
        let (project_name, servers) = self.project_file(user_id, project_id).await?;
        let servers = servers.unwrap_or_default();
        let (templates, aliases) = self.templates_and_aliases(user_id).await?;
        let kits = self.kits()?;
        let kit_id = match kits.find_project_kit(user_id, project_id).await? {
            Some(k) => k.id,
            None => {
                self.create_kit(
                    user_id,
                    CreateKitRequest {
                        name: format!("{project_name} project kit"),
                        project_id: Some(project_id.to_owned()),
                        ..Default::default()
                    },
                )
                .await?
                .id
            }
        };

        let mut results = Vec::with_capacity(req.servers.len());
        let mut created: Vec<String> = Vec::new();
        for want in &req.servers {
            let Some(server) = servers.iter().find(|s| s.name == want.name) else {
                results.push(failed(
                    &want.name,
                    "KIT_IMPORT_NOT_IN_FILE",
                    format!("\"{}\" is not a server of the project's .mcp.json", want.name),
                ));
                continue;
            };
            let Some(t) = templates.iter().find(|t| t.id == want.template_id) else {
                results.push(failed(
                    &want.name,
                    "KIT_IMPORT_UNKNOWN_TEMPLATE",
                    format!("template {} is not in the AionUi catalogue", want.template_id),
                ));
                continue;
            };
            // A read instance needs an allowlist (R6): the template's pinned
            // read tools, else its tools classified `read`.
            let tools_allow = (want.tier == "read").then(|| {
                if t.read_tools.is_empty() {
                    t.tool_classes
                        .iter()
                        .filter(|(_, class)| class.as_str() == "read")
                        .map(|(tool, _)| tool.clone())
                        .collect::<Vec<_>>()
                } else {
                    t.read_tools.clone()
                }
            });
            let tools_allow = tools_allow.filter(|a| !a.is_empty());
            let inst = match self
                .create_instance(
                    user_id,
                    CreateMcpMarketInstanceRequest {
                        name: want.instance_name.clone(),
                        template_id: t.id.clone(),
                        description: Some(format!("Imported keys from {project_name}/.mcp.json ({})", want.name)),
                        environment: want.environment.clone(),
                        tier: want.tier.clone(),
                        status: None,
                        clis: t.clis.clone(),
                        plain_env: BTreeMap::new(),
                        tools_allow,
                        tools_block: Vec::new(),
                    },
                )
                .await
            {
                Ok(inst) => inst,
                Err(e) => {
                    results.push(failed(&want.name, market_code(&e), e.to_string()));
                    continue;
                }
            };
            let mut missing = Vec::new();
            let mut stored = Ok(());
            for (field, required) in secret_fields(t) {
                match secret_value(server, t, field, field_aliases(&aliases, &t.id, field)) {
                    Some(value) => {
                        if let Err(e) = self.set_secret(user_id, &inst.id, field, &value).await {
                            stored = Err(e);
                            break;
                        }
                    }
                    None if required => missing.push(field.clone()),
                    None => {}
                }
            }
            if let Err(e) = stored {
                if let Err(undo) = self.delete_instance(user_id, &inst.id).await {
                    warn!(instance = %inst.name, error = %undo, "kits import: could not undo an instance after a secret failure");
                }
                results.push(failed(&want.name, market_code(&e), e.to_string()));
                continue;
            }
            created.push(inst.id.clone());
            results.push(KitImportServerResult {
                name: want.name.clone(),
                instance_id: Some(inst.id),
                error_code: None,
                message: None,
                missing_secrets: missing,
            });
        }
        if !created.is_empty() {
            kits.add_kit_items(user_id, &kit_id, "mcp", &created).await?;
        }
        info!(
            project = %project_id,
            requested = req.servers.len(),
            created = created.len(),
            "kits import: keys imported from the project's .mcp.json"
        );
        Ok(KitImportResponse {
            kit_id,
            servers: results,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn templates() -> Vec<McpMarketTemplate> {
        super::super::template::builtin_entries()
            .iter()
            .filter_map(|e| super::super::template::parse_template(e).ok())
            .collect()
    }

    const FILE: &str = r#"{"mcpServers":{
        "supabase-tb": {"command":"npx","args":["-y","mcp-remote@0.1.0","https://mcp.supabase.com/mcp?project_ref=abc","--header","Authorization: Bearer sbp_FILETOKEN123"]},
        "ctx": {"command":"npx","args":["-y","@upstash/context7-mcp@1.0.0"],"env":{"CONTEXT7_API_KEY":"${CTX}"}},
        "gh": {"type":"http","url":"https://api.githubcopilot.com/mcp/","headers":{"Authorization":"Bearer ghp_FILEPAT"}},
        "tb-paddle-sandbox": {"command":"node","args":["old.js"],"env":{"PADDLE_API_KEY":"pdl_sdbx_FILE"}},
        "sentry": {"type":"http","url":"https://mcp.sentry.dev/mcp"},
        "tavily": {"command":"npx","args":["-y","tavily-mcp@0.1.0"]},
        "tb-protocol": {"command":"node","args":["/srv/tb/protocol.js"]}
    }}"#;

    #[test]
    fn matches_by_package_host_name_and_classifies_the_rest() {
        let ts = templates();
        let servers = parse_mcp_json(FILE.as_bytes()).unwrap();
        let get = |n: &str| servers.iter().find(|s| s.name == n).unwrap();
        let id = |n: &str| match_server(get(n), &ts).1.map(|t| t.id.clone());
        assert_eq!(
            id("supabase-tb").as_deref(),
            Some("supabase"),
            "URL host in mcp-remote args"
        );
        assert_eq!(id("ctx").as_deref(), Some("context7"), "package without version");
        assert_eq!(id("gh").as_deref(), Some("github"), "url host");
        assert_eq!(
            id("tb-paddle-sandbox").as_deref(),
            Some("paddle-sandbox"),
            "longest id in name"
        );
        assert_eq!(id("sentry").as_deref(), Some("sentry"), "by name");
        assert_eq!(match_server(get("tavily"), &ts).0, KitImportMatch::None);
        assert_eq!(match_server(get("tb-protocol"), &ts).0, KitImportMatch::Custom);
    }

    #[test]
    fn secrets_come_from_env_aliases_and_headers_without_bearer() {
        let ts = templates();
        let servers = parse_mcp_json(FILE.as_bytes()).unwrap();
        let get = |n: &str| servers.iter().find(|s| s.name == n).unwrap();
        let t = |id: &str| ts.iter().find(|t| t.id == id).unwrap();
        assert_eq!(
            secret_value(get("supabase-tb"), t("supabase"), "SUPABASE_ACCESS_TOKEN", &[]).as_deref(),
            Some("sbp_FILETOKEN123")
        );
        assert_eq!(
            secret_value(get("gh"), t("github"), "GITHUB_PAT", &[]).as_deref(),
            Some("ghp_FILEPAT")
        );
        // An unresolved ${VAR} is not a credential.
        assert!(secret_value(get("ctx"), t("context7"), "CONTEXT7_API_KEY", &[]).is_none());
        // Alias: the file used an older env name.
        let aliased = parse_mcp_json(
            br#"{"mcpServers":{"gh":{"command":"x","env":{"GITHUB_PERSONAL_ACCESS_TOKEN":"ghp_OLD"}}}}"#,
        )
        .unwrap();
        assert!(secret_value(&aliased[0], t("github"), "GITHUB_PAT", &[]).is_none());
        assert_eq!(
            secret_value(
                &aliased[0],
                t("github"),
                "GITHUB_PAT",
                &["GITHUB_PERSONAL_ACCESS_TOKEN".to_owned()]
            )
            .as_deref(),
            Some("ghp_OLD")
        );
        // OAuth-only remote: no credential anywhere in the entry.
        assert!(get("sentry").is_remote() && !get("sentry").has_any_credential());
    }

    #[test]
    fn aliases_are_read_from_the_raw_catalogue_entries() {
        let entries = vec![
            serde_json::json!({"id": "github", "import_aliases": {"GITHUB_PAT": ["GITHUB_PERSONAL_ACCESS_TOKEN"]}}),
        ];
        let a = aliases_of(&entries);
        assert_eq!(
            field_aliases(&a, "github", "GITHUB_PAT"),
            ["GITHUB_PERSONAL_ACCESS_TOKEN".to_owned()]
        );
        assert!(field_aliases(&a, "github", "OTHER").is_empty());
        // The built-in catalogue carries aliases and they do not change the parsed template.
        assert!(!aliases_of(&super::super::template::builtin_entries()).is_empty());
    }

    #[test]
    fn malformed_file_is_none() {
        assert!(parse_mcp_json(b"{not json").is_none());
        assert!(parse_mcp_json(b"{\"other\":{}}").is_none());
    }
}
