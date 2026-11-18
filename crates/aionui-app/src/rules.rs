//! Composition of the rules service (card C37): the one place that decides
//! where the container-level rules file lives.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_mcp::rules::RuleService;

/// The global rules file of this machine: `AIONUI_GLOBAL_RULES_FILE` when set,
/// else `~/.clauterm/agents/AGENTS.md` when that directory exists (the file
/// `setup.sh` copies to the CLIs). `None` = no global file here; Global rules
/// still work, there is just nothing to import or generate.
pub(crate) fn global_rules_file() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AIONUI_GLOBAL_RULES_FILE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path)).filter(|p| p.is_absolute());
    }
    let dir = PathBuf::from(std::env::var_os("HOME")?)
        .join(".clauterm")
        .join("agents");
    dir.is_dir().then(|| dir.join("AGENTS.md"))
}

/// `data_dir` holds the backups of files AionUi rewrites (`rule-backups/`).
pub(crate) fn rule_service(database: &aionui_db::Database, data_dir: &std::path::Path) -> RuleService {
    RuleService::new(
        Arc::new(aionui_db::SqliteRuleRepository::new(database.pool().clone())),
        Arc::new(aionui_db::SqliteKitRepository::new(database.pool().clone())),
        global_rules_file(),
    )
    .with_backup_dir(data_dir.join("rule-backups"))
}
