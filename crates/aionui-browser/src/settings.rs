//! Per-user browser settings (C30, Settings → Browser): validation and a
//! root-only JSON file per user under AionCore's data dir. No migration: the
//! file holds four values and the browser user cannot reach it (it is not in
//! the browser's own directory).

use std::path::{Path, PathBuf};

use aionui_api_types::{
    BROWSER_MAX_INSTANCES_LIMIT, BROWSER_MAX_TABS_LIMIT, BrowserErrorCode, BrowserInstanceSettings, BrowserSettings,
    BrowserSettingsUpdate,
};
use serde::{Deserialize, Serialize};

use crate::error::BrowserError;
use crate::layout::safe_user_segment;

/// What is stored: the wire settings plus the per-instance rows of every
/// possible instance (a lowered `max_instances` keeps the hidden rows).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSettings {
    agents_enabled: bool,
    max_instances: u8,
    max_tabs: u8,
    #[serde(default)]
    hide_automation: Vec<u8>,
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
            devices_agents_enabled: d.devices_agents_enabled,
        }
    }
}

impl StoredSettings {
    fn to_wire(&self) -> BrowserSettings {
        BrowserSettings {
            agents_enabled: self.agents_enabled,
            max_instances: self.max_instances,
            max_tabs: self.max_tabs,
            devices_agents_enabled: self.devices_agents_enabled,
            instances: (1..=self.max_instances)
                .map(|instance| BrowserInstanceSettings {
                    instance,
                    hide_automation: self.hide_automation.contains(&instance),
                })
                .collect(),
        }
    }

    /// Clamp values a hand-edited file could carry out of range.
    fn sanitized(mut self) -> Self {
        self.max_instances = self.max_instances.clamp(1, BROWSER_MAX_INSTANCES_LIMIT);
        self.max_tabs = self.max_tabs.clamp(1, BROWSER_MAX_TABS_LIMIT);
        self.hide_automation
            .retain(|n| (1..=BROWSER_MAX_INSTANCES_LIMIT).contains(n));
        self.hide_automation.sort_unstable();
        self.hide_automation.dedup();
        self
    }
}

fn out_of_range(what: &str) -> BrowserError {
    BrowserError::Coded(
        BrowserErrorCode::BrowserSettingOutOfRange,
        format!("{what} is out of range"),
    )
}

/// Apply an update to `current`, refusing any out-of-range value.
pub fn apply_update(
    current: &BrowserSettings,
    update: &BrowserSettingsUpdate,
) -> Result<BrowserSettings, BrowserError> {
    let mut next = current.clone();
    if let Some(v) = update.agents_enabled {
        next.agents_enabled = v;
    }
    if let Some(v) = update.devices_agents_enabled {
        next.devices_agents_enabled = v;
    }
    if let Some(v) = update.max_instances {
        if !(1..=BROWSER_MAX_INSTANCES_LIMIT).contains(&v) {
            return Err(out_of_range("max_instances"));
        }
        next.max_instances = v;
    }
    if let Some(v) = update.max_tabs {
        if !(1..=BROWSER_MAX_TABS_LIMIT).contains(&v) {
            return Err(out_of_range("max_tabs"));
        }
        next.max_tabs = v;
    }
    if let Some(rows) = &update.instances {
        for row in rows {
            if !(1..=BROWSER_MAX_INSTANCES_LIMIT).contains(&row.instance) {
                return Err(out_of_range("instance"));
            }
        }
    }
    Ok(next)
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

    pub fn load(&self, user_id: &str) -> Result<BrowserSettings, BrowserError> {
        Ok(self.load_stored(user_id)?.to_wire())
    }

    /// Validate and persist; returns the new settings.
    pub fn update(&self, user_id: &str, update: &BrowserSettingsUpdate) -> Result<BrowserSettings, BrowserError> {
        let mut stored = self.load_stored(user_id)?;
        let next = apply_update(&stored.to_wire(), update)?;
        stored.agents_enabled = next.agents_enabled;
        stored.max_instances = next.max_instances;
        stored.max_tabs = next.max_tabs;
        stored.devices_agents_enabled = next.devices_agents_enabled;
        if let Some(rows) = &update.instances {
            for row in rows {
                stored.hide_automation.retain(|n| *n != row.instance);
                if row.hide_automation {
                    stored.hide_automation.push(row.instance);
                }
            }
        }
        let stored = stored.sanitized();
        write_private_json(&self.path(user_id)?, &stored)?;
        Ok(stored.to_wire())
    }

    /// Whether `instance` launches with the automation flag hidden.
    pub fn hide_automation(&self, user_id: &str, instance: u8) -> Result<bool, BrowserError> {
        Ok(self.load_stored(user_id)?.hide_automation.contains(&instance))
    }
}

fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<(), BrowserError> {
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

    fn store() -> (tempfile::TempDir, SettingsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("settings"));
        (dir, store)
    }

    #[test]
    fn defaults_until_saved_then_persisted_per_user() {
        let (_d, s) = store();
        assert_eq!(s.load("u1").unwrap(), BrowserSettings::default());
        let saved = s
            .update(
                "u1",
                &BrowserSettingsUpdate {
                    agents_enabled: Some(false),
                    max_tabs: Some(4),
                    instances: Some(vec![BrowserInstanceSettings {
                        instance: 2,
                        hide_automation: true,
                    }]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!saved.agents_enabled);
        assert_eq!(saved.max_tabs, 4);
        assert_eq!(saved.max_instances, 3, "unchanged field kept");
        assert_eq!(s.load("u1").unwrap(), saved, "read back from disk");
        assert!(s.hide_automation("u1", 2).unwrap());
        assert!(!s.hide_automation("u1", 1).unwrap());
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
    fn out_of_range_values_are_refused_with_the_code() {
        let (_d, s) = store();
        for update in [
            BrowserSettingsUpdate {
                max_instances: Some(0),
                ..Default::default()
            },
            BrowserSettingsUpdate {
                max_instances: Some(6),
                ..Default::default()
            },
            BrowserSettingsUpdate {
                max_tabs: Some(0),
                ..Default::default()
            },
            BrowserSettingsUpdate {
                max_tabs: Some(21),
                ..Default::default()
            },
            BrowserSettingsUpdate {
                instances: Some(vec![BrowserInstanceSettings {
                    instance: 6,
                    hide_automation: true,
                }]),
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
        assert_eq!(s.load("u1").unwrap(), BrowserSettings::default(), "nothing persisted");
    }

    #[test]
    fn a_lowered_instance_limit_keeps_hidden_rows_for_later() {
        let (_d, s) = store();
        s.update(
            "u1",
            &BrowserSettingsUpdate {
                instances: Some(vec![BrowserInstanceSettings {
                    instance: 3,
                    hide_automation: true,
                }]),
                ..Default::default()
            },
        )
        .unwrap();
        let low = s
            .update(
                "u1",
                &BrowserSettingsUpdate {
                    max_instances: Some(2),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(low.instances.len(), 2);
        let back = s
            .update(
                "u1",
                &BrowserSettingsUpdate {
                    max_instances: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(back.instances[2].hide_automation);
    }

    #[test]
    fn user_ids_that_are_not_path_safe_are_refused() {
        let (_d, s) = store();
        assert!(s.load("../etc").is_err());
        assert!(s.load("").is_err());
    }
}
