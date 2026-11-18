//! Project page data (card C42, TASK-169): usage analytics, disk usage and
//! the overview tiles. Business logic only; routes are in `insights_routes`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{
    OverviewConversations, OverviewKit, OverviewStorage, ProjectOverviewResponse, ProjectStorageResponse,
    ProjectUsageResponse, StorageChild, StorageDisk, StorageFile, StorageRoot, UsageByConversation, UsageByModel,
    UsageByProject, UsageDay, UsageSummaryResponse, UsageTotals,
};
use aionui_db::{IUsageRepository, UsageDayRow, UsageSums};
use chrono::{Days, NaiveDate, Utc};
use tokio::sync::Mutex;

use crate::memory::MemoryService;
use crate::service::ProjectService;
use crate::storage_scan::{ScannedRoot, scan_root};
use crate::types::ProjectError;

/// How long a storage scan is served from the cache.
pub const STORAGE_CACHE_TTL: Duration = Duration::from_secs(600);
const TOP_CONVERSATIONS: i64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum InsightsError {
    #[error("days must be 7, 30 or 90")]
    InvalidDays,
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error("database error")]
    Db(#[from] aionui_db::DbError),
    #[error("the storage scan could not run")]
    Scan,
}

type ScanCache = HashMap<(String, String), Arc<Mutex<Option<ProjectStorageResponse>>>>;

pub struct ProjectInsights {
    project: Arc<ProjectService>,
    usage: Arc<dyn IUsageRepository>,
    memory: Option<Arc<MemoryService>>,
    /// One slot per (user, project): the slot's lock also serialises scans, so
    /// concurrent requests wait for one walk instead of starting several.
    scans: Mutex<ScanCache>,
}

fn totals(s: &UsageSums) -> UsageTotals {
    UsageTotals {
        input: s.input_tokens,
        output: s.output_tokens,
        cache_read: s.cache_read_tokens,
        cache_write: s.cache_write_tokens,
        thought: s.thought_tokens,
        cost_micro_usd: s.cost_micro_usd,
        turns: s.turns,
    }
}

fn add(into: &mut UsageTotals, t: &UsageTotals) {
    into.input += t.input;
    into.output += t.output;
    into.cache_read += t.cache_read;
    into.cache_write += t.cache_write;
    into.thought += t.thought;
    into.turns += t.turns;
    if let Some(cost) = t.cost_micro_usd {
        into.cost_micro_usd = Some(into.cost_micro_usd.unwrap_or(0) + cost);
    }
}

fn range_days(days: Option<u32>) -> Result<u32, InsightsError> {
    match days.unwrap_or(30) {
        d @ (7 | 30 | 90) => Ok(d),
        _ => Err(InsightsError::InvalidDays),
    }
}

/// Every UTC day of the range ending `today`, oldest first, zero-filled.
fn fill_days(rows: Vec<UsageDayRow>, today: NaiveDate, range: u32) -> (Vec<UsageDay>, UsageTotals) {
    let by_day: HashMap<String, UsageTotals> = rows.into_iter().map(|r| (r.day, totals(&r.sums))).collect();
    let mut sum = UsageTotals::default();
    let days = (0..range)
        .rev()
        .filter_map(|back| today.checked_sub_days(Days::new(u64::from(back))))
        .map(|date| {
            let day = date.format("%Y-%m-%d").to_string();
            let t = by_day.get(&day).copied().unwrap_or_default();
            add(&mut sum, &t);
            UsageDay { day, totals: t }
        })
        .collect();
    (days, sum)
}

fn first_day(today: NaiveDate, range: u32) -> NaiveDate {
    today
        .checked_sub_days(Days::new(u64::from(range.saturating_sub(1))))
        .unwrap_or(today)
}

fn day_start_ms(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0).map_or(0, |t| t.and_utc().timestamp_millis())
}

impl ProjectInsights {
    pub fn new(
        project: Arc<ProjectService>,
        usage: Arc<dyn IUsageRepository>,
        memory: Option<Arc<MemoryService>>,
    ) -> Self {
        Self {
            project,
            usage,
            memory,
            scans: Mutex::new(HashMap::new()),
        }
    }

    // -- usage --------------------------------------------------------------

