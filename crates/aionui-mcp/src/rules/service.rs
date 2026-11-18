//! Rules service (C37): owner CRUD with caps, the effective view, the
//! confirmation-gated import, and the files AionUi writes once the owner
//! hands them over. AionUi only writes files; it never commits or pushes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    CreateRuleRequest, RULE_BODY_MAX_BYTES, RULES_EFFECTIVE_MAX_BYTES, RuleEffectiveItem, RuleEffectiveQuery,
    RuleEffectiveResponse, RuleImportFile, RuleImportPreviewResponse, RuleImportProposal, RuleImportQuery,
    RuleImportRequest, RuleImportResponse, RuleManageFilesRequest, RuleManageFilesResponse, RuleResponse,
    RuleSourceResponse, UpdateRuleRequest,
};
use aionui_db::models::{RuleRow, RuleSourceRow};
use aionui_db::{
    CreateRuleParams, DbError, EffectiveRule, IKitRepository, IRuleRepository, ImportRulesParams, ImportedRule,
    KitScope, RULE_NAME_TAKEN_MSG, RULE_SOURCE_EXISTS_MSG, UpdateRuleParams,
};
use tracing::{info, warn};

use super::compose::compose_rules;
use super::import::{
    IMPORT_FILE_MAX_BYTES, POINTER_LINE, PROJECT_RULE_FILES, is_pointer, render_global_file, split_into_rules,
};

const NAME_MAX: usize = 120;
const DESCRIPTION_MAX: usize = 300;

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NameTaken(String),
    #[error("{0}")]
    Stale(String),
    #[error("{0}")]
    AlreadyImported(String),
    #[error("{0}")]
    ConfirmRequired(String),
    #[error("{0}")]
    NotFound(String),
    /// A file is not what AionUi last saw (or is a symlink): nothing was written.
    #[error("{0}")]
    FileChanged(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<DbError> for RuleError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound(msg) => Self::NotFound(msg),
            DbError::Conflict(msg) if msg == RULE_NAME_TAKEN_MSG => Self::NameTaken(msg),
            DbError::Conflict(msg) if msg == RULE_SOURCE_EXISTS_MSG => Self::AlreadyImported(msg),
            DbError::Conflict(msg) => Self::Stale(msg),
            other => {
                tracing::error!(error = %other, "rules: database error");
                Self::Internal("database error".to_owned())
            }
        }
    }
}

fn invalid(msg: impl Into<String>) -> RuleError {
    RuleError::Invalid(msg.into())
}

#[derive(Clone)]
pub struct RuleService {
    repo: Arc<dyn IRuleRepository>,
    kits: Arc<dyn IKitRepository>,
    /// The container-level rules file (e.g. `~/.clauterm/agents/AGENTS.md`);
    /// `None` = no global file on this machine.
    global_file: Option<PathBuf>,
    /// Where the previous content of a file is copied before AionUi rewrites
    /// it (next to the database, never inside a repository). Without it no
    /// file is ever rewritten.
    backup_dir: Option<PathBuf>,
}

impl RuleService {
    pub fn new(repo: Arc<dyn IRuleRepository>, kits: Arc<dyn IKitRepository>, global_file: Option<PathBuf>) -> Self {
        Self {
            repo,
            kits,
            global_file,
            backup_dir: None,
        }
    }

    pub fn with_backup_dir(mut self, dir: PathBuf) -> Self {
        self.backup_dir = Some(dir);
        self
    }

    pub fn repo(&self) -> &Arc<dyn IRuleRepository> {
        &self.repo
    }

    // -- CRUD ---------------------------------------------------------------

