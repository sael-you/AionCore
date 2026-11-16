//! Voice business logic: admission, settings + kill switch, status, session
//! end, restart recovery. Per-call work runs in [`crate::runner`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aionui_api_types::{
    UpdateVoiceSettingsRequest, VoiceServerFrame, VoiceSessionResponse, VoiceSettingsResponse, VoiceStatusResponse,
};
use aionui_db::{
    IVoiceAuditRepository, IVoiceDraftRepository, IVoiceSessionRepository, IVoiceSettingsRepository,
    NewVoiceAuditEntry, VoiceSessionRow,
};
use aionui_realtime::BroadcastEventBus;
use dashmap::DashMap;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;

use crate::agent::AskRegistry;
use crate::budget::utc_day_start_ms;
use crate::conversation_mode::SessionMode;
use crate::conversation_port::ConversationPort;
use crate::credentials::CredentialPolicy;
use crate::drafts::{DraftEngine, SpeechFactory};
use crate::engine::VoiceEngine;
use crate::error::VoiceError;
use crate::frames::{VoiceSockets, call_ended_frame, error_frame};
use crate::lifecycle::{EndReason, VoiceTimings};
use crate::ports::{AuthRejected, AuthVerifier, ChannelPort, CredentialPort, SkillPort, VerifiedUser};
use crate::readback::{Clip, ClipStore, ClipVoice};
use crate::runner::CallCommand;
use crate::runner_drafts::ClientEvent;
use crate::settings::{VoiceSettings, micros_to_usd};
use crate::tools::ToolRunner;

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
    /// S1b: conversations (tools, targets, sends), drafts storage, speech.
    pub conversations: Arc<dyn ConversationPort>,
    pub drafts: Arc<dyn IVoiceDraftRepository>,
    pub speech: Arc<dyn SpeechFactory>,
    /// Text fallback for `say` / `ask` with no device (TASK-37).
    pub channels: Arc<dyn ChannelPort>,
    /// Source of the `cs-voice-concierge` skill (TASK-37, §3.7).
    pub skills: Arc<dyn SkillPort>,
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
    /// Conversation the user armed voice in (target when no name is said);
    /// in conversation mode the bound conversation.
    pub focus: Option<String>,
    pub mode: SessionMode,
    /// Car mode at admission (shorter agent lines, `[[AION_VOICE]]` mode).
    pub car: bool,
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

/// The conversation a user's conversation voice mode is bound to (C26). Kept
/// in memory only (no migration): a restart drops it and the client re-opens
/// the session with its binding on the next wake or tap.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    pub conversation_id: String,
    pub session_id: String,
    /// `None` while its call is live; after an idle/rotation hangup the mode
    /// is still armed on the device: the binding holds while a voice socket of
    /// the user is connected, and for `binding_grace` after.
    pub until: Option<Instant>,
}

pub(crate) struct ServiceInner {
    pub settings: Arc<dyn IVoiceSettingsRepository>,
    pub sessions: Arc<dyn IVoiceSessionRepository>,
    pub audit: Arc<dyn IVoiceAuditRepository>,
    pub credentials: Arc<dyn CredentialPort>,
    pub credential_policy: CredentialPolicy,
    pub engine: Arc<dyn VoiceEngine>,
    /// `minimax-cascade`, built over [`Self::engine`] as its ear.
    pub cascade: Arc<crate::engine::MiniMaxCascadeEngine>,
    pub verifier: Arc<dyn AuthVerifier>,
    pub sockets: VoiceSockets,
    pub timings: VoiceTimings,
    pub drafts: Arc<DraftEngine>,
    pub tools: ToolRunner,
    pub asks: AskRegistry,
    pub say_limiter: aionui_auth::RateLimiter,
    /// `voice.wake` per user per hour, shared with the follow-up watcher.
    pub wake_limiter: Arc<aionui_auth::RateLimiter>,
    pub live: DashMap<String, Arc<LiveCall>>,
    /// user id → conversation voice-mode binding (C26).
    pub bindings: DashMap<String, Binding>,
    /// user id → bound conversation whose turn a restart interrupted; spoken
    /// once when the device says hello again (C26 §3.4).
    pub interrupted: DashMap<String, String>,
    /// conversation id → its serial speech queue: the bound agent's lines are
    /// synthesized and delivered in the order they were said.
    pub speech_queues: DashMap<String, mpsc::UnboundedSender<crate::agent_service::BoundLine>>,
    pub tasks: DashMap<String, tokio::task::AbortHandle>,
    pub channels: Arc<dyn ChannelPort>,
    pub skills: Arc<dyn SkillPort>,
    /// Per-user admission lock: an admission, a resume and the kill switch of
    /// one user never interleave; other users are not blocked.
    admission: DashMap<String, Arc<Mutex<()>>>,
    /// Per-user settings lock: settings read-apply-writes of one user never
    /// interleave. Taken before the admission lock, never while holding it.
    settings_locks: DashMap<String, Arc<Mutex<()>>>,
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

