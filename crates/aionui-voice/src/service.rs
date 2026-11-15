//! Voice business logic: admission, settings + kill switch, status, session
//! end, restart recovery. Per-call work runs in [`crate::runner`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aionui_api_types::{
    CreateVoiceSessionRequest, CreateVoiceSessionResponse, UpdateVoiceSettingsRequest, VoiceSessionResponse,
    VoiceSettingsResponse, VoiceStatusResponse,
};
use aionui_db::{
    CreateVoiceSessionParams, DbError, IVoiceAuditRepository, IVoiceSessionRepository, IVoiceSettingsRepository,
    NewVoiceAuditEntry, VoiceSessionRow,
};
use dashmap::DashMap;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;

use crate::budget::{BudgetMeter, VOICE_MAX_OUTPUT_TOKENS, utc_day_start_ms};
use crate::credentials::{CredentialPolicy, ResolvedCredential};
use crate::engine::{EngineSessionConfig, OpenCallRequest, VoiceEngine};
use crate::error::VoiceError;
use crate::frames::{VoiceSockets, call_ended_frame, error_frame};
use crate::instructions::{BASE_INSTRUCTIONS, estimate_prefix_tokens, instructions_hash};
use crate::lifecycle::{EndReason, VoiceTimings};
use crate::ports::{AuthRejected, AuthVerifier, CredentialPort, VerifiedUser};
use crate::pricing::pricing_for;
use crate::runner::{CallCommand, CallRunner, RunMode};
use crate::settings::{VoiceSettings, micros_to_usd};

/// Largest SDP offer accepted (a browser audio-only offer is ≈ 2–6 KB).
pub const MAX_SDP_BYTES: usize = 32 * 1024;
const LIST_LIMIT: u32 = 50;
const TRANSCRIPTION_MODEL: &str = "gpt-transcribe";

/// Everything the voice service is built from (assembled in `aionui-app`).
pub struct VoiceServiceDeps {
    pub settings: Arc<dyn IVoiceSettingsRepository>,
    pub sessions: Arc<dyn IVoiceSessionRepository>,
    pub audit: Arc<dyn IVoiceAuditRepository>,
    pub credentials: Arc<dyn CredentialPort>,
    pub credential_policy: CredentialPolicy,
    pub engine: Arc<dyn VoiceEngine>,
    pub verifier: Arc<dyn AuthVerifier>,
    pub sockets: VoiceSockets,
    pub timings: VoiceTimings,
}

/// Liveness the WebSocket reports for the device that owns a call (§3.8).
#[derive(Debug)]
pub(crate) struct Liveness {
    pub last_heartbeat: Instant,
    pub frozen: bool,
}

/// A call currently driven by a runner task.
pub(crate) struct LiveCall {
    pub session_id: String,
    pub user_id: String,
    pub engine: String,
    pub model: String,
    pub cmd_tx: mpsc::Sender<CallCommand>,
    pub ready: AtomicBool,
    pub liveness: std::sync::Mutex<Liveness>,
}

impl LiveCall {
    fn heartbeat(&self) {
        if let Ok(mut l) = self.liveness.lock() {
            l.last_heartbeat = Instant::now();
        }
    }

    fn set_frozen(&self, frozen: bool) {
        if let Ok(mut l) = self.liveness.lock() {
            l.frozen = frozen;
            if !frozen {
                l.last_heartbeat = Instant::now();
            }
        }
    }
}

pub(crate) struct ServiceInner {
    pub settings: Arc<dyn IVoiceSettingsRepository>,
    pub sessions: Arc<dyn IVoiceSessionRepository>,
    pub audit: Arc<dyn IVoiceAuditRepository>,
    pub credentials: Arc<dyn CredentialPort>,
    pub credential_policy: CredentialPolicy,
    pub engine: Arc<dyn VoiceEngine>,
    pub verifier: Arc<dyn AuthVerifier>,
    pub sockets: VoiceSockets,
    pub timings: VoiceTimings,
    pub live: DashMap<String, Arc<LiveCall>>,
    pub tasks: DashMap<String, tokio::task::AbortHandle>,
    /// Serialises admissions per process so a resume/kill cannot interleave.
    admission: Mutex<()>,
}

impl ServiceInner {
    pub(crate) async fn audit(&self, user_id: &str, session_id: Option<&str>, event: &str, detail: Value) {
        let detail = detail.to_string();
        if let Err(err) = self
            .audit
            .append(NewVoiceAuditEntry {
                user_id,
                session_id,
                event,
                detail: &detail,
            })
            .await
        {
            tracing::warn!(error = %err, event, "voice audit append failed");
        }
    }
}