    pub async fn list(&self, user_id: &str) -> Result<Vec<RuleResponse>, RuleError> {
        let sources = self.repo.list_rule_sources(user_id).await?;
        Ok(self
            .repo
            .list_rules(user_id)
            .await?
            .iter()
            .map(|r| rule_response(r, &sources))
            .collect())
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<RuleResponse, RuleError> {
        let sources = self.repo.list_rule_sources(user_id).await?;
        self.repo
            .find_rule(user_id, id)
            .await?
            .map(|r| rule_response(&r, &sources))
            .ok_or_else(|| RuleError::NotFound(format!("rule {id} not found")))
    }

    pub async fn create(&self, user_id: &str, req: CreateRuleRequest) -> Result<RuleResponse, RuleError> {
        let name = clean_name(&req.name)?;
        let description = clean_description(req.description.as_deref().unwrap_or(""))?;
        let body = clean_body(&req.body)?;
        let row = self
            .repo
            .create_rule(&CreateRuleParams {
                user_id,
                name: &name,
                description: &description,
                body: &body,
                enabled: req.enabled.unwrap_or(true),
            })
            .await?;
        info!(rule = %row.id, "rules: created");
        Ok(rule_response(&row, &[]))
    }

    pub async fn update(&self, user_id: &str, id: &str, req: UpdateRuleRequest) -> Result<RuleResponse, RuleError> {
        let name = req.name.as_deref().map(clean_name).transpose()?;
        let description = req.description.as_deref().map(clean_description).transpose()?;
        let body = req.body.as_deref().map(clean_body).transpose()?;
        let row = self
            .repo
            .update_rule(
                user_id,
                id,
                &UpdateRuleParams {
                    name: name.as_deref(),
                    description: description.as_deref(),
                    body: body.as_deref(),
                    enabled: req.enabled,
                    expected_updated_at: req.expected_updated_at,
                },
            )
            .await?;
        info!(rule = %row.id, "rules: updated");
        self.sync_global_file(user_id).await;
        let sources = self.repo.list_rule_sources(user_id).await?;
        Ok(rule_response(&row, &sources))
    }

    pub async fn delete(&self, user_id: &str, id: &str) -> Result<(), RuleError> {
        if !self.repo.delete_rule(user_id, id).await? {
            return Err(RuleError::NotFound(format!("rule {id} not found")));
        }
        info!(rule = %id, "rules: deleted");
        self.sync_global_file(user_id).await;
        Ok(())
    }

    // -- effective ----------------------------------------------------------

    pub async fn effective(
        &self,
        user_id: &str,
        query: RuleEffectiveQuery,
    ) -> Result<RuleEffectiveResponse, RuleError> {
        let non_empty = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        let rules = match non_empty(query.conversation_id) {
            Some(cid) => {
                if self.kits.conversation_scope(user_id, &cid).await?.is_none() {
                    return Err(RuleError::NotFound(format!("conversation {cid} not found")));
                }
                self.repo.conversation_rules(user_id, &cid).await?
            }
            None => {
                let scope = KitScope {
                    project_id: non_empty(query.project_id),
                    assistant_id: non_empty(query.assistant_id),
                    conversation_id: None,
                };
                self.repo.effective_rules(user_id, &scope).await?
            }
        };
        Ok(effective_response(&rules))
    }

    /// The rules block of a conversation's next session (empty = none).
    /// Best-effort for the session start: an error is logged and yields no block.
    pub async fn conversation_block(&self, user_id: &str, conversation_id: &str) -> String {
        match self.repo.conversation_rules(user_id, conversation_id).await {
            Ok(rules) => {
                let composed = compose_rules(&rules);
                if !composed.omitted.is_empty() {
                    warn!(
                        conversation_id,
                        omitted = composed.omitted.len(),
                        "rules: effective rules exceed the cap; some were left out"
                    );
                }
                composed.block
            }
            Err(e) => {
                warn!(conversation_id, error = %e, "rules: cannot load the conversation's rules");
                String::new()
            }
        }
    }

    /// Refuse an attach that would push a scope over the 24 KB cap. Checked
    /// for the scope the attachment names (global alone, global + project, …);
    /// combinations across levels are caught at compose time (named, not cut).
    pub async fn check_attach_cap(&self, user_id: &str, scope: &KitScope, rule_id: &str) -> Result<(), RuleError> {
        let rule = self
            .repo
            .find_rule(user_id, rule_id)
            .await?
            .ok_or_else(|| invalid(format!("rule {rule_id} does not exist")))?;
        let current = self.repo.effective_rules(user_id, scope).await?;
        if current.iter().any(|r| r.rule.id == rule.id) {
            return Ok(());
        }
        let total: usize = current.iter().map(|r| r.rule.body.trim().len()).sum::<usize>() + rule.body.trim().len();
        if total > RULES_EFFECTIVE_MAX_BYTES {
            return Err(invalid(format!(
                "attaching \"{}\" would bring this scope's rules to {total} bytes; the limit is {RULES_EFFECTIVE_MAX_BYTES}",
                rule.name
            )));
        }
        Ok(())
    }

    // -- import -------------------------------------------------------------

    async fn scope_files(
        &self,
        user_id: &str,
        scope: &str,
        project_id: Option<&str>,
    ) -> Result<(String, String, Vec<PathBuf>), RuleError> {
        match scope {
            "global" => Ok((
                "Global".to_owned(),
                String::new(),
                self.global_file.iter().cloned().collect(),
            )),
            "project" => {
                let pid = project_id
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| invalid("project_id is required for scope project"))?;
                let info = self
                    .kits
                    .project_info(user_id, pid)
                    .await?
                    .ok_or_else(|| RuleError::NotFound(format!("project {pid} not found")))?;
                let files = info
                    .workspace_uri
                    .as_deref()
                    .and_then(|u| url::Url::parse(u).ok())
                    .and_then(|u| u.to_file_path().ok())
                    .map(|dir| PROJECT_RULE_FILES.iter().map(|f| dir.join(f)).collect())
                    .unwrap_or_default();
                Ok((info.name, pid.to_owned(), files))
            }
            other => Err(invalid(format!("scope must be global or project, got \"{other}\""))),
        }
    }

