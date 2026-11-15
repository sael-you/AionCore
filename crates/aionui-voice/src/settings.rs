//! Voice settings: defaults, validation, and the effective call profile.

use aionui_api_types::{UpdateVoiceSettingsRequest, VoiceSettingsResponse};
use aionui_db::VoiceSettingsRow;

use crate::error::VoiceError;

pub const ENGINE_OPENAI_REALTIME: &str = "openai-realtime";
pub const MODEL_FULL: &str = "gpt-realtime-2.1";
pub const MODEL_MINI: &str = "gpt-realtime-2.1-mini";
pub const DEFAULT_VOICE: &str = "marin";
pub const DEFAULT_DAILY_BUDGET_MICROS: i64 = 5_000_000;
/// Upper bound for the daily budget setting (guards typos like 5000).
pub const MAX_DAILY_BUDGET_MICROS: i64 = 100_000_000;

/// Engines selectable in this build. `openai-relay` arrives with S3,
/// `minimax-cascade` with S4.
const SUPPORTED_ENGINES: &[&str] = &[ENGINE_OPENAI_REALTIME];
const SUPPORTED_MODELS: &[&str] = &[MODEL_FULL, MODEL_MINI];
const SUPPORTED_EAGERNESS: &[&str] = &["auto", "low", "medium", "high"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSettings {
    pub enabled: bool,
    pub engine: String,
    pub model: String,
    pub car_mode: bool,
    pub voice: String,
    pub eagerness: String,
    pub daily_budget_micros: i64,
    pub provider_id: Option<String>,
    extra: String,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            engine: ENGINE_OPENAI_REALTIME.into(),
            model: MODEL_FULL.into(),
            car_mode: false,
            voice: DEFAULT_VOICE.into(),
            eagerness: "auto".into(),
            daily_budget_micros: DEFAULT_DAILY_BUDGET_MICROS,
            provider_id: None,
            extra: "{}".into(),
        }
    }
}

/// The call profile actually sent to the provider (§3.5 car mode = mini model,
/// `far_field`, eagerness `low`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallProfile {
    pub engine: String,
    pub model: String,
    pub voice: String,
    pub eagerness: String,
    pub noise_reduction: String,
}

impl VoiceSettings {
    pub fn from_row(row: VoiceSettingsRow) -> Self {
        Self {
            enabled: row.enabled,
            engine: row.engine,
            model: row.model,
            car_mode: row.car_mode,
            voice: row.voice,
            eagerness: row.eagerness,
            daily_budget_micros: row.daily_budget_micros,
            provider_id: row.provider_id,
            extra: row.extra,
        }
    }

    pub fn to_row(&self, user_id: &str) -> VoiceSettingsRow {
        VoiceSettingsRow {
            user_id: user_id.to_owned(),
            enabled: self.enabled,
            engine: self.engine.clone(),
            model: self.model.clone(),
            car_mode: self.car_mode,
            voice: self.voice.clone(),
            eagerness: self.eagerness.clone(),
            daily_budget_micros: self.daily_budget_micros,
            provider_id: self.provider_id.clone(),
            extra: self.extra.clone(),
            updated_at: aionui_common::now_ms(),
        }
    }

    pub fn to_response(&self, used_today_micros: Option<i64>) -> VoiceSettingsResponse {
        VoiceSettingsResponse {
            enabled: self.enabled,
            engine: self.engine.clone(),
            wake_word_enabled: self.wake_word_enabled(),
            car_mode: self.car_mode,
            daily_budget_usd: micros_to_usd(self.daily_budget_micros),
            used_today_usd: used_today_micros.map(micros_to_usd),
            model: self.model.clone(),
            voice: self.voice.clone(),
            eagerness: self.eagerness.clone(),
            provider_id: self.provider_id.clone(),
        }
    }

