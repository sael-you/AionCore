//! Voice business logic: admission, settings + kill switch, status, session
//! end, restart recovery. Per-call work runs in [`crate::runner`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aionui_api_types::{UpdateVoiceSettingsRequest, VoiceSessionResponse, VoiceSettingsResponse, VoiceStatusResponse};
use aionui_db::{
    IVoiceAuditRepository, IVoiceSessionRepository, IVoiceSettingsRepository, NewVoiceAuditEntry, VoiceSessionRow,
};
use dashmap::DashMap;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;

use crate::budget::utc_day_start_ms;
use crate::credentials::CredentialPolicy;
use crate::engine::VoiceEngine;
use crate::error::VoiceError;
use crate::frames::{VoiceSockets, call_ended_frame, error_frame};
use crate::lifecycle::{EndReason, VoiceTimings};
use crate::ports::{AuthRejected, AuthVerifier, CredentialPort, VerifiedUser};
use crate::runner::CallCommand;
use crate::settings::{VoiceSettings, micros_to_usd};

const LIST_LIMIT: u32 = 50;

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
    /// Session cookie the call re-validates every 60 s: the creating request's,
    /// or (recovered calls) the first socket's that binds the session.
    pub token: std::sync::Mutex<Option<String>>,
}

impl LiveCall {
    pub(crate) fn token(&self) -> Option<String> {
        self.token.lock().ok().and_then(|t| t.clone())
    }

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
    /// Per-user admission lock: an admission, a resume and the kill switch of
    /// one user never interleave; other users are not blocked.
    admission: DashMap<String, Arc<Mutex<()>>>,
    /// Set when the HTTP listener is serving; device liveness of recovered
    /// calls is measured from here, not from process start.
    listening_since: std::sync::Mutex<Option<Instant>>,
}

impl ServiceInner {
    pub(crate) fn user_lock(&self, user_id: &str) -> Arc<Mutex<()>> {
        self.admission
            .entry(user_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub(crate) fn mark_listening(&self) {
        if let Ok(mut since) = self.listening_since.lock()
            && since.is_none()
        {
            *since = Some(Instant::now());
        }
    }

    pub(crate) fn listening_since(&self) -> Option<Instant> {
        self.listening_since.lock().ok().and_then(|s| *s)
    }

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
    pub(crate) inner: Arc<ServiceInner>,
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
                admission: DashMap::new(),
                listening_since: std::sync::Mutex::new(None),
            }),
        }
    }

    pub fn sockets(&self) -> &VoiceSockets {
        &self.inner.sockets
    }

    pub fn timings(&self) -> VoiceTimings {
        self.inner.timings
    }

    /// The HTTP listener is up (called by the app; any voice request implies it).
    pub fn mark_listening(&self) {
        self.inner.mark_listening();
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

    /// Kill switch for one user: hang up, notify, close sockets. Holds the
    /// user's admission lock so an admission in flight finishes first (and
    /// then sees voice off and hangs up its call itself).
    async fn kill_user(&self, user_id: &str, audit: bool) {
        let lock = self.inner.user_lock(user_id);
        let _guard = lock.lock().await;
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

    /// A socket's session cookie failed re-validation: end only the calls
    /// that run on that cookie or that this socket is bound to.
    pub async fn end_calls_for_socket(&self, user_id: &str, token: Option<&str>, bound_session: Option<&str>) -> usize {
        let ids: Vec<String> = self
            .inner
            .live
            .iter()
            .filter(|c| c.user_id == user_id)
            .filter(|c| {
                let same_token = token.is_some() && c.token().as_deref() == token;
                same_token || bound_session == Some(c.session_id.as_str())
            })
            .map(|c| c.session_id.clone())
            .collect();
        for id in &ids {
            self.end_live(id, EndReason::AuthRevoked).await;
        }
        ids.len()
    }

    /// A socket bound the session: a call without a cookie (recovered after a
    /// restart) adopts the socket's for its 60 s re-validation.
    pub fn adopt_token(&self, user_id: &str, session_id: &str, token: &str) {
        if let Some(call) = self.inner.live.get(session_id)
            && call.user_id == user_id
            && let Ok(mut slot) = call.token.lock()
            && slot.is_none()
        {
            *slot = Some(token.to_owned());
        }
    }

    /// Ask a runner to end and wait until it hung up and persisted.
    pub(crate) async fn end_live(&self, session_id: &str, reason: EndReason) {
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

    pub(crate) fn live_session_belongs(&self, user_id: &str, session_id: &str) -> bool {
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