    async fn preview_files(
        &self,
        user_id: &str,
        scope: &str,
        project_id: Option<&str>,
    ) -> Result<(String, Vec<(RuleImportFile, String)>), RuleError> {
        let (prefix, pid, paths) = self.scope_files(user_id, scope, project_id).await?;
        let sources = self.repo.list_rule_sources(user_id).await?;
        let taken: Vec<String> = self
            .repo
            .list_rules(user_id)
            .await?
            .into_iter()
            .map(|r| r.name)
            .collect();
        let mut files = Vec::new();
        let mut seen_contents: Vec<String> = Vec::new();
        for path in paths {
            let path_str = path.to_string_lossy().into_owned();
            let known = sources.iter().find(|s| s.path == path_str);
            let content = match read_rule_file(&path).await {
                FileRead::Content(c) => Some(c),
                FileRead::Missing => None,
                FileRead::Refused => {
                    // A symlink out of the project, a non-file or an oversized file.
                    files.push((
                        RuleImportFile {
                            path: path_str,
                            status: "refused".to_owned(),
                            bytes: 0,
                            rules: Vec::new(),
                        },
                        String::new(),
                    ));
                    continue;
                }
            };
            let (status, rules, bytes, original) = match (known, content) {
                (Some(s), c) => (
                    s.status.clone(),
                    Vec::new(),
                    c.as_deref().map_or(0, str::len),
                    String::new(),
                ),
                (None, None) => ("missing".to_owned(), Vec::new(), 0, String::new()),
                (None, Some(c)) if is_pointer(&c) || seen_contents.contains(&c) => {
                    ("pointer".to_owned(), Vec::new(), c.len(), String::new())
                }
                (None, Some(c)) => {
                    seen_contents.push(c.clone());
                    // Later files of the same project keep distinct names.
                    let file_prefix = if files.iter().any(|(f, _): &(RuleImportFile, String)| f.status == "new") {
                        format!(
                            "{prefix} ({})",
                            path.file_name()
                                .map(|f| f.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        )
                    } else {
                        prefix.clone()
                    };
                    let rules = split_into_rules(&file_prefix, &c)
                        .into_iter()
                        .map(|r| RuleImportProposal {
                            name_taken: taken.contains(&r.name),
                            description: format!("Imported from {path_str}"),
                            bytes: r.body.len(),
                            name: r.name,
                            body: r.body,
                        })
                        .collect();
                    ("new".to_owned(), rules, c.len(), c)
                }
            };
            files.push((
                RuleImportFile {
                    path: path_str,
                    status,
                    bytes,
                    rules,
                },
                original,
            ));
        }
        Ok((pid, files))
    }

    /// What an import of this scope would create. Writes nothing.
    pub async fn import_preview(
        &self,
        user_id: &str,
        query: RuleImportQuery,
    ) -> Result<RuleImportPreviewResponse, RuleError> {
        let (pid, files) = self
            .preview_files(user_id, &query.scope, query.project_id.as_deref())
            .await?;
        Ok(RuleImportPreviewResponse {
            scope: query.scope,
            project_id: (!pid.is_empty()).then_some(pid),
            files: files.into_iter().map(|(f, _)| f).collect(),
        })
    }

    /// Import every `new` file of the scope (one transaction per file). The
    /// files on disk are not touched.
    pub async fn import(&self, user_id: &str, req: RuleImportRequest) -> Result<RuleImportResponse, RuleError> {
        if !req.confirm {
            return Err(RuleError::ConfirmRequired(
                "an import needs confirm: true after the owner saw the preview".to_owned(),
            ));
        }
        let (pid, files) = self
            .preview_files(user_id, &req.scope, req.project_id.as_deref())
            .await?;
        let new: Vec<&(RuleImportFile, String)> = files.iter().filter(|(f, _)| f.status == "new").collect();
        if new.is_empty() {
            return Err(invalid("nothing to import for this scope"));
        }
        if let Some(taken) = new.iter().flat_map(|(f, _)| f.rules.iter()).find(|r| r.name_taken) {
            return Err(RuleError::NameTaken(format!(
                "a rule named \"{}\" already exists; rename or delete it first",
                taken.name
            )));
        }
        // Imported rules are attached at the scope: the scope's 24 KB cap holds
        // for an import as for a manual attach, and the 8 KB cap per rule.
        let scope = KitScope {
            project_id: (!pid.is_empty()).then(|| pid.clone()),
            ..Default::default()
        };
        let existing: usize = self
            .repo
            .effective_rules(user_id, &scope)
            .await?
            .iter()
            .map(|r| r.rule.body.trim().len())
            .sum();
        let incoming: usize = new.iter().flat_map(|(f, _)| f.rules.iter()).map(|r| r.bytes).sum();
        if let Some(big) = new
            .iter()
            .flat_map(|(f, _)| f.rules.iter())
            .find(|r| r.bytes > RULE_BODY_MAX_BYTES)
        {
            return Err(invalid(format!(
                "\"{}\" is {} bytes; a rule holds at most {RULE_BODY_MAX_BYTES}",
                big.name, big.bytes
            )));
        }
        if existing + incoming > RULES_EFFECTIVE_MAX_BYTES {
            return Err(invalid(format!(
                "this import would bring the scope's rules to {} bytes ({existing} already attached + {incoming} \
                 imported); the limit is {RULES_EFFECTIVE_MAX_BYTES}. Shorten the file or move long material into a skill",
                existing + incoming
            )));
        }
        let mut out = RuleImportResponse {
            sources: Vec::new(),
            rules: Vec::new(),
        };
        for (file, original) in new {
            let rules: Vec<ImportedRule<'_>> = file
                .rules
                .iter()
                .map(|r| ImportedRule {
                    name: &r.name,
                    description: &r.description,
                    body: &r.body,
                })
                .collect();
            let (source, rows) = self
                .repo
                .import_rules(&ImportRulesParams {
                    user_id,
                    scope: &req.scope,
                    project_id: &pid,
                    path: &file.path,
                    original,
                    rules: &rules,
                })
                .await?;
            info!(scope = %req.scope, rules = rows.len(), "rules: imported a file");
            out.rules
                .extend(rows.iter().map(|r| rule_response(r, std::slice::from_ref(&source))));
            out.sources.push(source_response(&source));
        }
        Ok(out)
    }

    pub async fn sources(&self, user_id: &str) -> Result<Vec<RuleSourceResponse>, RuleError> {
        Ok(self
            .repo
            .list_rule_sources(user_id)
            .await?
            .iter()
            .map(source_response)
            .collect())
    }

    // -- files AionUi writes -------------------------------------------------

    /// Hand the imported files of a scope over to AionUi (owner-confirmed per
    /// scope). Only files that were imported are rewritten: their original
    /// content is in `rule_sources.original`.
    pub async fn manage_files(
        &self,
        user_id: &str,
        req: RuleManageFilesRequest,
    ) -> Result<RuleManageFilesResponse, RuleError> {
        if !req.confirm {
            return Err(RuleError::ConfirmRequired(
                "rewriting files needs confirm: true from the owner for this scope".to_owned(),
            ));
        }
        let (_, pid, _) = self.scope_files(user_id, &req.scope, req.project_id.as_deref()).await?;
        let sources: Vec<RuleSourceRow> = self
            .repo
            .list_rule_sources(user_id)
            .await?
            .into_iter()
            .filter(|s| s.scope == req.scope && s.project_id == pid)
            .collect();
        if sources.is_empty() {
            return Err(invalid(
                "import this scope's files first; only imported files are rewritten",
            ));
        }
        let backup_dir = self
            .backup_dir
            .as_deref()
            .ok_or_else(|| RuleError::Internal("no backup directory is configured; files are not rewritten".into()))?;
        // Check every file before writing any: all or nothing.
        let mut plan = Vec::new();
        for source in sources {
            let text = if source.scope == "global" {
                self.global_file_text(user_id).await?
            } else {
                format!("{POINTER_LINE}\n")
            };
            let path = PathBuf::from(&source.path);
            let current = read_for_write(&path).await?;
            // Drift: the file must still be what the owner saw — the imported
            // content, or (already handed over) what AionUi wrote.
            let expected_ours = source.status == "managed" && is_pointer(&current);
            if !expected_ours && current != source.original {
                return Err(RuleError::FileChanged(format!(
                    "{} changed since it was imported; nothing was written. Review the difference, then re-import \
                     or restore the file:\n{}",
                    source.path,
                    line_diff(&source.original, &current)
                )));
            }
            plan.push((source, path, current, text));
        }
        let mut written = Vec::new();
        for (source, path, current, text) in plan {
            if current != text {
                backup_file(backup_dir, &path, &current).await?;
                write_file(&path, &text).await?;
            }
            let row = if source.status == "managed" {
                source
            } else {
                self.repo.mark_rule_source_managed(user_id, &source.id).await?
            };
            info!(scope = %row.scope, "rules: file handed over to AionUi");
            written.push(source_response(&row));
        }
        Ok(RuleManageFilesResponse { written })
    }

    async fn global_file_text(&self, user_id: &str) -> Result<String, RuleError> {
        let rules = self.repo.effective_rules(user_id, &KitScope::default()).await?;
        let pairs: Vec<(String, String)> = rules.into_iter().map(|r| (r.rule.name, r.rule.body)).collect();
        Ok(render_global_file(&pairs))
    }

    /// Regenerate the global file after a change, once the owner handed it
    /// over (`managed`). Best-effort: a failure is logged, the change stands.
    pub async fn sync_global_file(&self, user_id: &str) {
        let managed = match self.repo.list_rule_sources(user_id).await {
            Ok(sources) => sources
                .into_iter()
                .filter(|s| s.scope == "global" && s.status == "managed")
                .collect::<Vec<_>>(),
            Err(e) => {
                warn!(error = %e, "rules: cannot read sources for the global file");
                return;
            }
        };
        if managed.is_empty() {
            return;
        }
        let text = match self.global_file_text(user_id).await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "rules: cannot render the global file");
                return;
            }
        };
        let Some(backup_dir) = self.backup_dir.as_deref() else {
            return;
        };
        for source in managed {
            let path = PathBuf::from(&source.path);
            // Only a file that is still AionUi's generated one is regenerated;
            // a hand-edited or replaced file is left alone (drift).
            let current = match read_for_write(&path).await {
                Ok(c) if is_pointer(&c) => c,
                Ok(_) => {
                    warn!("rules: the global file was edited by hand; not regenerating it");
                    continue;
                }
                Err(e) => {
                    warn!(error = %e, "rules: the global file cannot be regenerated");
                    continue;
                }
            };
            if current == text {
                continue;
            }
            let result = async {
                backup_file(backup_dir, &path, &current).await?;
                write_file(&path, &text).await
            }
            .await;
            if let Err(e) = result {
                warn!(error = %e, "rules: cannot write the generated global file");
            }
        }
    }
}