    fn extra_json(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(&self.extra)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    /// Wake word on by default (§1); stored in the `extra` JSON column.
    pub fn wake_word_enabled(&self) -> bool {
        self.extra_json()
            .get("wake_word_enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    }

    fn set_extra(&mut self, key: &str, value: serde_json::Value) {
        let mut extra = self.extra_json();
        extra.insert(key.to_owned(), value);
        self.extra = serde_json::Value::Object(extra).to_string();
    }

    /// Apply a validated partial update.
    pub fn apply(&self, req: &UpdateVoiceSettingsRequest) -> Result<Self, VoiceError> {
        let mut next = self.clone();
        if let Some(enabled) = req.enabled {
            next.enabled = enabled;
        }
        if let Some(engine) = &req.engine {
            ensure_one_of("engine", engine, SUPPORTED_ENGINES)?;
            next.engine = engine.clone();
        }
        if let Some(model) = &req.model {
            ensure_one_of("model", model, SUPPORTED_MODELS)?;
            next.model = model.clone();
        }
        if let Some(wake_word) = req.wake_word_enabled {
            next.set_extra("wake_word_enabled", serde_json::Value::Bool(wake_word));
        }
        if let Some(car_mode) = req.car_mode {
            next.car_mode = car_mode;
        }
        if let Some(voice) = &req.voice {
            let valid = !voice.is_empty()
                && voice.len() <= 32
                && voice.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !valid {
                return Err(VoiceError::BadRequest(
                    "voice must be 1-32 characters [A-Za-z0-9_-]".into(),
                ));
            }
            next.voice = voice.clone();
        }
        if let Some(eagerness) = &req.eagerness {
            ensure_one_of("eagerness", eagerness, SUPPORTED_EAGERNESS)?;
            next.eagerness = eagerness.clone();
        }
        if let Some(usd) = req.daily_budget_usd {
            if !usd.is_finite() || usd < 0.0 {
                return Err(VoiceError::BadRequest(
                    "daily_budget_usd must be a non-negative number".into(),
                ));
            }
            let micros = (usd * 1_000_000.0).round() as i64;
            if micros > MAX_DAILY_BUDGET_MICROS {
                return Err(VoiceError::BadRequest(format!(
                    "daily_budget_usd must be at most {}",
                    micros_to_usd(MAX_DAILY_BUDGET_MICROS)
                )));
            }
            next.daily_budget_micros = micros;
        }
        if let Some(provider_id) = &req.provider_id {
            let trimmed = provider_id.trim();
            if trimmed.len() > 128 {
                return Err(VoiceError::BadRequest("provider_id is too long".into()));
            }
            next.provider_id = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        }
        Ok(next)
    }

    pub fn call_profile(&self) -> CallProfile {
        if self.car_mode {
            CallProfile {
                engine: self.engine.clone(),
                model: MODEL_MINI.into(),
                voice: self.voice.clone(),
                eagerness: "low".into(),
                noise_reduction: "far_field".into(),
            }
        } else {
            CallProfile {
                engine: self.engine.clone(),
                model: self.model.clone(),
                voice: self.voice.clone(),
                eagerness: self.eagerness.clone(),
                noise_reduction: "far_field".into(),
            }
        }
    }
}

fn ensure_one_of(field: &str, value: &str, allowed: &[&str]) -> Result<(), VoiceError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(VoiceError::BadRequest(format!(
            "{field} must be one of: {}",
            allowed.join(", ")
        )))
    }
}

pub fn micros_to_usd(micros: i64) -> f64 {
    micros as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_voice_off() {
        let settings = VoiceSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.daily_budget_micros, 5_000_000);
        assert_eq!(settings.model, MODEL_FULL);
    }

    #[test]
    fn apply_validates_each_field() {
        let base = VoiceSettings::default();
        let ok = base
            .apply(&UpdateVoiceSettingsRequest {
                enabled: Some(true),
                model: Some(MODEL_MINI.into()),
                daily_budget_usd: Some(2.5),
                provider_id: Some(" prov-1 ".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(ok.enabled);
        assert_eq!(ok.model, MODEL_MINI);
        assert_eq!(ok.daily_budget_micros, 2_500_000);
        assert_eq!(ok.provider_id.as_deref(), Some("prov-1"));

        let cleared = ok
            .apply(&UpdateVoiceSettingsRequest {
                provider_id: Some(String::new()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(cleared.provider_id, None);

        for bad in [
            UpdateVoiceSettingsRequest {
                engine: Some("minimax-cascade".into()),
                ..Default::default()
            },
            UpdateVoiceSettingsRequest {
                model: Some("gpt-realtime".into()),
                ..Default::default()
            },
            UpdateVoiceSettingsRequest {
                eagerness: Some("eager".into()),
                ..Default::default()
            },
            UpdateVoiceSettingsRequest {
                voice: Some("<script>".into()),
                ..Default::default()
            },
            UpdateVoiceSettingsRequest {
                daily_budget_usd: Some(-1.0),
                ..Default::default()
            },
            UpdateVoiceSettingsRequest {
                daily_budget_usd: Some(5000.0),
                ..Default::default()
            },
            UpdateVoiceSettingsRequest {
                daily_budget_usd: Some(f64::NAN),
                ..Default::default()
            },
        ] {
            assert!(matches!(base.apply(&bad), Err(VoiceError::BadRequest(_))), "{bad:?}");
        }
    }

    #[test]
    fn wake_word_defaults_on_and_round_trips_through_extra() {
        let base = VoiceSettings::default();
        assert!(base.wake_word_enabled());
        let off = base
            .apply(&UpdateVoiceSettingsRequest {
                wake_word_enabled: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert!(!off.wake_word_enabled());
        let row = off.to_row("u");
        assert!(!VoiceSettings::from_row(row).wake_word_enabled());
        assert!(!off.to_response(Some(1_500_000)).wake_word_enabled);
        assert_eq!(off.to_response(Some(1_500_000)).used_today_usd, Some(1.5));
    }

    #[test]
    fn car_mode_profile_uses_mini_low_far_field() {
        let settings = VoiceSettings {
            car_mode: true,
            ..Default::default()
        };
        let profile = settings.call_profile();
        assert_eq!(profile.model, MODEL_MINI);
        assert_eq!(profile.eagerness, "low");
        assert_eq!(profile.noise_reduction, "far_field");
    }
}
