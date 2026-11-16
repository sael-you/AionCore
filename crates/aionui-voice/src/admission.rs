//! Call admission (`POST /api/voice/sessions`, §3.6.2), runner spawning, and
//! restart recovery (§3.6.4).
//!
//! Everything from the `starting` row onward runs in a spawned task that the
//! request awaits: a client or proxy that drops the request mid-open can no
//! longer strand a `starting` row (which would block the user via the
//! one-live-call index) or leak a provider call — the task always reaches
//! `finish` or a runner.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use aionui_api_types::{CreateVoiceSessionRequest, CreateVoiceSessionResponse};
use aionui_db::{CreateVoiceSessionParams, DbError, VoiceSessionRow};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::budget::{BudgetMeter, VOICE_MAX_OUTPUT_TOKENS, utc_day_start_ms};
use crate::conversation_mode::{SessionMode, TRANSCRIBE_ONLY_INSTRUCTIONS};
use crate::credentials::ResolvedCredential;
use crate::engine::{EngineCall, EngineSessionConfig, OpenCallRequest};
use crate::error::VoiceError;
use crate::frames::call_ended_frame;
use crate::instructions::{estimate_prefix_tokens, instructions_hash};
use crate::lifecycle::EndReason;
use crate::pricing::pricing_for;
use crate::runner::{CallRunner, RunMode};
use crate::service::{LiveCall, Liveness, VoiceService};
use crate::settings::{CallProfile, ENGINE_MINIMAX_CASCADE, MODEL_MINI, VoiceSettings};

/// Largest SDP offer accepted (a browser audio-only offer is ≈ 2–6 KB).
pub const MAX_SDP_BYTES: usize = 32 * 1024;
const TRANSCRIPTION_MODEL: &str = "gpt-transcribe";
/// Transcription keywords (conversation and project names) per call.
const MAX_KEYWORDS: usize = 40;
const MAX_KEYWORD_CHARS: usize = 60;

/// What a runner starts from.
pub(crate) struct RunnerStart {
    pub user_id: String,
    pub session_id: String,
    pub model: String,
    pub engine_call: Box<dyn EngineCall>,
    pub meter: BudgetMeter,
    pub token: Option<String>,
    pub mode: RunMode,
    /// When the call started (hard cap / rotation clocks).
    pub started: Instant,
    pub focus: Option<String>,
    /// Summary of the call this one resumes, added to the model context.
    pub carry: Option<String>,
    pub session_mode: SessionMode,
    pub car: bool,
    /// Engine kind of the call (`openai-realtime`, `minimax-cascade`).
    pub engine: &'static str,
}

impl VoiceService {
    /// `POST /api/voice/sessions`: admission → provider call → SDP answer at
    /// once; the runner attaches the sideband in parallel.
    ///
    /// `token` is the caller's session cookie; the call re-validates it every 60 s.
    pub async fn create_session(
        &self,
        user_id: &str,
        token: Option<String>,
        req: CreateVoiceSessionRequest,
    ) -> Result<CreateVoiceSessionResponse, VoiceError> {
        validate_create(&req)?;
        if SessionMode::parse(req.mode.as_deref())?.is_conversation() && req.focus_conversation_id.is_none() {
            return Err(VoiceError::BadRequest(
                "conversation mode needs focus_conversation_id".into(),
            ));
        }
        self.inner.mark_listening();
        let this = self.clone();
        let user_id = user_id.to_owned();
        tokio::spawn(async move { this.admit(&user_id, token, req).await })
            .await
            .map_err(|_| VoiceError::Internal("voice admission task failed".into()))?
    }