enum FileRead {
    Content(String),
    Missing,
    /// Not read: a symlink leaving its directory, not a regular file, or too large.
    Refused,
}

/// Read a rules file for an import. A symlink is followed only when its
/// target stays inside the file's own directory tree (the project root).
async fn read_rule_file(path: &Path) -> FileRead {
    let Ok(meta) = tokio::fs::symlink_metadata(path).await else {
        return FileRead::Missing;
    };
    if meta.file_type().is_symlink() {
        let root = match path.parent() {
            Some(p) => tokio::fs::canonicalize(p).await.ok(),
            None => None,
        };
        let target = tokio::fs::canonicalize(path).await.ok();
        match (root, target) {
            (Some(root), Some(target)) if target.starts_with(&root) => {}
            _ => return FileRead::Refused,
        }
    }
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() && meta.len() <= IMPORT_FILE_MAX_BYTES => {
            match tokio::fs::read_to_string(path).await {
                Ok(c) => FileRead::Content(c),
                Err(_) => FileRead::Refused,
            }
        }
        Ok(_) => FileRead::Refused,
        Err(_) => FileRead::Missing,
    }
}

/// The current content of a file AionUi is about to rewrite. Refuses a
/// symlink (the file or its directory) and anything that is not a regular file.
async fn read_for_write(path: &Path) -> Result<String, RuleError> {
    let refuse = |why: &str| RuleError::FileChanged(format!("{} {why}; nothing was written", path.display()));
    let parent = path.parent().ok_or_else(|| refuse("has no directory"))?;
    let parent_meta = tokio::fs::symlink_metadata(parent)
        .await
        .map_err(|_| refuse("is in a directory that no longer exists"))?;
    if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
        return Err(refuse("is in a symlinked directory"));
    }
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| refuse("no longer exists"))?;
    if meta.file_type().is_symlink() {
        return Err(refuse("is a symlink"));
    }
    if !meta.is_file() || meta.len() > IMPORT_FILE_MAX_BYTES {
        return Err(refuse("is not a regular rules file"));
    }
    tokio::fs::read_to_string(path)
        .await
        .map_err(|_| refuse("cannot be read"))
}

