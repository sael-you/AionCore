//! Per-user browser settings (C30, Settings → Browser; TASK-124 named
//! instances): validation and a root-only JSON file per user under
//! AionCore's data dir. No migration: the browser user cannot reach the file
//! (it is not in the browser's own directory). A file written before TASK-124
//! has no instance rows; it reads as rows 1..=max_instances with default names
//! and is made explicit on the next write.

use std::path::{Path, PathBuf};

use aionui_api_types::{
    BROWSER_INSTANCE_COLORS, BROWSER_INSTANCE_EMOJI_MAX_BYTES, BROWSER_INSTANCE_NAME_MAX, BROWSER_MAX_INSTANCES_LIMIT,
    BROWSER_MAX_TABS_LIMIT, BrowserEngine, BrowserErrorCode, BrowserInstanceCreate, BrowserInstancePatch,
    BrowserInstanceSettings, BrowserSettings, BrowserSettingsUpdate, browser_instance_default_name,
};
use serde::{Deserialize, Serialize};

use crate::error::BrowserError;
use crate::layout::safe_user_segment;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredInstance {
    instance: u8,
    name: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    emoji: Option<String>,
    #[serde(default)]
    hide_automation: bool,
    /// C35: the engine of this instance (absent in older files = Chrome).
    #[serde(default)]
    engine: BrowserEngine,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSettings {
    agents_enabled: bool,
    max_instances: u8,
    max_tabs: u8,
    /// Pre-TASK-124 per-instance flag list (read only for the migration).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hide_automation: Vec<u8>,
    /// The existing instances; `None` = written before TASK-124.
    #[serde(default)]
    instances: Option<Vec<StoredInstance>>,
    /// C34: "Agents may use my Mac browser" (the owner's own browser, paired
    /// through the extension; an extra bridge instance next to the profiles).
    #[serde(default)]
    devices_agents_enabled: bool,
}

impl Default for StoredSettings {
    fn default() -> Self {
        let d = BrowserSettings::default();
        Self {
            agents_enabled: d.agents_enabled,
            max_instances: d.max_instances,
            max_tabs: d.max_tabs,
            hide_automation: Vec::new(),
            instances: None,
            devices_agents_enabled: d.devices_agents_enabled,
        }
    }
}

fn out_of_range(what: &str) -> BrowserError {
    BrowserError::Coded(
        BrowserErrorCode::BrowserSettingOutOfRange,
        format!("{what} is out of range"),
    )
}

fn not_found(n: u8) -> BrowserError {
    BrowserError::coded(BrowserErrorCode::BrowserInstanceNotFound, format!("no instance {n}"))
}

/// A trimmed name of 1..=40 characters without control characters.
pub fn validate_name(raw: &str) -> Result<String, BrowserError> {
    let name = raw.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > BROWSER_INSTANCE_NAME_MAX || name.chars().any(char::is_control) {
        return Err(out_of_range("name"));
    }
    Ok(name.to_owned())
}

/// `""` = clear; otherwise a palette id.
fn validate_color(raw: &str) -> Result<Option<String>, BrowserError> {
    match raw.trim() {
        "" => Ok(None),
        id if BROWSER_INSTANCE_COLORS.contains(&id) => Ok(Some(id.to_owned())),
        _ => Err(out_of_range("color")),
    }
}

/// `""` = clear; otherwise at most 16 bytes, no control characters.
fn validate_emoji(raw: &str) -> Result<Option<String>, BrowserError> {
    let e = raw.trim();
    if e.is_empty() {
        return Ok(None);
    }
    if e.len() > BROWSER_INSTANCE_EMOJI_MAX_BYTES || e.chars().any(char::is_control) {
        return Err(out_of_range("emoji"));
    }
    Ok(Some(e.to_owned()))
}

impl StoredSettings {
    /// The instance rows (migrated from a pre-TASK-124 file when needed),
    /// cleaned: numbers 1..=5 once each, instance 1 present, valid unique names.
    fn rows(&self) -> Vec<StoredInstance> {
        let mut rows = match &self.instances {
            Some(rows) => rows.clone(),
            None => (1..=self.max_instances.clamp(1, BROWSER_MAX_INSTANCES_LIMIT))
                .map(|n| StoredInstance {
                    instance: n,
                    name: browser_instance_default_name(n),
                    color: None,
                    emoji: None,
                    hide_automation: self.hide_automation.contains(&n),
                    engine: BrowserEngine::default(),
                })
                .collect(),
        };
        rows.retain(|r| (1..=BROWSER_MAX_INSTANCES_LIMIT).contains(&r.instance));
        rows.sort_by_key(|r| r.instance);
        rows.dedup_by_key(|r| r.instance);
        if rows.first().map(|r| r.instance) != Some(1) {
            rows.insert(
                0,
                StoredInstance {
                    instance: 1,
                    name: browser_instance_default_name(1),
                    color: None,
                    emoji: None,
                    hide_automation: false,
                    engine: BrowserEngine::default(),
                },
            );
        }
        let mut seen: Vec<String> = Vec::new();
        for r in &mut rows {
            let valid = validate_name(&r.name)
                .ok()
                .filter(|n| !seen.contains(&n.to_lowercase()));
            r.name = valid.unwrap_or_else(|| browser_instance_default_name(r.instance));
            seen.push(r.name.to_lowercase());
            r.color = r.color.as_deref().and_then(|c| validate_color(c).ok().flatten());
            r.emoji = r.emoji.as_deref().and_then(|e| validate_emoji(e).ok().flatten());
        }
        rows
    }

    fn to_wire(&self) -> BrowserSettings {
        BrowserSettings {
            agents_enabled: self.agents_enabled,
            max_instances: self.max_instances,
            max_tabs: self.max_tabs,
            devices_agents_enabled: self.devices_agents_enabled,
            instances: self
                .rows()
                .into_iter()
                .map(|r| BrowserInstanceSettings {
                    instance: r.instance,
                    name: r.name,
                    color: r.color,
                    emoji: r.emoji,
                    hide_automation: r.hide_automation,
                    engine: r.engine,
                })
                .collect(),
        }
    }

    /// Clamp values a hand-edited file could carry out of range.
    fn sanitized(mut self) -> Self {
        self.max_tabs = self.max_tabs.clamp(1, BROWSER_MAX_TABS_LIMIT);
        let rows = self.rows();
        self.max_instances = self
            .max_instances
            .clamp(1, BROWSER_MAX_INSTANCES_LIMIT)
            .max(rows.len() as u8);
        self
    }

    /// Make the rows explicit (drops the pre-TASK-124 list).
    fn with_rows(mut self, rows: Vec<StoredInstance>) -> Self {
        self.instances = Some(rows);
        self.hide_automation.clear();
        self
    }
}

/// Apply a patch to row `n` of `rows` (names stay unique case-insensitively).
fn patch_row(
    rows: &mut [StoredInstance],
    n: u8,
    name: Option<&str>,
    color: Option<&str>,
    emoji: Option<&str>,
    hide_automation: Option<bool>,
    engine: Option<BrowserEngine>,
) -> Result<(), BrowserError> {
    let name = name.map(validate_name).transpose()?;
    if let Some(name) = &name
        && rows
            .iter()
            .any(|r| r.instance != n && r.name.to_lowercase() == name.to_lowercase())
    {
        return Err(BrowserError::coded(
            BrowserErrorCode::BrowserProfileNameTaken,
            "another instance has that name",
        ));
    }
    let color = color.map(validate_color).transpose()?;
    let emoji = emoji.map(validate_emoji).transpose()?;
    let row = rows.iter_mut().find(|r| r.instance == n).ok_or_else(|| not_found(n))?;
    if let Some(name) = name {
        row.name = name;
    }
    if let Some(color) = color {
        row.color = color;
    }
    if let Some(emoji) = emoji {
        row.emoji = emoji;
    }
    if let Some(h) = hide_automation {
        row.hide_automation = h;
    }
    if let Some(e) = engine {
        row.engine = e;
    }
    Ok(())
}

/// File store: `<dir>/<user>.json`, written atomically (temp + rename), 0600.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    dir: PathBuf,
}