    pub async fn project_usage(
        &self,
        user_id: &str,
        project_id: &str,
        days: Option<u32>,
    ) -> Result<ProjectUsageResponse, InsightsError> {
        let range = range_days(days)?;
        // 404 for a project that is not the user's, before any usage read.
        self.project.name_and_local_roots(user_id, project_id).await?;
        let today = Utc::now().date_naive();
        let from = first_day(today, range);
        let (from_s, to_s) = (
            from.format("%Y-%m-%d").to_string(),
            today.format("%Y-%m-%d").to_string(),
        );
        let scope = Some(project_id);
        let (days, totals_sum) = fill_days(
            self.usage.usage_by_day(user_id, scope, &from_s, &to_s).await?,
            today,
            range,
        );
        let by_model = self
            .usage
            .usage_by_model(user_id, scope, &from_s, &to_s)
            .await?
            .into_iter()
            .map(|r| UsageByModel {
                model: r.model,
                backend: r.backend,
                totals: totals(&r.sums),
            })
            .collect();
        let by_conversation = self
            .usage
            .usage_by_conversation(user_id, project_id, day_start_ms(from), TOP_CONVERSATIONS)
            .await?
            .into_iter()
            .map(|r| UsageByConversation {
                conversation_id: r.conversation_id,
                name: r.name,
                last_at: r.last_at,
                totals: totals(&r.sums),
            })
            .collect();
        Ok(ProjectUsageResponse {
            since: self.usage.usage_since(user_id, scope).await?,
            range_days: range,
            totals: totals_sum,
            days,
            by_conversation,
            by_model,
        })
    }

    pub async fn usage_summary(&self, user_id: &str, days: Option<u32>) -> Result<UsageSummaryResponse, InsightsError> {
        let range = range_days(days)?;
        let today = Utc::now().date_naive();
        let from = first_day(today, range);
        let (from_s, to_s) = (
            from.format("%Y-%m-%d").to_string(),
            today.format("%Y-%m-%d").to_string(),
        );
        let (days, totals_sum) = fill_days(
            self.usage.usage_by_day(user_id, None, &from_s, &to_s).await?,
            today,
            range,
        );
        let by_project = self
            .usage
            .usage_by_project(user_id, &from_s, &to_s)
            .await?
            .into_iter()
            .map(|r| UsageByProject {
                project_id: (!r.project_id.is_empty()).then_some(r.project_id),
                name: r.name,
                totals: totals(&r.sums),
            })
            .collect();
        let by_model = self
            .usage
            .usage_by_model(user_id, None, &from_s, &to_s)
            .await?
            .into_iter()
            .map(|r| UsageByModel {
                model: r.model,
                backend: r.backend,
                totals: totals(&r.sums),
            })
            .collect();
        Ok(UsageSummaryResponse {
            since: self.usage.usage_since(user_id, None).await?,
            range_days: range,
            totals: totals_sum,
            days,
            by_project,
            by_model,
        })
    }

    // -- storage ------------------------------------------------------------

    /// The roots a scan may walk: the project's workspace and attached
    /// folders, and the workspace's Claude memory dir. Nothing else.
    async fn storage_roots(&self, user_id: &str, project_id: &str) -> Result<Vec<(String, PathBuf)>, InsightsError> {
        let (_, folders) = self.project.name_and_local_roots(user_id, project_id).await?;
        let mut roots: Vec<(String, PathBuf)> = Vec::new();
        for (role, path) in folders {
            let label = if role == "workspace" { "workspace" } else { "attached" };
            if !roots.iter().any(|(_, p)| *p == path) {
                roots.push((label.to_owned(), path));
            }
        }
        if let (Some(memory), Some((_, workspace))) =
            (self.memory.as_ref(), roots.first().filter(|(l, _)| l == "workspace"))
        {
            roots.push(("memory".to_owned(), memory.dir_for(workspace)));
        }
        Ok(roots)
    }

    async fn slot(&self, user_id: &str, project_id: &str) -> Arc<Mutex<Option<ProjectStorageResponse>>> {
        self.scans
            .lock()
            .await
            .entry((user_id.to_owned(), project_id.to_owned()))
            .or_default()
            .clone()
    }

    /// The cached scan when it is younger than [`STORAGE_CACHE_TTL`], else a
    /// new one; `force` always scans. The walk runs in a blocking task.
    pub async fn storage(
        &self,
        user_id: &str,
        project_id: &str,
        force: bool,
    ) -> Result<ProjectStorageResponse, InsightsError> {
        let roots = self.storage_roots(user_id, project_id).await?;
        let slot = self.slot(user_id, project_id).await;
        let mut cached = slot.lock().await;
        let ttl_ms = i64::try_from(STORAGE_CACHE_TTL.as_millis()).unwrap_or(i64::MAX);
        if !force
            && let Some(scan) = cached.as_ref()
            && aionui_common::now_ms() - scan.scanned_at < ttl_ms
        {
            return Ok(scan.clone());
        }
        let scan = tokio::task::spawn_blocking(move || scan_all(roots))
            .await
            .map_err(|_| InsightsError::Scan)?;
        *cached = Some(scan.clone());
        Ok(scan)
    }

    /// The cached scan, if any (never scans).
    async fn cached_storage(&self, user_id: &str, project_id: &str) -> Option<ProjectStorageResponse> {
        let slot = self
            .scans
            .lock()
            .await
            .get(&(user_id.to_owned(), project_id.to_owned()))
            .cloned()?;
        // A scan in progress holds the lock: the overview does not wait for it.
        slot.try_lock().ok().and_then(|cached| cached.clone())
    }

    // -- overview -----------------------------------------------------------