/// Copy `content` to `<backup_dir>/<epoch ms>-<path with / as _>` (0600).
async fn backup_file(backup_dir: &Path, path: &Path, content: &str) -> Result<(), RuleError> {
    let name: String = path
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let target = backup_dir.join(format!("{}-{name}", aionui_common::now_ms()));
    let result = async {
        tokio::fs::create_dir_all(backup_dir).await?;
        write_new(&target, content).await
    }
    .await;
    result.map_err(|e| {
        tracing::error!(error = %e, "rules: backup failed; the file is not rewritten");
        RuleError::Internal("cannot back up the rules file".to_owned())
    })
}

/// Create `path` exclusively (`O_CREAT | O_EXCL`: never through an existing
/// file or symlink) with mode 0600 and write `text`.
async fn write_new(path: &Path, text: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(path).await?;
    file.write_all(text.as_bytes()).await?;
    file.sync_all().await
}

/// Replace `path` (checked by [`read_for_write`] just before) through an
/// exclusively created sibling temp file + rename, so no write ever goes
/// through a symlink and a reader never sees half a file. The file keeps its
/// previous permissions.
async fn write_file(path: &Path, text: &str) -> Result<(), RuleError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(
        ".{name}.{}.aionui-tmp",
        aionui_common::generate_prefixed_id("w")
    ));
    let result = async {
        let perms = tokio::fs::symlink_metadata(path).await.map(|m| m.permissions());
        write_new(&tmp, text).await?;
        if let Ok(perms) = perms {
            tokio::fs::set_permissions(&tmp, perms).await?;
        }
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result.map_err(|e| {
        tracing::error!(error = %e, "rules: file write failed");
        RuleError::Internal("cannot write the rules file".to_owned())
    })
}