impl SettingsStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, user_id: &str) -> Result<PathBuf, BrowserError> {
        Ok(self.dir.join(format!("{}.json", safe_user_segment(user_id)?)))
    }

    fn load_stored(&self, user_id: &str) -> Result<StoredSettings, BrowserError> {
        let path = self.path(user_id)?;
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StoredSettings>(&bytes) {
                Ok(s) => Ok(s.sanitized()),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "browser settings unreadable; using defaults");
                    Ok(StoredSettings::default())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoredSettings::default()),
            Err(e) => Err(BrowserError::Io(e)),
        }
    }

    fn save(&self, user_id: &str, stored: StoredSettings) -> Result<BrowserSettings, BrowserError> {
        write_private_json(&self.path(user_id)?, &stored)?;
        Ok(stored.to_wire())
    }

    pub fn load(&self, user_id: &str) -> Result<BrowserSettings, BrowserError> {
        Ok(self.load_stored(user_id)?.to_wire())
    }

    /// Validate and persist; returns the new settings.
    pub fn update(&self, user_id: &str, update: &BrowserSettingsUpdate) -> Result<BrowserSettings, BrowserError> {
        let mut stored = self.load_stored(user_id)?;
        let mut rows = stored.rows();
        if let Some(v) = update.agents_enabled {
            stored.agents_enabled = v;
        }
        if let Some(v) = update.devices_agents_enabled {
            stored.devices_agents_enabled = v;
        }
        if let Some(v) = update.max_tabs {
            if !(1..=BROWSER_MAX_TABS_LIMIT).contains(&v) {
                return Err(out_of_range("max_tabs"));
            }
            stored.max_tabs = v;
        }
        if let Some(v) = update.max_instances {
            // Never below the existing instances (no silent deletes).
            if !(1..=BROWSER_MAX_INSTANCES_LIMIT).contains(&v) || usize::from(v) < rows.len() {
                return Err(out_of_range("max_instances"));
            }
            stored.max_instances = v;
        }
        for row in update.instances.iter().flatten() {
            patch_row(
                &mut rows,
                row.instance,
                row.name.as_deref(),
                row.color.as_deref(),
                row.emoji.as_deref(),
                row.hide_automation,
                row.engine,
            )?;
        }
        self.save(user_id, stored.with_rows(rows))
    }

    /// A new instance at the lowest free number; returns (settings, number).
    pub fn create(&self, user_id: &str, req: &BrowserInstanceCreate) -> Result<(BrowserSettings, u8), BrowserError> {
        let stored = self.load_stored(user_id)?;
        let mut rows = stored.rows();
        if rows.len() >= usize::from(stored.max_instances) {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserInstanceLimit,
                "all instances are in use",
            ));
        }
        let n = (1..=BROWSER_MAX_INSTANCES_LIMIT)
            .find(|n| !rows.iter().any(|r| r.instance == *n))
            .ok_or_else(|| BrowserError::coded(BrowserErrorCode::BrowserInstanceLimit, "no free instance"))?;
        rows.push(StoredInstance {
            instance: n,
            name: browser_instance_default_name(n),
            color: None,
            emoji: None,
            hide_automation: false,
            engine: BrowserEngine::default(),
        });
        rows.sort_by_key(|r| r.instance);
        patch_row(
            &mut rows,
            n,
            Some(&req.name),
            req.color.as_deref(),
            req.emoji.as_deref(),
            req.hide_automation,
            req.engine,
        )?;
        Ok((self.save(user_id, stored.with_rows(rows))?, n))
    }

    pub fn patch(&self, user_id: &str, n: u8, req: &BrowserInstancePatch) -> Result<BrowserSettings, BrowserError> {
        let stored = self.load_stored(user_id)?;
        let mut rows = stored.rows();
        patch_row(
            &mut rows,
            n,
            req.name.as_deref(),
            req.color.as_deref(),
            req.emoji.as_deref(),
            req.hide_automation,
            req.engine,
        )?;
        self.save(user_id, stored.with_rows(rows))
    }

    /// Remove instance `n`'s row (the caller stops it and wipes its profile).
    pub fn delete(&self, user_id: &str, n: u8) -> Result<BrowserSettings, BrowserError> {
        if n == 1 {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserInstanceProtected,
                "the main instance cannot be deleted",
            ));
        }
        let stored = self.load_stored(user_id)?;
        let mut rows = stored.rows();
        if !rows.iter().any(|r| r.instance == n) {
            return Err(not_found(n));
        }
        rows.retain(|r| r.instance != n);
        self.save(user_id, stored.with_rows(rows))
    }

    /// Whether instance `n` exists for `user_id`.
    pub fn exists(&self, user_id: &str, n: u8) -> Result<bool, BrowserError> {
        Ok(self.load_stored(user_id)?.rows().iter().any(|r| r.instance == n))
    }

    /// The engine `instance` launches with.
    pub fn engine(&self, user_id: &str, instance: u8) -> Result<BrowserEngine, BrowserError> {
        Ok(self
            .load_stored(user_id)?
            .rows()
            .iter()
            .find(|r| r.instance == instance)
            .map(|r| r.engine)
            .unwrap_or_default())
    }

    /// Whether `instance` launches with the automation flag hidden.
    pub fn hide_automation(&self, user_id: &str, instance: u8) -> Result<bool, BrowserError> {
        Ok(self
            .load_stored(user_id)?
            .rows()
            .iter()
            .any(|r| r.instance == instance && r.hide_automation))
    }
}