#[derive(Clone)]
pub struct VoiceService {
    inner: Arc<ServiceInner>,
}

impl VoiceService {
    pub fn new(deps: VoiceServiceDeps) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                settings: deps.settings,
                sessions: deps.sessions,
                audit: deps.audit,
                credentials: deps.credentials,
                credential_policy: deps.credential_policy,
                engine: deps.engine,
                verifier: deps.verifier,
                sockets: deps.sockets,
                timings: deps.timings,
                live: DashMap::new(),
                tasks: DashMap::new(),
                admission: Mutex::new(()),
            }),
        }
    }

    pub fn sockets(&self) -> &VoiceSockets {
        &self.inner.sockets
    }

    pub fn timings(&self) -> VoiceTimings {
        self.inner.timings
    }

    // ---------------------------------------------------------------- settings

    pub async fn settings(&self, user_id: &str) -> Result<VoiceSettings, VoiceError> {
        Ok(self
            .inner
            .settings
            .get(user_id)
            .await?
            .map(VoiceSettings::from_row)
            .unwrap_or_default())
    }

    /// Update settings. Turning Voice off is the kill switch: live calls are
    /// hung up and the user's voice sockets are closed.
    pub async fn update_settings(
        &self,
        user_id: &str,
        req: &UpdateVoiceSettingsRequest,
    ) -> Result<VoiceSettings, VoiceError> {
        let current = self.settings(user_id).await?;
        let next = current.apply(req)?;
        self.inner.settings.upsert(&next.to_row(user_id)).await?;
        let killed = current.enabled && !next.enabled;
        self.inner
            .audit(
                user_id,
                None,
                "settings_updated",
                json!({"enabled": next.enabled, "model": next.model, "carMode": next.car_mode,
                       "dailyBudgetMicros": next.daily_budget_micros}),
            )
            .await;
        tracing::info!(user_id, enabled = next.enabled, model = %next.model, "voice settings updated");
        if !next.enabled {
            self.kill_user(user_id, killed).await;
        }
        Ok(next)
    }

    /// Kill switch for one user: hang up, notify, close sockets.
    async fn kill_user(&self, user_id: &str, audit: bool) {
        let ended = self.end_user_calls(user_id, EndReason::Disabled).await;
        let sockets = &self.inner.sockets;
        sockets.send_user(user_id, &error_frame(None, "VOICE_DISABLED", "Voice is turned off."));
        let closed = sockets.close_user(user_id, "voice disabled");
        if audit {
            self.inner
                .audit(
                    user_id,
                    None,
                    "kill_switch",
                    json!({"endedCalls": ended, "closedSockets": closed}),
                )
                .await;
            tracing::info!(user_id, ended, closed, "voice kill switch applied");
        }
    }

    /// Settings as returned by the API, with today's spend.
    pub async fn settings_response(
        &self,
        user_id: &str,
        settings: &VoiceSettings,
    ) -> Result<VoiceSettingsResponse, VoiceError> {
        let used = self
            .inner
            .sessions
            .cost_since(user_id, utc_day_start_ms(aionui_common::now_ms()))
            .await?;
        Ok(settings.to_response(Some(used)))
    }

    pub async fn is_enabled(&self, user_id: &str) -> bool {
        self.settings(user_id).await.map(|s| s.enabled).unwrap_or(false)
    }

    // ----------------------------------------------------------------- status

    pub async fn status(&self, user_id: &str) -> Result<VoiceStatusResponse, VoiceError> {
        let settings = self.settings(user_id).await?;
        let used = self
            .inner
            .sessions
            .cost_since(user_id, utc_day_start_ms(aionui_common::now_ms()))
            .await?;
        let active = self.live_call_for(user_id).map(|c| c.session_id.clone());
        Ok(VoiceStatusResponse {
            enabled: settings.enabled,
            live_calls: u32::from(active.is_some()),
            active_session_id: active,
            used_today_usd: micros_to_usd(used),
            limit_usd: micros_to_usd(settings.daily_budget_micros),
        })
    }

    pub async fn list_sessions(&self, user_id: &str) -> Result<Vec<VoiceSessionResponse>, VoiceError> {
        let rows = self.inner.sessions.list(user_id, LIST_LIMIT).await?;
        Ok(rows.into_iter().map(session_response).collect())
    }

    // -------------------------------------------------------------- admission

    /// `POST /api/voice/sessions` (§3.6.2): admission → provider call → SDP
    /// answer at once; the runner attaches the sideband in parallel.
    ///
    /// `token` is the caller's session cookie; the call re-validates it every 60 s.
    pub async fn create_session(
        &self,
        user_id: &str,
        token: Option<String>,
        req: CreateVoiceSessionRequest,
    ) -> Result<CreateVoiceSessionResponse, VoiceError> {
        validate_create(&req)?;
        let _admission = self.inner.admission.lock().await;

        let settings = self.settings(user_id).await?;
        if !settings.enabled {
            return Err(VoiceError::Disabled);
        }
        if let Some(engine) = req.engine.as_deref()
            && engine != settings.engine
        {
            return Err(VoiceError::BadRequest(format!(
                "engine '{engine}' is not the configured voice engine"
            )));
        }
        if let Some(previous) = req.resume_of.as_deref()
            && self.live_session_belongs(user_id, previous)
        {
            self.end_live(previous, EndReason::Resumed).await;
        }

        let profile = settings.call_profile();
        let tools: Vec<Value> = Vec::new();
        let used = self
            .inner
            .sessions
            .cost_since(user_id, utc_day_start_ms(aionui_common::now_ms()))
            .await?;
        let meter = BudgetMeter::new(
            pricing_for(&profile.model),
            settings.daily_budget_micros,
            used,
            estimate_prefix_tokens(BASE_INSTRUCTIONS, &tools),
        );
        if !meter.can_afford_response() {
            self.inner
                .audit(user_id, None, "budget_refused", json!({"usedMicros": used}))
                .await;
            return Err(VoiceError::BudgetReached);
        }

        let credential = self.resolve_credential(user_id, &settings).await?;
        let config = EngineSessionConfig {
            model: profile.model.clone(),
            instructions: BASE_INSTRUCTIONS.to_owned(),
            voice: profile.voice.clone(),
            eagerness: profile.eagerness.clone(),
            noise_reduction: profile.noise_reduction.clone(),
            transcription_model: TRANSCRIPTION_MODEL.to_owned(),
            keywords: Vec::new(),
            reasoning_effort: "low".to_owned(),
            max_output_tokens: VOICE_MAX_OUTPUT_TOKENS,
            create_response: true,
            tools: tools.clone(),
        };
        let hash = instructions_hash(&config.instructions, &tools);
        let session_id = aionui_common::generate_prefixed_id("vs");
        let engine_kind = self.inner.engine.kind();

        self.inner
            .sessions
            .create_starting(CreateVoiceSessionParams {
                id: &session_id,
                user_id,
                engine: engine_kind,
                model: &profile.model,
                resume_of: req.resume_of.as_deref(),
                device: req.device_id.as_deref(),
                instructions_hash: Some(&hash),
            })
            .await
            .map_err(|err| match err {
                DbError::Conflict(_) => VoiceError::Busy,
                other => other.into(),
            })?;

        let opened = self
            .inner
            .engine
            .open(OpenCallRequest {
                credential,
                sdp_offer: req.sdp_offer,
                config,
            })
            .await;
        let (answer, engine_call) = match opened {
            Ok(opened) => opened,
            Err(err) => {
                let _ = self
                    .inner
                    .sessions
                    .finish(user_id, &session_id, "failed", EndReason::CreateFailed.as_str())
                    .await;
                self.inner
                    .audit(
                        user_id,
                        Some(&session_id),
                        "session_failed",
                        json!({"reason": "create_failed"}),
                    )
                    .await;
                tracing::warn!(user_id, session_id = %session_id, error = %err, "voice call creation failed");
                return Err(VoiceError::Provider(err.to_string()));
            }
        };
        let call_id = engine_call.call_id().to_owned();
        self.inner.sessions.mark_live(user_id, &session_id, &call_id).await?;
        self.inner
            .audit(
                user_id,
                Some(&session_id),
                "session_created",
                json!({"engine": engine_kind, "model": profile.model, "resumeOf": req.resume_of}),
            )
            .await;
        tracing::info!(user_id, session_id = %session_id, engine = engine_kind, model = %profile.model,
            "voice call created");

        self.spawn_runner(
            user_id,
            &session_id,
            &profile.model,
            engine_call,
            meter,
            token,
            RunMode::Fresh,
        );
        Ok(CreateVoiceSessionResponse {
            voice_session_id: session_id,
            sdp_answer: answer,
            engine: engine_kind.to_owned(),
            model: profile.model,
        })
    }

    async fn resolve_credential(
        &self,
        user_id: &str,
        settings: &VoiceSettings,
    ) -> Result<ResolvedCredential, VoiceError> {
        let credentials = self
            .inner
            .credentials
            .provider_credentials(user_id)
            .await
            .map_err(VoiceError::Internal)?;
        self.inner
            .credential_policy
            .resolve_openai(credentials, settings.provider_id.as_deref())
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_runner(
        &self,
        user_id: &str,
        session_id: &str,
        model: &str,
        engine_call: Box<dyn crate::engine::EngineCall>,
        meter: BudgetMeter,
        token: Option<String>,
        mode: RunMode,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let call = Arc::new(LiveCall {
            session_id: session_id.to_owned(),
            user_id: user_id.to_owned(),
            engine: self.inner.engine.kind().to_owned(),
            model: model.to_owned(),
            cmd_tx,
            ready: AtomicBool::new(false),
            liveness: std::sync::Mutex::new(Liveness {
                last_heartbeat: Instant::now(),
                frozen: false,
            }),
        });
        self.inner.live.insert(session_id.to_owned(), call.clone());
        let runner = CallRunner::new(self.inner.clone(), call, engine_call, cmd_rx, meter, token, mode);
        let handle = tokio::spawn(runner.run());
        self.inner.tasks.insert(session_id.to_owned(), handle.abort_handle());
    }

    /// Test seam: stop every runner without hangup or persistence, as if the
    /// process had crashed (restart-recovery tests).
    #[cfg(feature = "voice-test")]
    pub fn simulate_crash(&self) {
        for entry in self.inner.tasks.iter() {
            entry.value().abort();
        }
        self.inner.tasks.clear();
        self.inner.live.clear();
    }

    // -------------------------------------------------------------------- end

    /// `POST /api/voice/sessions/{id}/end`.
    pub async fn end_session(
        &self,
        user_id: &str,
        session_id: &str,
        reason: EndReason,
    ) -> Result<VoiceSessionResponse, VoiceError> {
        let row = self
            .inner
            .sessions
            .get(user_id, session_id)
            .await?
            .ok_or_else(|| VoiceError::NotFound(format!("voice session '{session_id}' not found")))?;
        if self.live_session_belongs(user_id, session_id) {
            self.end_live(session_id, reason).await;
        } else if (row.state == "starting" || row.state == "live")
            && self
                .inner
                .sessions
                .finish(user_id, session_id, reason.terminal_state(), reason.as_str())
                .await?
        {
            // No runner (e.g. it crashed): close the record and tell the client.
            self.inner
                .sockets
                .send_user(user_id, &call_ended_frame(session_id, reason.as_str()));
        }
        let row = self
            .inner
            .sessions
            .get(user_id, session_id)
            .await?
            .ok_or_else(|| VoiceError::NotFound(format!("voice session '{session_id}' not found")))?;
        Ok(session_response(row))
    }

    /// End every live call of the user; returns how many were ended.
    pub async fn end_user_calls(&self, user_id: &str, reason: EndReason) -> usize {
        let ids: Vec<String> = self
            .inner
            .live
            .iter()
            .filter(|c| c.user_id == user_id)
            .map(|c| c.session_id.clone())
            .collect();
        for id in &ids {
            self.end_live(id, reason).await;
        }
        ids.len()
    }

    /// Ask a runner to end and wait until it hung up and persisted.
    async fn end_live(&self, session_id: &str, reason: EndReason) {
        let Some(call) = self.inner.live.get(session_id).map(|c| c.clone()) else {
            return;
        };
        let (done_tx, done_rx) = oneshot::channel();
        if call
            .cmd_tx
            .send(CallCommand::End { reason, done: done_tx })
            .await
            .is_ok()
        {
            let _ = done_rx.await;
        }
    }

    // ------------------------------------------------- socket-facing helpers

    fn live_call_for(&self, user_id: &str) -> Option<Arc<LiveCall>> {
        self.inner
            .live
            .iter()
            .find(|c| c.user_id == user_id)
            .map(|c| c.value().clone())
    }

    fn live_session_belongs(&self, user_id: &str, session_id: &str) -> bool {
        self.inner.live.get(session_id).is_some_and(|c| c.user_id == user_id)
    }

    /// Whether `session_id` is one of the user's sessions (per-frame check).
    pub async fn session_belongs(&self, user_id: &str, session_id: &str) -> bool {
        if self.live_session_belongs(user_id, session_id) {
            return true;
        }
        matches!(self.inner.sessions.get(user_id, session_id).await, Ok(Some(_)))
    }

    /// `(engine, model)` when the user's call is live and its config was acknowledged.
    pub fn ready_call(&self, user_id: &str, session_id: &str) -> Option<(String, String)> {
        let call = self.inner.live.get(session_id)?;
        (call.user_id == user_id && call.ready.load(Ordering::SeqCst))
            .then(|| (call.engine.clone(), call.model.clone()))
    }

    pub fn note_heartbeat(&self, user_id: &str, session_id: Option<&str>) {
        self.for_user_calls(user_id, session_id, |call| call.heartbeat());
    }

    pub fn note_frozen(&self, user_id: &str, session_id: Option<&str>, frozen: bool) {
        self.for_user_calls(user_id, session_id, |call| call.set_frozen(frozen));
    }

    fn for_user_calls(&self, user_id: &str, session_id: Option<&str>, f: impl Fn(&LiveCall)) {
        for call in self.inner.live.iter() {
            let matches_session = session_id.is_none_or(|id| id == call.session_id);
            if call.user_id == user_id && matches_session {
                f(&call);
            }
        }
    }

    pub async fn verify_token(&self, token: &str) -> Result<VerifiedUser, AuthRejected> {
        self.inner.verifier.verify(token).await
    }

    // ------------------------------------------------ S1b hooks (toggling)

    /// Switch provider-initiated responses on/off (drafts, `ask`, clips — §3.1).
    pub async fn set_create_response(&self, user_id: &str, session_id: &str, enabled: bool) -> Result<(), VoiceError> {
        let call = self.owned_live(user_id, session_id)?;
        let (done, rx) = oneshot::channel();
        call.cmd_tx
            .send(CallCommand::SetCreateResponse { enabled, done })
            .await
            .map_err(|_| VoiceError::NotFound("voice call ended".into()))?;
        rx.await.map_err(|_| VoiceError::NotFound("voice call ended".into()))?
    }

    /// Backend-issued `response.create` with a budget reservation (§3.5).
    pub async fn request_response(&self, user_id: &str, session_id: &str) -> Result<(), VoiceError> {
        let call = self.owned_live(user_id, session_id)?;
        let (done, rx) = oneshot::channel();
        call.cmd_tx
            .send(CallCommand::CreateResponse { done })
            .await
            .map_err(|_| VoiceError::NotFound("voice call ended".into()))?;
        rx.await.map_err(|_| VoiceError::NotFound("voice call ended".into()))?
    }

    fn owned_live(&self, user_id: &str, session_id: &str) -> Result<Arc<LiveCall>, VoiceError> {
        self.inner
            .live
            .get(session_id)
            .filter(|c| c.user_id == user_id)
            .map(|c| c.value().clone())
            .ok_or_else(|| VoiceError::NotFound(format!("live voice session '{session_id}' not found")))
    }

    // --------------------------------------------------------------- recovery

    /// Startup: re-attach a sideband to every call persisted as live, or close
    /// it (§3.6.4, §6 "Deploy restarts AionUi mid-call"). Returns how many
    /// calls were handed to a runner.
    pub async fn recover_open_sessions(&self) -> usize {
        let rows = match self.inner.sessions.list_open().await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(error = %err, "voice recovery: listing open sessions failed");
                return 0;
            }
        };
        let mut resumed = 0;
        for row in rows {
            if self.inner.live.contains_key(&row.id) {
                continue;
            }
            match self.recover_one(&row).await {
                Ok(()) => resumed += 1,
                Err(reason) => {
                    let _ = self
                        .inner
                        .sessions
                        .finish(&row.user_id, &row.id, reason.terminal_state(), reason.as_str())
                        .await;
                    self.inner
                        .sockets
                        .send_user(&row.user_id, &call_ended_frame(&row.id, reason.as_str()));
                    self.inner
                        .audit(
                            &row.user_id,
                            Some(&row.id),
                            "session_ended",
                            json!({"reason": reason.as_str()}),
                        )
                        .await;
                    tracing::info!(session_id = %row.id, reason = reason.as_str(), "voice recovery closed session");
                }
            }
        }
        resumed
    }

    async fn recover_one(&self, row: &VoiceSessionRow) -> Result<(), EndReason> {
        let call_id = row
            .call_id
            .clone()
            .filter(|_| row.state == "live")
            .ok_or(EndReason::Restart)?;
        let settings = self.settings(&row.user_id).await.map_err(|_| EndReason::Restart)?;
        let credential = self
            .resolve_credential(&row.user_id, &settings)
            .await
            .map_err(|_| EndReason::Restart)?;
        let profile = settings.call_profile();
        let tools: Vec<Value> = Vec::new();
        let config = EngineSessionConfig {
            model: row.model.clone(),
            instructions: BASE_INSTRUCTIONS.to_owned(),
            voice: profile.voice,
            eagerness: profile.eagerness,
            noise_reduction: profile.noise_reduction,
            transcription_model: TRANSCRIPTION_MODEL.to_owned(),
            keywords: Vec::new(),
            reasoning_effort: "low".to_owned(),
            max_output_tokens: VOICE_MAX_OUTPUT_TOKENS,
            create_response: true,
            tools: tools.clone(),
        };
        let mut engine_call = self.inner.engine.resume(credential, call_id, config);
        if !settings.enabled {
            let _ = engine_call.close(EndReason::Disabled.as_str()).await;
            return Err(EndReason::Disabled);
        }
        let used = self
            .inner
            .sessions
            .cost_since(&row.user_id, utc_day_start_ms(aionui_common::now_ms()))
            .await
            .unwrap_or(0);
        // This session's own cost is in `used`; the meter tracks it as base.
        let meter = BudgetMeter::new(
            pricing_for(&row.model),
            settings.daily_budget_micros,
            used,
            estimate_prefix_tokens(BASE_INSTRUCTIONS, &tools),
        );
        self.spawn_runner(
            &row.user_id,
            &row.id,
            &row.model,
            engine_call,
            meter,
            None,
            RunMode::Recovered,
        );
        Ok(())
    }
}