/// Lines only in `old` (`-`) and only in `new` (`+`), in order, at most 40.
fn line_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out: Vec<String> = Vec::new();
    for l in old_lines.iter().filter(|l| !new_lines.contains(l)) {
        out.push(format!("- {l}"));
    }
    for l in new_lines.iter().filter(|l| !old_lines.contains(l)) {
        out.push(format!("+ {l}"));
    }
    if out.is_empty() {
        return "(only whitespace or line order differs)".to_owned();
    }
    let more = out.len().saturating_sub(40);
    out.truncate(40);
    if more > 0 {
        out.push(format!("… {more} more changed lines"));
    }
    out.join("\n")
}

fn effective_response(rules: &[EffectiveRule]) -> RuleEffectiveResponse {
    let composed = compose_rules(rules);
    RuleEffectiveResponse {
        rules: rules
            .iter()
            .map(|r| RuleEffectiveItem {
                id: r.rule.id.clone(),
                name: r.rule.name.clone(),
                level: r.level.to_owned(),
                bytes: r.rule.body.trim().len(),
                delivered: composed.delivered.contains(&r.rule.id),
            })
            .collect(),
        bytes: composed.block.len(),
        block: composed.block,
        max_bytes: RULES_EFFECTIVE_MAX_BYTES,
    }
}