    fn settings_lock(&self, user_id: &str) -> Arc<Mutex<()>> {
        self.settings_locks
            .entry(user_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// The conversation the user's voice mode is bound to, if still valid:
    /// while its call is live, while the armed device's voice socket is
    /// connected (renewing the grace), or within the grace after.
    pub(crate) fn bound_conversation(&self, user_id: &str) -> Option<String> {
        let mut binding = self.bindings.get_mut(user_id)?;
        let Some(until) = binding.until else {
            return Some(binding.conversation_id.clone());
        };
        // Only a device still armed in conversation mode (or an older client
        // that does not say) renews it: a Jarvis-mode socket never does.
        if self.sockets.conversation_capable_count(user_id) > 0 {
            binding.until = Some(Instant::now() + self.timings.binding_grace);
            return Some(binding.conversation_id.clone());
        }
        (until > Instant::now()).then(|| binding.conversation_id.clone())
    }

    /// Daily budget left for spoken clips: limit minus today's spend.
    pub(crate) async fn tts_remaining(&self, user_id: &str) -> i64 {
        let limit = match self.settings.get(user_id).await {
            Ok(row) => row.map(VoiceSettings::from_row).unwrap_or_default().daily_budget_micros,
            Err(_) => return 0,
        };
        let used = self
            .sessions
            .cost_since(user_id, utc_day_start_ms(aionui_common::now_ms()))
            .await
            .unwrap_or(i64::MAX);
        limit.saturating_sub(used)
    }

    /// A call ended: the user ended voice mode (drop the binding) or the
    /// call merely went away (keep it for [`BINDING_GRACE`]).
    pub(crate) fn release_binding(&self, user_id: &str, session_id: &str, reason: EndReason) {
        let ends_mode = matches!(
            reason,
            EndReason::User | EndReason::Sleep | EndReason::Disabled | EndReason::AuthRevoked | EndReason::Budget
        );
        // Every connected device is armed in Jarvis mode (the call ran on a
        // device that switched or left): nothing keeps conversation mode armed.
        let only_jarvis =
            self.sockets.count_for_user(user_id) > 0 && self.sockets.conversation_capable_count(user_id) == 0;
        if ends_mode || only_jarvis {
            self.bindings
                .remove_if(user_id, |_, binding| binding.session_id == session_id);
        } else if let Some(mut binding) = self.bindings.get_mut(user_id)
            && binding.session_id == session_id
        {
            binding.until = Some(Instant::now() + self.timings.binding_grace);
        }
    }

    /// The device was re-armed in Jarvis mode: the user left conversation
    /// voice mode, so a binding kept for an idle/hung-up call ends now
    /// instead of being renewed by the Jarvis socket. A live
    /// conversation-mode call (another device) keeps its binding; it ends
    /// with that call or when a Jarvis call is admitted.
    pub(crate) fn leave_conversation_mode(&self, user_id: &str) -> bool {
        let removed = self
            .bindings
            .remove_if(user_id, |_, binding| binding.until.is_some())
            .is_some();
        if removed {
            tracing::info!(user_id, "voice conversation binding cleared: device switched to Jarvis");
        }
        removed
    }

    /// Jarvis's session instructions: the user's `cs-voice-concierge` skill
    /// compiled (§3.7), else the built-in ones.
    pub(crate) async fn jarvis_instructions(&self, user_id: &str) -> String {
        use crate::instructions::{BASE_INSTRUCTIONS, CONCIERGE_SKILL, compile_concierge_skill};
        let Some(markdown) = self.skills.skill_markdown(user_id, CONCIERGE_SKILL).await else {
            return BASE_INSTRUCTIONS.to_owned();
        };
        match compile_concierge_skill(&markdown) {
            Ok(compiled) => {
                tracing::info!(
                    user_id,
                    chars = compiled.chars().count(),
                    "voice instructions compiled from the concierge skill"
                );
                compiled
            }
            Err(err) => {
                tracing::warn!(user_id, error = ?err, "voice concierge skill not usable; built-in instructions");
                BASE_INSTRUCTIONS.to_owned()
            }
        }
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
        let service = Self::build(deps);
        let hook = TtsBudgetHook {
            inner: Arc::downgrade(&service.inner),
        };
        let _ = service.inner.drafts.budget.set(Arc::new(hook));
        service
    }

    fn build(deps: VoiceServiceDeps) -> Self {
        let drafts = Arc::new(DraftEngine::new(
            deps.drafts,
            deps.conversations.clone(),
            deps.speech,
            ClipStore::default(),
            deps.sockets.clone(),
            deps.sessions.clone(),
        ));
        Self {
            inner: Arc::new(ServiceInner {
                drafts,
                tools: ToolRunner {
                    conversations: deps.conversations,
                },
                asks: AskRegistry::default(),
                say_limiter: aionui_auth::RateLimiter::new(
                    crate::agent::SAY_PER_MINUTE,
                    std::time::Duration::from_secs(60),
                ),
                wake_limiter: crate::followup::wake_limiter(),
                settings: deps.settings,
                sessions: deps.sessions,
                audit: deps.audit,
                credentials: deps.credentials,
                credential_policy: deps.credential_policy,
                cascade: Arc::new(crate::engine::MiniMaxCascadeEngine::new(deps.engine.clone())),
                engine: deps.engine,
                verifier: deps.verifier,
                sockets: deps.sockets,
                timings: deps.timings,
                live: DashMap::new(),
                bindings: DashMap::new(),
                interrupted: DashMap::new(),
                speech_queues: DashMap::new(),
                tasks: DashMap::new(),
                channels: deps.channels,
                skills: deps.skills,
                admission: DashMap::new(),
                settings_locks: DashMap::new(),
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
        // Read-apply-write under the user's settings lock: a concurrent update
        // that read `enabled: true` can never land after the kill switch. Not
        // the admission lock: `enabled: false` must be written while an
        // admission waits on the provider, so its re-check sees it.
        let lock = self.inner.settings_lock(user_id);
        let _guard = lock.lock().await;
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

    /// Kill switch for one user: hang up, notify, close sockets. Runs after
    /// `enabled: false` is written and holds the user's admission lock, so an
    /// admission in flight finishes first (and then sees voice off and hangs
    /// up its call itself).
    async fn kill_user(&self, user_id: &str, audit: bool) {
        let lock = self.inner.user_lock(user_id);
        let _guard = lock.lock().await;
        let ended = self.end_user_calls(user_id, EndReason::Disabled).await;
        self.inner.bindings.remove(user_id);
        let cancelled_drafts = self.inner.drafts.cancel_all_for_user(user_id).await;
        self.inner.asks.cancel_user(user_id);
        let sockets = &self.inner.sockets;
        sockets.send_user(user_id, &error_frame(None, "VOICE_DISABLED", "Voice is turned off."));
        let closed = sockets.close_user(user_id, "voice disabled");
        if audit {
            self.inner
                .audit(
                    user_id,
                    None,
                    "kill_switch",
                    json!({"endedCalls": ended, "closedSockets": closed, "cancelledDrafts": cancelled_drafts}),
                )
                .await;
            tracing::info!(user_id, ended, closed, cancelled_drafts, "voice kill switch applied");
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
        let now = aionui_common::now_ms();
        let (calls, connected) = self
            .inner
            .sessions
            .calls_since(user_id, utc_day_start_ms(now), now)
            .await?;
        let mut response = settings.to_response(Some(used));
        response.calls_today = Some(calls);
        response.connected_seconds_today = Some(connected);
        Ok(response)
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

    /// A client frame about drafts / clips. With a live call it goes to the
    /// call's runner (evidence, hold); taps on a card without a live call act
    /// on the draft directly (a confirmed hold is then sent by the worker).
    pub(crate) async fn client_event(&self, user_id: &str, session_id: Option<&str>, event: ClientEvent) {
        let call = session_id
            .and_then(|id| self.inner.live.get(id).map(|c| c.clone()))
            .filter(|c| c.user_id == user_id);
        if let Some(call) = call {
            let _ = call.cmd_tx.send(CallCommand::Client(event)).await;
            return;
        }
        let drafts = &self.inner.drafts;
        match event {
            ClientEvent::ConfirmTap {
                draft_id,
                version,
                hash,
            } => {
                if let Ok(Some(row)) = drafts.repo.get_draft(user_id, &draft_id, version).await {
                    drafts.confirm(&row, Some(&hash)).await;
                }
            }
            ClientEvent::CancelTap { draft_id, version } => {
                if let Ok(Some(row)) = drafts.repo.get_draft(user_id, &draft_id, version).await {
                    drafts.cancel(&row).await;
                }
            }
            _ => {}
        }
    }

    /// A device said hello armed in `mode`: Jarvis mode ends a conversation
    /// binding whose call is gone (C26 mode switch). Unknown or absent
    /// modes change nothing.
    pub fn note_hello_mode(&self, user_id: &str, conn_id: aionui_realtime::ConnectionId, mode: Option<&str>) {
        let mode = mode.map(str::trim).filter(|m| !m.is_empty());
        let jarvis = matches!(mode.map(|m| SessionMode::parse(Some(m))), Some(Ok(SessionMode::Jarvis)));
        self.inner.sockets.set_jarvis(conn_id, user_id, jarvis);
        if jarvis {
            self.inner.leave_conversation_mode(user_id);
        }
    }

    /// A device said hello: if a restart interrupted the turn of the user's
    /// bound conversation, say so once (a clip through the normal TTS path).
    pub fn speak_interrupted(&self, user_id: &str) {
        let Some((_, conversation_id)) = self.inner.interrupted.remove(user_id) else {
            return;
        };
        let drafts = self.inner.drafts.clone();
        let user_id = user_id.to_owned();
        tokio::spawn(async move {
            drafts
                .speak_line(
                    &user_id,
                    None,
                    &conversation_id,
                    "interrupted",
                    crate::conversation_mode::INTERRUPTED_LINE,
                    ClipVoice::Agent,
                )
                .await;
        });
    }

    /// The user's pending draft card, replayed on `voice.hello`.
    pub async fn pending_draft_frames(&self, user_id: &str) -> Vec<VoiceServerFrame> {
        self.inner.drafts.replay_for(user_id).await
    }

    /// `GET /api/voice/readbacks/{id}`: single use, owner only, 2 min.
    pub fn take_clip(&self, user_id: &str, clip_id: &str) -> Option<Clip> {
        self.inner.drafts.clips.take(user_id, clip_id)
    }

    pub(crate) fn has_live_call(&self, user_id: &str) -> bool {
        self.inner.live.iter().any(|c| c.user_id == user_id)
    }

    /// Start the draft worker and the follow-up watcher (called once by the app).
    pub fn start_background(&self, bus: Arc<BroadcastEventBus>) {
        crate::drafts::DraftWorker {
            engine: self.inner.drafts.clone(),
            live: Arc::new(self.clone()),
            owner: aionui_common::generate_prefixed_id("vw"),
            started_at_ms: aionui_common::now_ms(),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
        .spawn();
        crate::followup::FollowUpWatcher::new(
            bus,
            self.inner.drafts.clone(),
            self.inner.sockets.clone(),
            Arc::new(self.clone()),
            self.inner.wake_limiter.clone(),
        )
        .spawn();
    }

    /// Test seam: one draft-worker pass as if this process started at
    /// `started_at_ms` (recovery tests).
    #[cfg(feature = "voice-test")]
    pub async fn run_draft_sweep(&self, recovery: bool, started_at_ms: i64) {
        crate::drafts::DraftWorker {
            engine: self.inner.drafts.clone(),
            live: Arc::new(self.clone()),
            owner: aionui_common::generate_prefixed_id("vw"),
            started_at_ms,
            wake: Arc::new(tokio::sync::Notify::new()),
        }
        .sweep(recovery)
        .await;
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

impl crate::drafts::LiveSessions for VoiceService {
    fn is_live(&self, session_id: &str) -> bool {
        self.inner.live.contains_key(session_id)
    }
}

/// [`crate::drafts::TtsBudget`] over the service (weak: no Arc cycle).
struct TtsBudgetHook {
    inner: std::sync::Weak<ServiceInner>,
}

#[async_trait::async_trait]
impl crate::drafts::TtsBudget for TtsBudgetHook {
    async fn remaining_micros(&self, user_id: &str) -> i64 {
        match self.inner.upgrade() {
            Some(inner) => inner.tts_remaining(user_id).await,
            None => 0,
        }
    }

    fn charged(&self, user_id: &str, session_id: &str, micros: i64) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        for call in inner.live.iter().filter(|c| c.user_id == user_id) {
            let _ = call.cmd_tx.try_send(CallCommand::ExternalCharge {
                session_id: session_id.to_owned(),
                micros,
            });
        }
    }
}

impl crate::followup::CallPresence for VoiceService {
    fn has_live_call(&self, user_id: &str) -> bool {
        VoiceService::has_live_call(self, user_id)
    }

    fn bound_conversation(&self, user_id: &str) -> Option<String> {
        self.inner.bound_conversation(user_id)
    }

    fn note_bound_turn_end(&self, user_id: &str, conversation_id: &str) {
        let Some(session_id) = self.inner.bindings.get(user_id).map(|b| b.session_id.clone()) else {
            return;
        };
        let inner = self.inner.clone();
        let (user_id, conversation_id) = (user_id.to_owned(), conversation_id.to_owned());
        tokio::spawn(async move {
            inner
                .audit(
                    &user_id,
                    Some(&session_id),
                    crate::conversation_mode::AUDIT_TURN_END,
                    json!({"conversationId": conversation_id}),
                )
                .await;
        });
    }
}