pub(crate) fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<(), BrowserError> {
    let dir = path
        .parent()
        .ok_or_else(|| BrowserError::Internal("settings path has no parent".into()))?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let body = serde_json::to_vec_pretty(value).map_err(|e| BrowserError::Internal(e.to_string()))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &body))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::BrowserInstanceUpdate;

    fn store() -> (tempfile::TempDir, SettingsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("settings"));
        (dir, store)
    }

    fn create(name: &str) -> BrowserInstanceCreate {
        BrowserInstanceCreate {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn defaults_are_main_plus_numbered_instances_and_persist_per_user() {
        let (_d, s) = store();
        let d = s.load("u1").unwrap();
        assert_eq!(d, BrowserSettings::default());
        assert_eq!(d.instances[0].name, "Main");
        assert_eq!(d.instances[2].name, "Instance 3");
        let saved = s
            .update(
                "u1",
                &BrowserSettingsUpdate {
                    agents_enabled: Some(false),
                    max_tabs: Some(4),
                    instances: Some(vec![BrowserInstanceUpdate {
                        instance: 2,
                        name: Some("  TB Discord ".into()),
                        color: Some("indigo".into()),
                        emoji: Some("🎮".into()),
                        hide_automation: Some(true),
                        engine: Some(BrowserEngine::Brave),
                    }]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!saved.agents_enabled);
        assert_eq!(saved.instances[1].name, "TB Discord");
        assert_eq!(saved.instances[1].color.as_deref(), Some("indigo"));
        assert_eq!(saved.instances[1].emoji.as_deref(), Some("🎮"));
        assert_eq!(s.load("u1").unwrap(), saved, "read back from disk");
        assert!(s.hide_automation("u1", 2).unwrap());
        assert_eq!(s.engine("u1", 2).unwrap(), BrowserEngine::Brave);
        assert_eq!(s.engine("u1", 1).unwrap(), BrowserEngine::Chrome, "others keep Chrome");
        assert_eq!(saved.instances[1].engine, BrowserEngine::Brave);
        assert_eq!(
            s.load("u2").unwrap(),
            BrowserSettings::default(),
            "other users untouched"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(s.path("u1").unwrap()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_pre_task_124_file_migrates_with_its_flags() {
        let (_d, s) = store();
        std::fs::create_dir_all(s.dir.clone()).unwrap();
        std::fs::write(
            s.path("u1").unwrap(),
            r#"{"agents_enabled":true,"max_instances":4,"max_tabs":10,"hide_automation":[3]}"#,
        )
        .unwrap();
        let st = s.load("u1").unwrap();
        let names: Vec<&str> = st.instances.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["Main", "Instance 2", "Instance 3", "Instance 4"]);
        assert!(st.instances[2].hide_automation);
    }

    #[test]
    fn create_patch_and_delete_follow_the_rules() {
        let (_d, s) = store();
        // Defaults fill the cap (3): a fourth needs a higher cap.
        let err = s.create("u1", &create("Work")).unwrap_err();
        assert_eq!(err.code(), Some(BrowserErrorCode::BrowserInstanceLimit));
        s.update(
            "u1",
            &BrowserSettingsUpdate {
                max_instances: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        let (st, n) = s.create("u1", &create("Work")).unwrap();
        assert_eq!(n, 4);
        assert_eq!(st.instances.len(), 4);
        // Names are unique case-insensitively.
        let err = s.create("u1", &create("work")).unwrap_err();
        assert_eq!(err.code(), Some(BrowserErrorCode::BrowserProfileNameTaken));
        let err = s
            .patch(
                "u1",
                2,
                &BrowserInstancePatch {
                    name: Some("MAIN".into()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert_eq!(err.code(), Some(BrowserErrorCode::BrowserProfileNameTaken));
        // "" clears colour / emoji; absent keeps them.
        s.patch(
            "u1",
            4,
            &BrowserInstancePatch {
                color: Some("teal".into()),
                emoji: Some("💼".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let st = s
            .patch(
                "u1",
                4,
                &BrowserInstancePatch {
                    color: Some(String::new()),
                    ..Default::default()
                },
            )
            .unwrap();
        let row = st.instances.iter().find(|i| i.instance == 4).unwrap();
        assert_eq!((row.color.as_deref(), row.emoji.as_deref()), (None, Some("💼")));
        // Delete: 1 is protected, unknown is 404, the gap is reused by the next create.
        assert_eq!(
            s.delete("u1", 1).unwrap_err().code(),
            Some(BrowserErrorCode::BrowserInstanceProtected)
        );
        assert_eq!(
            s.delete("u1", 5).unwrap_err().code(),
            Some(BrowserErrorCode::BrowserInstanceNotFound)
        );
        let st = s.delete("u1", 2).unwrap();
        assert!(!st.instances.iter().any(|i| i.instance == 2));
        assert!(!s.exists("u1", 2).unwrap());
        let (_, n) = s.create("u1", &create("Again")).unwrap();
        assert_eq!(n, 2, "lowest free number");
    }

    #[test]
    fn out_of_range_values_are_refused_with_the_code() {
        let (_d, s) = store();
        let bad_patch = |p: BrowserInstancePatch| s.patch("u1", 1, &p).unwrap_err().code();
        for p in [
            BrowserInstancePatch {
                name: Some("   ".into()),
                ..Default::default()
            },
            BrowserInstancePatch {
                name: Some("x".repeat(41)),
                ..Default::default()
            },
            BrowserInstancePatch {
                name: Some("bad\nname".into()),
                ..Default::default()
            },
            BrowserInstancePatch {
                color: Some("#ff0000".into()),
                ..Default::default()
            },
            BrowserInstancePatch {
                emoji: Some("x".repeat(17)),
                ..Default::default()
            },
        ] {
            assert_eq!(
                bad_patch(p.clone()),
                Some(BrowserErrorCode::BrowserSettingOutOfRange),
                "{p:?}"
            );
        }
        for update in [
            BrowserSettingsUpdate {
                max_instances: Some(0),
                ..Default::default()
            },
            BrowserSettingsUpdate {
                max_instances: Some(2),
                ..Default::default()
            },
            BrowserSettingsUpdate {
                max_tabs: Some(21),
                ..Default::default()
            },
        ] {
            let err = s.update("u1", &update).unwrap_err();
            assert_eq!(
                err.code(),
                Some(BrowserErrorCode::BrowserSettingOutOfRange),
                "{update:?}"
            );
        }
        let unknown = BrowserSettingsUpdate {
            instances: Some(vec![BrowserInstanceUpdate {
                instance: 5,
                name: Some("x".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(
            s.update("u1", &unknown).unwrap_err().code(),
            Some(BrowserErrorCode::BrowserInstanceNotFound)
        );
        assert_eq!(s.load("u1").unwrap(), BrowserSettings::default(), "nothing persisted");
    }

    #[test]
    fn user_ids_that_are_not_path_safe_are_refused() {
        let (_d, s) = store();
        assert!(s.load("../etc").is_err());
        assert!(s.load("").is_err());
    }

    /// C35: a file written before engines existed has no `engine` key; every
    /// instance reads as Chrome, and a patch changes only the engine.
    #[test]
    fn engine_defaults_to_chrome_for_older_files_and_is_patchable() {
        let (_d, s) = store();
        std::fs::create_dir_all(s.dir.clone()).unwrap();
        std::fs::write(
            s.path("u1").unwrap(),
            r#"{"agents_enabled":true,"max_instances":3,"max_tabs":10,"instances":[{"instance":1,"name":"Main","hide_automation":true}]}"#,
        )
        .unwrap();
        assert_eq!(s.engine("u1", 1).unwrap(), BrowserEngine::Chrome);
        let patched = s
            .patch(
                "u1",
                1,
                &BrowserInstancePatch {
                    engine: Some(BrowserEngine::HeadlessShell),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(patched.instances[0].engine, BrowserEngine::HeadlessShell);
        assert!(patched.instances[0].hide_automation, "other fields untouched");
        assert_eq!(patched.instances[0].name, "Main");
        let (_, n) = s
            .create(
                "u1",
                &BrowserInstanceCreate {
                    name: "Edge work".into(),
                    engine: Some(BrowserEngine::Edge),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(s.engine("u1", n).unwrap(), BrowserEngine::Edge);
        let wire = serde_json::to_value(s.load("u1").unwrap()).unwrap();
        assert_eq!(wire["instances"][0]["engine"], "headless-shell");
        assert!(serde_json::from_str::<BrowserInstancePatch>(r#"{"engine":"firefox"}"#).is_err());
    }
}