fn rule_response(row: &RuleRow, sources: &[RuleSourceRow]) -> RuleResponse {
    RuleResponse {
        id: row.id.clone(),
        name: row.name.clone(),
        description: row.description.clone(),
        body: row.body.clone(),
        enabled: row.enabled,
        origin: row.origin.clone(),
        source_path: sources
            .iter()
            .find(|s| !row.origin_ref.is_empty() && s.id == row.origin_ref)
            .map(|s| s.path.clone()),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn source_response(row: &RuleSourceRow) -> RuleSourceResponse {
    RuleSourceResponse {
        id: row.id.clone(),
        scope: row.scope.clone(),
        project_id: (!row.project_id.is_empty()).then(|| row.project_id.clone()),
        path: row.path.clone(),
        status: row.status.clone(),
        imported_at: row.imported_at,
        managed_at: row.managed_at,
    }
}

fn clean_name(value: &str) -> Result<String, RuleError> {
    let v = value.trim();
    if v.is_empty() || v.chars().count() > NAME_MAX || v.chars().any(char::is_control) {
        return Err(invalid("name: 1–120 characters, no control characters"));
    }
    Ok(v.to_owned())
}

fn clean_description(value: &str) -> Result<String, RuleError> {
    let v = value.trim();
    if v.chars().count() > DESCRIPTION_MAX {
        return Err(invalid("description: at most 300 characters"));
    }
    Ok(v.to_owned())
}

fn clean_body(value: &str) -> Result<String, RuleError> {
    let v = value.replace("\r\n", "\n");
    let v = v.trim();
    if v.is_empty() {
        return Err(invalid("body: must not be empty"));
    }
    if v.len() > RULE_BODY_MAX_BYTES {
        return Err(invalid(format!(
            "body: {} bytes; a rule holds at most {RULE_BODY_MAX_BYTES} (move long material into a skill)",
            v.len()
        )));
    }
    Ok(v.to_owned())
}