    pub async fn overview(&self, user_id: &str, project_id: &str) -> Result<ProjectOverviewResponse, InsightsError> {
        let (name, folders) = self.project.name_and_local_roots(user_id, project_id).await?;
        let counts = self.usage.project_counts(user_id, project_id).await?;
        let today = Utc::now().date_naive().format("%Y-%m-%d").to_string();
        let tokens_today = self
            .usage
            .usage_by_day(user_id, Some(project_id), &today, &today)
            .await?
            .first()
            .map(|r| totals(&r.sums))
            .unwrap_or_default();
        let memory_files = match (
            self.memory.as_ref(),
            folders.first().filter(|(role, _)| role == "workspace"),
        ) {
            (Some(memory), Some((_, workspace))) => Some(count_memory_files(memory.dir_for(workspace)).await),
            _ => None,
        };
        let storage = self
            .cached_storage(user_id, project_id)
            .await
            .map(|scan| OverviewStorage {
                total_bytes: scan.total_bytes,
                scanned_at: scan.scanned_at,
                disk: scan.disk,
            });
        Ok(ProjectOverviewResponse {
            project_id: project_id.to_owned(),
            name,
            conversations: OverviewConversations {
                active: counts.conversations_active,
                total: counts.conversations_total,
            },
            open_cards: counts.open_cards,
            kit: OverviewKit {
                skills: counts.kit_skills,
                mcps: counts.kit_mcps,
                rules: counts.kit_rules,
                servers: counts.kit_servers,
            },
            memory_files,
            tokens_today,
            storage,
        })
    }
}

/// `*.md` files directly in the memory dir (0 when it does not exist).
async fn count_memory_files(dir: PathBuf) -> i64 {
    tokio::task::spawn_blocking(move || {
        let is_real_dir = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir());
        if !is_real_dir {
            return 0;
        }
        let count = std::fs::read_dir(&dir)
            .map(|read| {
                read.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                    .count()
            })
            .unwrap_or(0);
        i64::try_from(count).unwrap_or(i64::MAX)
    })
    .await
    .unwrap_or(0)
}

fn root_response(label: String, path: PathBuf, scan: ScannedRoot) -> StorageRoot {
    StorageRoot {
        label,
        path: path.to_string_lossy().into_owned(),
        exists: scan.exists,
        bytes: scan.bytes,
        entries: scan.entries,
        truncated: scan.truncated,
        children: scan
            .children
            .into_iter()
            .map(|c| StorageChild {
                name: c.name,
                bytes: c.bytes,
                kind: c.kind.to_owned(),
            })
            .collect(),
        largest_files: scan
            .largest_files
            .into_iter()
            .map(|(path, bytes)| StorageFile { path, bytes })
            .collect(),
    }
}

/// Blocking: walk every root (each with its own budget) and read the disk.
fn scan_all(roots: Vec<(String, PathBuf)>) -> ProjectStorageResponse {
    let disk = roots.first().and_then(|(_, path)| {
        let total = fs2::total_space(path).ok()?;
        let free = fs2::available_space(path).ok()?;
        Some(StorageDisk {
            total_bytes: total,
            free_bytes: free,
        })
    });
    let roots: Vec<StorageRoot> = roots
        .into_iter()
        .map(|(label, path)| {
            let scan = scan_root(&path);
            root_response(label, path, scan)
        })
        .collect();
    ProjectStorageResponse {
        scanned_at: aionui_common::now_ms(),
        total_bytes: roots.iter().map(|r| r.bytes).sum(),
        disk,
        roots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(day: &str, input: i64, cost: Option<i64>) -> UsageDayRow {
        UsageDayRow {
            day: day.into(),
            sums: UsageSums {
                input_tokens: input,
                cost_micro_usd: cost,
                turns: 1,
                ..Default::default()
            },
        }
    }

    #[test]
    fn days_are_zero_filled_oldest_first_and_summed() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        let (days, sum) = fill_days(
            vec![row("2026-11-18", 5, None), row("2026-11-13", 7, Some(40))],
            today,
            7,
        );
        let labels: Vec<&str> = days.iter().map(|d| d.day.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "2026-11-12",
                "2026-11-13",
                "2026-11-14",
                "2026-11-15",
                "2026-11-16",
                "2026-11-17",
                "2026-11-18"
            ]
        );
        assert_eq!(days[1].totals.input, 7);
        assert_eq!(days[2].totals, UsageTotals::default());
        assert_eq!((sum.input, sum.turns, sum.cost_micro_usd), (12, 2, Some(40)));
        assert_eq!(first_day(today, 7), NaiveDate::from_ymd_opt(2026, 11, 12).unwrap());
        assert_eq!(day_start_ms(today) % 86_400_000, 0);
    }

    #[test]
    fn only_7_30_90_days_are_accepted() {
        assert_eq!(range_days(None).unwrap(), 30);
        assert_eq!(range_days(Some(90)).unwrap(), 90);
        assert!(matches!(range_days(Some(14)), Err(InsightsError::InvalidDays)));
    }
}