fn validate_create(req: &CreateVoiceSessionRequest) -> Result<(), VoiceError> {
    if req.sdp_offer.len() > MAX_SDP_BYTES {
        return Err(VoiceError::BadRequest(format!(
            "sdp_offer exceeds {MAX_SDP_BYTES} bytes"
        )));
    }
    if !req.sdp_offer.trim_start().starts_with("v=0") {
        return Err(VoiceError::BadRequest("sdp_offer must be an SDP offer".into()));
    }
    let short_printable = |value: &Option<String>, max: usize| {
        value
            .as_deref()
            .is_none_or(|v| v.len() <= max && !v.chars().any(char::is_control))
    };
    if !short_printable(&req.device_id, 64) {
        return Err(VoiceError::BadRequest(
            "device_id must be at most 64 printable characters".into(),
        ));
    }
    if !short_printable(&req.resume_of, 128) || !short_printable(&req.focus_conversation_id, 128) {
        return Err(VoiceError::BadRequest(
            "resume_of / focus_conversation_id are too long".into(),
        ));
    }
    if !short_printable(&req.engine, 32) {
        return Err(VoiceError::BadRequest("engine is invalid".into()));
    }
    Ok(())
}

pub(crate) fn session_response(row: VoiceSessionRow) -> VoiceSessionResponse {
    VoiceSessionResponse {
        id: row.id,
        engine: row.engine,
        model: row.model,
        state: row.state,
        end_reason: row.end_reason,
        created_at: row.created_at,
        ready_at: row.ready_at,
        ended_at: row.ended_at,
        input_tokens: row.input_tokens,
        cached_input_tokens: row.cached_input_tokens,
        output_tokens: row.output_tokens,
        responses: row.responses,
        unknown_responses: row.unknown_responses,
        cost_usd: micros_to_usd(row.cost_micros),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(sdp: &str) -> CreateVoiceSessionRequest {
        CreateVoiceSessionRequest {
            engine: None,
            sdp_offer: sdp.into(),
            device_id: None,
            focus_conversation_id: None,
            resume_of: None,
        }
    }

    #[test]
    fn create_request_validation() {
        assert!(validate_create(&req("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\n")).is_ok());
        assert!(matches!(validate_create(&req("hello")), Err(VoiceError::BadRequest(_))));
        let huge = format!("v=0\r\n{}", "a".repeat(MAX_SDP_BYTES));
        assert!(matches!(validate_create(&req(&huge)), Err(VoiceError::BadRequest(_))));
        let mut bad_device = req("v=0");
        bad_device.device_id = Some("x\u{0}".into());
        assert!(matches!(validate_create(&bad_device), Err(VoiceError::BadRequest(_))));
    }
}