    async fn admit(
        &self,
        user_id: &str,
        token: Option<String>,
        req: CreateVoiceSessionRequest,
    ) -> Result<CreateVoiceSessionResponse, VoiceError> {
        let lock = self.inner.user_lock(user_id);
        let _guard = lock.lock().await;

        let settings = self.settings(user_id).await?;
        if !settings.enabled {
            return Err(VoiceError::Disabled);
        }
        let mode = SessionMode::parse(req.mode.as_deref())?;
        if mode.is_conversation() {
            // Only a conversation the user owns (`status` is user-scoped).
            let conversation = req.focus_conversation_id.as_deref().unwrap_or_default();
            match self.inner.drafts.conversations.status(user_id, conversation).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(VoiceError::NotFound(format!("conversation '{conversation}' not found")));
                }
                Err(err) => return Err(VoiceError::Internal(err)),
            }
        }
        if let Some(engine) = req.engine.as_deref()
            && !client_engine_matches(engine, &settings.engine)
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
        let carry = match req.resume_of.as_deref() {
            Some(previous) => self.previous_call_summary(user_id, previous).await,
            None => None,
        };

        let mut profile = settings.call_profile();
        // Conversation mode is an ear only: no brain, so always the realtime
        // engine on a realtime model (a cascade setting picks the ear's model).
        let cascade = !mode.is_conversation() && profile.engine == ENGINE_MINIMAX_CASCADE;
        if mode.is_conversation() && profile.engine == ENGINE_MINIMAX_CASCADE {
            profile.model = MODEL_MINI.to_owned();
        }
        let engine = if cascade {
            self.inner.cascade.clone()
        } else {
            self.inner.engine.clone()
        };
        let tools: Vec<Value> = if mode.is_conversation() {
            Vec::new()
        } else {
            crate::tools::definitions()
        };
        let instructions = if mode.is_conversation() {
            TRANSCRIBE_ONLY_INSTRUCTIONS.to_owned()
        } else {
            self.inner.jarvis_instructions(user_id).await
        };
        let used = self
            .inner
            .sessions
            .cost_since(user_id, utc_day_start_ms(aionui_common::now_ms()))
            .await?;
        let meter = BudgetMeter::new(
            pricing_for(&profile.model),
            settings.daily_budget_micros,
            used,
            estimate_prefix_tokens(&instructions, &tools),
        );
        if !meter.can_afford_response() {
            self.inner
                .audit(user_id, None, "budget_refused", json!({"usedMicros": used}))
                .await;
            return Err(VoiceError::BudgetReached);
        }

        let credential = self.resolve_credential(user_id, &settings).await?;
        let brain = if cascade {
            Some(self.resolve_minimax(user_id).await?)
        } else {
            None
        };
        let config = if mode.is_conversation() {
            conversation_session_config(&profile, &profile.model, self.transcription_keywords(user_id).await)
        } else {
            session_config(&profile, &profile.model, &instructions, tools.clone())
        };
        let hash = instructions_hash(&config.instructions, &tools);
        let session_id = aionui_common::generate_prefixed_id("vs");
        let engine_kind = engine.kind();

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

        let opened = engine
            .open(OpenCallRequest {
                credential,
                sdp_offer: req.sdp_offer,
                config,
                brain,
            })
            .await;
        let (answer, mut engine_call) = match opened {
            Ok(opened) => opened,
            Err(err) => {
                tracing::warn!(user_id, session_id = %session_id, error = %err, "voice call creation failed");
                self.close_unstarted(user_id, &session_id, None, EndReason::CreateFailed)
                    .await;
                return Err(VoiceError::Provider(err.to_string()));
            }
        };

        // The kill switch may have flipped while the provider was answering.
        if !self.is_enabled(user_id).await {
            self.close_unstarted(user_id, &session_id, Some(engine_call.as_mut()), EndReason::Disabled)
                .await;
            return Err(VoiceError::Disabled);
        }
        let call_id = engine_call.call_id().to_owned();
        if let Err(err) = self.inner.sessions.mark_live(user_id, &session_id, &call_id).await {
            tracing::error!(user_id, session_id = %session_id, error = %err, "voice mark_live failed; hanging up");
            self.close_unstarted(
                user_id,
                &session_id,
                Some(engine_call.as_mut()),
                EndReason::CreateFailed,
            )
            .await;
            return Err(err.into());
        }
        self.inner
            .audit(
                user_id,
                Some(&session_id),
                "session_created",
                json!({"engine": engine_kind, "model": profile.model, "resumeOf": req.resume_of,
                   "mode": if mode.is_conversation() { "conversation" } else { "jarvis" },
                   "focus": if mode.is_conversation() { req.focus_conversation_id.as_deref() } else { None }}),
            )
            .await;
        tracing::info!(user_id, session_id = %session_id, engine = engine_kind, model = %profile.model,
            "voice call created");

        self.spawn_runner(RunnerStart {
            user_id: user_id.to_owned(),
            session_id: session_id.clone(),
            model: profile.model.clone(),
            engine_call,
            meter,
            token,
            mode: RunMode::Fresh,
            started: Instant::now(),
            focus: req.focus_conversation_id.clone(),
            carry,
            session_mode: mode,
            car: settings.car_mode,
            engine: engine_kind,
        });
        Ok(CreateVoiceSessionResponse {
            voice_session_id: session_id,
            sdp_answer: answer,
            engine: engine_kind.to_owned(),
            model: profile.model,
        })
    }

    /// Names that bias transcription: the user's conversations and projects.
    async fn transcription_keywords(&self, user_id: &str) -> Vec<String> {
        let conversations = &self.inner.drafts.conversations;
        let mut names: Vec<String> = conversations
            .list_projects(user_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.name)
            .collect();
        let mut recent = conversations.list_conversations(user_id).await.unwrap_or_default();
        recent.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        names.extend(recent.into_iter().map(|c| c.name));
        let mut keywords: Vec<String> = Vec::new();
        for name in names {
            let name = name.trim();
            if name.is_empty() || name.chars().count() > MAX_KEYWORD_CHARS || keywords.iter().any(|k| k == name) {
                continue;
            }
            keywords.push(name.to_owned());
            if keywords.len() >= MAX_KEYWORDS {
                break;
            }
        }
        keywords
    }

    /// Short summary of a previous call of this user from its last transcript
    /// turns (rotation / re-dial, §3.5): no extra model call.
    async fn previous_call_summary(&self, user_id: &str, previous: &str) -> Option<String> {
        self.inner.sessions.get(user_id, previous).await.ok().flatten()?;
        let turns = self
            .inner
            .drafts
            .repo
            .recent_transcripts(previous, SUMMARY_TURNS)
            .await
            .ok()?;
        summarize_turns(&turns)
    }

    /// End a call that never got a runner: hang up (if created), persist, audit.
    async fn close_unstarted(
        &self,
        user_id: &str,
        session_id: &str,
        engine_call: Option<&mut (dyn EngineCall + '_)>,
        reason: EndReason,
    ) {
        if let Some(call) = engine_call
            && let Err(err) = call.close(reason.as_str()).await
        {
            tracing::warn!(session_id, error = %err, "voice provider hangup failed");
        }
        if let Err(err) = self
            .inner
            .sessions
            .finish(user_id, session_id, reason.terminal_state(), reason.as_str())
            .await
        {
            tracing::error!(session_id, error = %err, "voice session finish failed");
        }
        self.inner
            .audit(
                user_id,
                Some(session_id),
                "session_failed",
                json!({"reason": reason.as_str()}),
            )
            .await;
    }

    pub(crate) async fn resolve_credential(
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

    /// The MiniMax key of the cascade brain (official MiniMax hosts only).
    async fn resolve_minimax(&self, user_id: &str) -> Result<ResolvedCredential, VoiceError> {
        let credentials = self
            .inner
            .credentials
            .provider_credentials(user_id)
            .await
            .map_err(VoiceError::Internal)?;
        self.inner
            .credential_policy
            .resolve_minimax(&credentials)
            .ok_or(VoiceError::NoMiniMaxProvider)
    }

    /// Register the call, then start its runner. The runner waits until it is
    /// registered, so a runner that ends at once cannot leave a stale entry.
    pub(crate) fn spawn_runner(&self, start: RunnerStart) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let call = Arc::new(LiveCall {
            session_id: start.session_id.clone(),
            user_id: start.user_id,
            engine: start.engine.to_owned(),
            model: start.model,
            cmd_tx,
            ready: AtomicBool::new(false),
            liveness: std::sync::Mutex::new(Liveness {
                last_heartbeat: Instant::now(),
                frozen: false,
            }),
            token: std::sync::Mutex::new(start.token),
            focus: start.focus.clone(),
            mode: start.session_mode,
            car: start.car,
        });
        self.inner.live.insert(start.session_id.clone(), call.clone());
        match (start.session_mode, start.focus) {
            (SessionMode::Conversation, Some(conversation_id)) => {
                self.inner.bindings.insert(
                    call.user_id.clone(),
                    crate::service::Binding {
                        conversation_id,
                        session_id: start.session_id.clone(),
                        until: None,
                    },
                );
            }
            // Jarvis takes over: the user left conversation voice mode.
            _ => {
                self.inner.bindings.remove(&call.user_id);
            }
        }
        let runner = CallRunner::new(
            self.inner.clone(),
            call,
            start.engine_call,
            cmd_rx,
            start.meter,
            start.mode,
            start.started,
            start.carry,
        );
        let (registered_tx, registered_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = registered_rx.await;
            runner.run().await;
        });
        self.inner.tasks.insert(start.session_id, handle.abort_handle());
        let _ = registered_tx.send(());
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

    /// Startup: re-attach a sideband to every call persisted as live, or close
    /// it (§3.6.4, §6 "Deploy restarts AionUi mid-call"). Returns how many
    /// calls were handed to a runner.
    pub async fn recover_open_sessions(&self) -> usize {
        // The backend serves again: device liveness of recovered calls counts
        // from now, even if no device ever reconnects (else never stale).
        self.inner.mark_listening();
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
                    if row.instructions_hash.as_deref() == Some(conversation_instructions_hash().as_str()) {
                        self.note_interrupted_turn(&row).await;
                    }
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

    /// A conversation-mode call died with the process: when its last voice
    /// message has no turn end of that conversation after it (in any of the
    /// user's sessions: a rotation changes the session id) and the
    /// conversation is not running now, the restart interrupted that turn —
    /// say so on the next hello.
    async fn note_interrupted_turn(&self, row: &VoiceSessionRow) {
        use crate::conversation_mode::{AUDIT_TURN_END, AUDIT_VOICE_MESSAGE};
        let Ok(events) = self.inner.audit.list(&row.user_id, 500).await else {
            return;
        };
        let conversation_of = |detail: &str| {
            serde_json::from_str::<Value>(detail)
                .ok()
                .and_then(|d| d["conversationId"].as_str().map(str::to_owned))
        };
        // Newest first: this session's last voice message names the conversation.
        let Some(conversation_id) = events
            .iter()
            .find(|e| e.session_id.as_deref() == Some(row.id.as_str()) && e.event == AUDIT_VOICE_MESSAGE)
            .and_then(|e| conversation_of(&e.detail))
        else {
            return;
        };
        let last = events.iter().find(|e| {
            (e.event == AUDIT_VOICE_MESSAGE || e.event == AUDIT_TURN_END)
                && conversation_of(&e.detail).as_deref() == Some(conversation_id.as_str())
        });
        if last.is_none_or(|e| e.event != AUDIT_VOICE_MESSAGE) {
            return;
        }
        let running = matches!(
            self.inner.drafts.conversations.status(&row.user_id, &conversation_id).await,
            Ok(Some(status)) if status.is_processing
        );
        if !running {
            tracing::info!(session_id = %row.id, conversation_id = %conversation_id,
                "voice recovery: the restart interrupted a bound turn");
            self.inner.interrupted.insert(row.user_id.clone(), conversation_id);
        }
    }

    async fn recover_one(&self, row: &VoiceSessionRow) -> Result<(), EndReason> {
        let call_id = row
            .call_id
            .clone()
            .filter(|_| row.state == "live")
            .ok_or(EndReason::Restart)?;
        // A conversation-mode call has no persisted binding: hang it up; the
        // client re-opens it with its binding on the next wake (C26 §3.4).
        if row.instructions_hash.as_deref() == Some(conversation_instructions_hash().as_str()) {
            return Err(EndReason::Restart);
        }
        // The cascade brain's context lived in the dead process: end it like a
        // conversation-mode call; the client drops its WebRTC ear on
        // `voice.callEnded` and re-dials on the next wake.
        if row.engine == ENGINE_MINIMAX_CASCADE {
            return Err(EndReason::Restart);
        }
        let lock = self.inner.user_lock(&row.user_id);
        let _guard = lock.lock().await;
        let settings = self.settings(&row.user_id).await.map_err(|_| EndReason::Restart)?;
        let credential = self
            .resolve_credential(&row.user_id, &settings)
            .await
            .map_err(|_| EndReason::Restart)?;
        let tools: Vec<Value> = crate::tools::definitions();
        let instructions = self.inner.jarvis_instructions(&row.user_id).await;
        let config = session_config(&settings.call_profile(), &row.model, &instructions, tools.clone());
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
        // This session's earlier cost is in `used`; the meter tracks it as base.
        let meter = BudgetMeter::new(
            pricing_for(&row.model),
            settings.daily_budget_micros,
            used,
            estimate_prefix_tokens(&instructions, &tools),
        );
        self.spawn_runner(RunnerStart {
            user_id: row.user_id.clone(),
            session_id: row.id.clone(),
            model: row.model.clone(),
            engine_call,
            meter,
            token: None,
            mode: RunMode::Recovered,
            started: started_from_created_at(row.created_at),
            focus: None,
            carry: None,
            session_mode: SessionMode::Jarvis,
            car: settings.car_mode,
            engine: self.inner.engine.kind(),
        });
        Ok(())
    }
}

const SUMMARY_TURNS: u32 = 8;
const SUMMARY_WORDS_PER_TURN: usize = 25;
const SUMMARY_MAX_WORDS: usize = 150;

/// "Summary of the previous call: User: … Jarvis: …", bounded in words.
pub(crate) fn summarize_turns(turns: &[(String, String)]) -> Option<String> {
    let mut words = 0;
    let mut parts = Vec::new();
    for (role, text) in turns {
        let speaker = if role == "assistant" { "Jarvis" } else { "User" };
        let line: Vec<&str> = text.split_whitespace().take(SUMMARY_WORDS_PER_TURN).collect();
        if line.is_empty() {
            continue;
        }
        if words + line.len() > SUMMARY_MAX_WORDS {
            break;
        }
        words += line.len();
        parts.push(format!("{speaker}: {}", line.join(" ")));
    }
    (!parts.is_empty()).then(|| {
        format!(
            "Summary of the previous call (context only, not a request): {}",
            parts.join(" ")
        )
    })
}

/// The call's original start on the monotonic clock, so a recovered call keeps
/// its hard-cap and rotation deadlines.
fn started_from_created_at(created_at_ms: i64) -> Instant {
    let age_ms = aionui_common::now_ms().saturating_sub(created_at_ms).max(0);
    let now = Instant::now();
    now.checked_sub(Duration::from_millis(age_ms as u64)).unwrap_or(now)
}

fn session_config(profile: &CallProfile, model: &str, instructions: &str, tools: Vec<Value>) -> EngineSessionConfig {
    EngineSessionConfig {
        model: model.to_owned(),
        instructions: instructions.to_owned(),
        voice: profile.voice.clone(),
        eagerness: profile.eagerness.clone(),
        noise_reduction: profile.noise_reduction.clone(),
        transcription_model: TRANSCRIPTION_MODEL.to_owned(),
        keywords: Vec::new(),
        reasoning_effort: "low".to_owned(),
        max_output_tokens: VOICE_MAX_OUTPUT_TOKENS,
        create_response: true,
        tools,
    }
}

/// The transcribe-only "ear" of conversation mode (C26 §2.3 1): no tools, no
/// provider responses, tolerant end-of-turn, keywords for names.
fn conversation_session_config(profile: &CallProfile, model: &str, keywords: Vec<String>) -> EngineSessionConfig {
    EngineSessionConfig {
        model: model.to_owned(),
        instructions: TRANSCRIBE_ONLY_INSTRUCTIONS.to_owned(),
        voice: profile.voice.clone(),
        eagerness: "low".to_owned(),
        noise_reduction: profile.noise_reduction.clone(),
        transcription_model: TRANSCRIPTION_MODEL.to_owned(),
        keywords,
        reasoning_effort: "low".to_owned(),
        max_output_tokens: VOICE_MAX_OUTPUT_TOKENS,
        create_response: false,
        tools: Vec::new(),
    }
}

/// Hash stored on conversation-mode sessions (recovery tells them apart).
pub(crate) fn conversation_instructions_hash() -> String {
    instructions_hash(TRANSCRIBE_ONLY_INSTRUCTIONS, &[])
}

/// The engine a client asks for matches the setting. Every browser build sends
/// `openai-realtime` (its WebRTC transport); the cascade's ear is that same
/// call, so a cascade setting accepts it and the client needs no change.
fn client_engine_matches(requested: &str, configured: &str) -> bool {
    requested == configured || (configured == ENGINE_MINIMAX_CASCADE && requested == crate::engine::openai::ENGINE_KIND)
}

fn validate_create(req: &CreateVoiceSessionRequest) -> Result<(), VoiceError> {
    SessionMode::parse(req.mode.as_deref())?;
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
            mode: None,
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
        let mut bad_mode = req("v=0");
        bad_mode.mode = Some("dictation".into());
        assert!(matches!(validate_create(&bad_mode), Err(VoiceError::BadRequest(_))));
    }

    #[test]
    fn summary_is_bounded_and_attributed() {
        let turns = vec![
            ("user".to_owned(), "tell the kanban build to rerun the tests".to_owned()),
            ("assistant".to_owned(), "Done, it is running.".to_owned()),
        ];
        let summary = summarize_turns(&turns).unwrap();
        assert!(summary.contains("User: tell the kanban build"));
        assert!(summary.contains("Jarvis: Done"));
        let long = vec![("user".to_owned(), "word ".repeat(400)); 10];
        let bounded = summarize_turns(&long).unwrap();
        assert!(bounded.split_whitespace().count() <= SUMMARY_MAX_WORDS + 20);
        assert!(summarize_turns(&[]).is_none());
    }

    #[test]
    fn recovered_start_is_back_dated() {
        let started = started_from_created_at(aionui_common::now_ms() - 60_000);
        assert!(started.elapsed() >= Duration::from_secs(59));
        let future = started_from_created_at(aionui_common::now_ms() + 60_000);
        assert!(future.elapsed() < Duration::from_secs(1));
    }
}
