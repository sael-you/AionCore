//! One task per live call: config gate, event handling, budget metering,
//! lossy re-attach, timers, 60 s session re-validation, and the hangup path.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aionui_api_types::{VoiceAssistantAudioFinished, VoiceBudget, VoiceCaption, VoiceReady, VoiceServerFrame};
use aionui_db::VoiceUsageDelta;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::budget::{BudgetMeter, BudgetThreshold};
use crate::engine::{EngineCall, EngineError, EngineEvent};
use crate::error::VoiceError;
use crate::frames::{call_ended_frame, error_frame, state_frame};
use crate::lifecycle::{EndReason, TickInputs, tick_decision};
use crate::ports::AuthRejected;
use crate::runner_drafts::{ClientEvent, TurnState};
use crate::service::{LiveCall, ServiceInner};
use crate::settings::micros_to_usd;

pub(crate) enum CallCommand {
    End {
        reason: EndReason,
        done: oneshot::Sender<()>,
    },
    SetCreateResponse {
        enabled: bool,
        done: oneshot::Sender<Result<(), VoiceError>>,
    },
    CreateResponse {
        done: oneshot::Sender<Result<(), VoiceError>>,
    },
    /// A client frame for this call (evidence, taps) or a pending `ask`.
    Client(ClientEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunMode {
    /// Call just created by this process.
    Fresh,
    /// Call persisted as live by a previous process (restart recovery).
    Recovered,
}

pub(crate) struct CallRunner {
    pub(crate) inner: Arc<ServiceInner>,
    pub(crate) call: Arc<LiveCall>,
    pub(crate) engine_call: Box<dyn EngineCall>,
    cmd_rx: mpsc::Receiver<CallCommand>,
    pub(crate) meter: BudgetMeter,
    pub(crate) turn: TurnState,
    /// Summary of the call this one resumes (rotation / re-dial).
    carry: Option<String>,
    mode: RunMode,
    started: Instant,
    last_activity: Instant,
    last_revalidate: Instant,
    attached: bool,
    speaking: bool,
    speech_start_ms: Option<u64>,
    end_waiters: Vec<oneshot::Sender<()>>,
}

impl CallRunner {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        inner: Arc<ServiceInner>,
        call: Arc<LiveCall>,
        engine_call: Box<dyn EngineCall>,
        cmd_rx: mpsc::Receiver<CallCommand>,
        meter: BudgetMeter,
        mode: RunMode,
        started: Instant,
        carry: Option<String>,
    ) -> Self {
        let now = Instant::now();
        Self {
            inner,
            call,
            engine_call,
            cmd_rx,
            meter,
            mode,
            started,
            last_activity: now,
            last_revalidate: now,
            attached: false,
            speaking: false,
            speech_start_ms: None,
            end_waiters: Vec::new(),
            turn: TurnState::default(),
            carry,
        }
    }

    pub(crate) async fn run(mut self) {
        let reason = self.drive().await;
        self.finish(reason).await;
    }

    fn session_id(&self) -> &str {
        &self.call.session_id
    }

    fn send(&self, frame: VoiceServerFrame) {
        self.inner.sockets.send_user(&self.call.user_id, &frame);
    }

    async fn drive(&mut self) -> EndReason {
        if let Some(reason) = self.setup().await {
            return reason;
        }
        let mut tick = tokio::time::interval(self.inner.timings.tick);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let outcome = tokio::select! {
                event = self.engine_call.next_event(), if self.attached => match event {
                    Some(event) => self.on_event(event).await,
                    None => self.on_event(EngineEvent::ControlLost).await,
                },
                command = self.cmd_rx.recv() => match command {
                    Some(command) => self.on_command(command).await,
                    None => Some(EndReason::User),
                },
                _ = tick.tick() => self.on_tick().await,
            };
            if let Some(reason) = outcome {
                return reason;
            }
        }
    }

    /// Attach + config gate (§3.6.2). An `End` during setup wins.
    async fn setup(&mut self) -> Option<EndReason> {
        let result = {
            let listen = self.engine_call.listen();
            tokio::pin!(listen);
            loop {
                tokio::select! {
                    result = &mut listen => break result,
                    command = self.cmd_rx.recv() => match command {
                        Some(CallCommand::End { reason, done }) => {
                            self.end_waiters.push(done);
                            return Some(reason);
                        }
                        Some(other) => reject_not_ready(other),
                        None => return Some(EndReason::User),
                    },
                }
            }
        };
        if let Err(err) = result {
            tracing::warn!(session_id = %self.call.session_id, error = %err, "voice call setup failed");
            self.send(error_frame(
                Some(self.session_id()),
                "VOICE_SETUP_FAILED",
                "The voice call could not start.",
            ));
            return Some(match err {
                EngineError::ConfigTimeout => EndReason::ConfigTimeout,
                EngineError::CallGone => EndReason::ProviderEnded,
                _ if self.mode == RunMode::Recovered => EndReason::Restart,
                _ => EndReason::AttachFailed,
            });
        }
        self.attached = true;
        if self.mode == RunMode::Recovered {
            let delta = self.meter.charge_restart_gap();
            self.persist(&delta).await;
        }
        let now_ms = aionui_common::now_ms();
        if let Err(err) = self
            .inner
            .sessions
            .mark_ready(&self.call.user_id, &self.call.session_id, now_ms)
            .await
        {
            tracing::warn!(error = %err, "voice mark_ready failed");
        }
        self.call.ready.store(true, Ordering::SeqCst);
        tracing::info!(
            session_id = %self.call.session_id,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            recovered = self.mode == RunMode::Recovered,
            "voice call ready"
        );
        self.send_ready();
        let carry = self.carry.take();
        self.on_call_ready(carry).await;
        None
    }

    fn send_ready(&self) {
        self.send(VoiceServerFrame::Ready(VoiceReady {
            voice_session_id: self.call.session_id.clone(),
            engine: self.call.engine.clone(),
            model: self.call.model.clone(),
        }));
        self.send(state_frame(Some(self.session_id()), "listening", None));
    }

    async fn persist(&mut self, delta: &VoiceUsageDelta) {
        if *delta == VoiceUsageDelta::default() {
            return;
        }
        if let Err(err) = self
            .inner
            .sessions
            .add_usage(
                &self.call.user_id,
                &self.call.session_id,
                delta,
                aionui_common::now_ms(),
            )
            .await
        {
            tracing::warn!(error = %err, session_id = %self.call.session_id, "voice usage persist failed");
        }
    }

    fn budget_frame(&self, warning: Option<&str>) -> VoiceServerFrame {
        VoiceServerFrame::Budget(VoiceBudget {
            voice_session_id: Some(self.call.session_id.clone()),
            used_usd: micros_to_usd(self.meter.used_micros()),
            limit_usd: micros_to_usd(self.meter.limit_micros()),
            warning: warning.map(str::to_owned),
        })
    }

    /// Returns `Some(reason)` when the budget cap was reached.
    fn check_budget(&mut self) -> Option<EndReason> {
        match self.meter.threshold() {
            Some(BudgetThreshold::Warning) => {
                self.send(self.budget_frame(Some("80")));
                None
            }
            Some(BudgetThreshold::Reached) => {
                self.send(self.budget_frame(Some("100")));
                Some(EndReason::Budget)
            }
            None => None,
        }
    }

    async fn on_event(&mut self, event: EngineEvent) -> Option<EndReason> {
        if !matches!(event, EngineEvent::ConfigApplied | EngineEvent::ControlLost) {
            self.last_activity = Instant::now();
        }
        match event {
            EngineEvent::UserSpeechStarted { audio_start_ms } => {
                self.speech_start_ms = audio_start_ms;
                self.on_user_speech_started();
                self.send(state_frame(Some(self.session_id()), "listening", None));
            }
            EngineEvent::UserSpeechStopped { audio_end_ms } => {
                if let (Some(start), Some(end)) = (self.speech_start_ms.take(), audio_end_ms) {
                    self.meter.on_user_audio(end.saturating_sub(start));
                }
                self.send(state_frame(Some(self.session_id()), "thinking", None));
            }
            EngineEvent::InputCommitted { .. } | EngineEvent::ConfigApplied => {}
            EngineEvent::Transcript {
                role,
                text,
                is_final,
                stt_seconds,
            } => {
                if let Some(seconds) = stt_seconds {
                    let delta = self.meter.on_stt_seconds(seconds);
                    self.persist(&delta).await;
                }
                let user_final = (role == crate::engine::TranscriptRole::User && is_final).then(|| text.clone());
                let assistant_final =
                    (role == crate::engine::TranscriptRole::Assistant && is_final).then(|| text.clone());
                self.send(VoiceServerFrame::Caption(VoiceCaption {
                    voice_session_id: self.call.session_id.clone(),
                    role: role.as_str().to_owned(),
                    text,
                    is_final,
                }));
                if let Some(text) = user_final
                    && !text.trim().is_empty()
                    && let Some(reason) = self.on_user_final(&text).await
                {
                    return Some(reason);
                }
                if let Some(text) = assistant_final
                    && !text.trim().is_empty()
                {
                    self.on_assistant_final(&text).await;
                }
                return self.check_budget();
            }
            EngineEvent::ResponseStarted { response_id } => {
                if let Err(refused) = self.meter.on_response_created(&response_id) {
                    tracing::info!(
                        session_id = %self.call.session_id,
                        needed_micros = refused.needed_micros,
                        remaining_micros = refused.remaining_micros,
                        "voice response refused: budget reached"
                    );
                    let _ = self.engine_call.interrupt().await;
                    self.send(self.budget_frame(Some("100")));
                    self.send(error_frame(
                        Some(self.session_id()),
                        "VOICE_BUDGET_REACHED",
                        "Budget reached.",
                    ));
                    return Some(EndReason::Budget);
                }
            }
            EngineEvent::ResponseDone {
                response_id,
                status,
                usage,
            } => {
                let delta = self.meter.on_response_done(&response_id, usage.as_ref());
                tracing::debug!(session_id = %self.call.session_id, status = %status, cost_micros = delta.cost_micros,
                    "voice response done");
                self.persist(&delta).await;
                return self.check_budget();
            }
            EngineEvent::AssistantAudioStarted { .. } => {
                self.speaking = true;
                self.send(state_frame(Some(self.session_id()), "speaking", None));
            }
            EngineEvent::AssistantAudioFinished { response_id } => {
                self.speaking = false;
                self.send(VoiceServerFrame::AssistantAudioFinished(VoiceAssistantAudioFinished {
                    voice_session_id: self.call.session_id.clone(),
                    response_id,
                }));
                self.send(state_frame(Some(self.session_id()), "listening", None));
            }
            EngineEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                self.on_tool_call(&call_id, &name, &arguments).await;
            }
            EngineEvent::ProviderError { code } => {
                tracing::warn!(session_id = %self.call.session_id, code = %code, "voice provider error event");
                self.send(error_frame(Some(self.session_id()), "VOICE_PROVIDER_ERROR", &code));
            }
            EngineEvent::ControlLost => return self.recover_control().await,
        }
        None
    }

    /// Lossy re-attach (§3.6.4): in-flight responses become unknown (charged
    /// at their reservation); retry until the deadline, then hang up.
    async fn recover_control(&mut self) -> Option<EndReason> {
        self.attached = false;
        self.speaking = false;
        let delta = self.meter.charge_in_flight_as_unknown();
        self.persist(&delta).await;
        tracing::warn!(session_id = %self.call.session_id, unknown_responses = delta.unknown_responses,
            "voice sideband lost; re-attaching");
        self.send(state_frame(Some(self.session_id()), "offline", Some("reconnecting")));
        let deadline = Instant::now() + self.inner.timings.reattach_deadline;
        while Instant::now() < deadline {
            let attempt = tokio::select! {
                result = tokio::time::timeout_at(deadline, self.engine_call.reattach()) => result,
                command = self.cmd_rx.recv() => match command {
                    Some(CallCommand::End { reason, done }) => {
                        self.end_waiters.push(done);
                        return Some(reason);
                    }
                    Some(other) => { reject_not_ready(other); continue; }
                    None => return Some(EndReason::User),
                },
            };
            match attempt {
                Ok(Ok(())) => {
                    self.attached = true;
                    tracing::info!(session_id = %self.call.session_id, "voice sideband re-attached");
                    self.send_ready();
                    return self.check_budget();
                }
                Ok(Err(EngineError::CallGone)) => return Some(EndReason::ProviderEnded),
                Ok(Err(err)) => {
                    tracing::debug!(session_id = %self.call.session_id, error = %err, "voice re-attach attempt failed");
                    tokio::time::sleep(self.inner.timings.reattach_retry_delay).await;
                }
                Err(_) => break,
            }
        }
        Some(EndReason::ControlLost)
    }

    async fn on_command(&mut self, command: CallCommand) -> Option<EndReason> {
        match command {
            CallCommand::End { reason, done } => {
                self.end_waiters.push(done);
                Some(reason)
            }
            CallCommand::SetCreateResponse { enabled, done } => {
                let result = self
                    .engine_call
                    .set_create_response(enabled)
                    .await
                    .map_err(|e| VoiceError::Provider(e.to_string()));
                let _ = done.send(result);
                None
            }
            CallCommand::Client(event) => {
                self.on_client(event).await;
                None
            }
            CallCommand::CreateResponse { done } => {
                if self.meter.reserve_own().is_err() {
                    let _ = done.send(Err(VoiceError::BudgetReached));
                    return None;
                }
                let result = self.engine_call.create_response().await;
                if result.is_err() {
                    self.meter.release_own();
                }
                let _ = done.send(result.map_err(|e| VoiceError::Provider(e.to_string())));
                None
            }
        }
    }

    async fn on_tick(&mut self) -> Option<EndReason> {
        self.tick_drafts().await;
        let timings = self.inner.timings;
        if let Some(token) = self.call.token()
            && self.last_revalidate.elapsed() >= timings.revalidate_every
        {
            self.last_revalidate = Instant::now();
            match self.inner.verifier.verify(&token).await {
                Ok(user) if user.id == self.call.user_id => {}
                Ok(_) | Err(AuthRejected::Rejected) => {
                    tracing::warn!(session_id = %self.call.session_id, "voice call session re-validation failed");
                    return Some(EndReason::AuthRevoked);
                }
                Err(AuthRejected::Unavailable) => {
                    tracing::warn!(session_id = %self.call.session_id, "voice call re-validation unavailable");
                }
            }
        }
        // Liveness counts from the later of the last heartbeat and the moment
        // the HTTP listener came up: a recovered call is not "stale" because
        // the backend itself took long to start.
        let listening = self.inner.listening_since();
        let (since_heartbeat, frozen) = match (self.call.liveness.lock(), listening) {
            (Ok(l), Some(listening)) => (l.last_heartbeat.max(listening).elapsed(), l.frozen),
            (Ok(l), None) => (std::time::Duration::ZERO, l.frozen),
            (Err(_), _) => (std::time::Duration::ZERO, false),
        };
        // Conversation mode: a running turn of the bound conversation keeps
        // the (muted, response-free) call open, so a barge-in needs no re-dial.
        if self.call.mode.is_conversation()
            && self.last_activity.elapsed() >= timings.idle_hangup
            && let Some(conversation_id) = self.call.focus.as_deref()
            && let Ok(Some(status)) = self
                .inner
                .drafts
                .conversations
                .status(&self.call.user_id, conversation_id)
                .await
            && (status.is_processing || status.pending_confirmations > 0)
        {
            self.last_activity = Instant::now();
        }
        let inputs = TickInputs {
            since_start: self.started.elapsed(),
            since_activity: self.last_activity.elapsed(),
            since_heartbeat,
            device_frozen: frozen,
            response_in_flight: self.meter.has_in_flight(),
            assistant_speaking: self.speaking,
            draft_pending: self.draft_pending(),
        };
        tick_decision(&inputs, &timings)
    }

    /// Provider hangup + persisted end + notification (§3.6.6).
    async fn finish(mut self, reason: EndReason) {
        if let Err(err) = self.engine_call.close(reason.as_str()).await {
            tracing::warn!(session_id = %self.call.session_id, error = %err, "voice provider hangup failed");
        }
        let user_id = self.call.user_id.clone();
        let session_id = self.call.session_id.clone();
        if let Err(err) = self
            .inner
            .sessions
            .finish(&user_id, &session_id, reason.terminal_state(), reason.as_str())
            .await
        {
            tracing::error!(error = %err, session_id = %session_id, "voice session finish failed");
        }
        let duration_ms = self.started.elapsed().as_millis() as u64;
        let cost_micros = self.meter.session_cost_micros();
        self.inner
            .audit(
                &user_id,
                Some(&session_id),
                "session_ended",
                json!({"reason": reason.as_str(), "durationMs": duration_ms, "costMicros": cost_micros}),
            )
            .await;
        tracing::info!(
            user_id = %user_id,
            session_id = %session_id,
            engine = %self.call.engine,
            model = %self.call.model,
            reason = reason.as_str(),
            duration_ms,
            cost_usd = micros_to_usd(cost_micros),
            "voice call ended"
        );
        // The one end signal clients act on (sent exactly once: `finish` runs once).
        self.send(call_ended_frame(&session_id, reason.as_str()));
        let state = if reason == EndReason::Disabled {
            "offline"
        } else {
            "armed"
        };
        self.send(state_frame(Some(&session_id), state, Some(reason.as_str())));
        self.inner.live.remove(&session_id);
        self.inner.tasks.remove(&session_id);
        if self.call.mode.is_conversation() {
            self.inner.release_binding(&user_id, &session_id, reason);
        }
        // Answer anything still queued, then release the waiters.
        self.cmd_rx.close();
        while let Ok(command) = self.cmd_rx.try_recv() {
            match command {
                CallCommand::End { done, .. } => self.end_waiters.push(done),
                other => reject_not_ready(other),
            }
        }
        for waiter in self.end_waiters.drain(..) {
            let _ = waiter.send(());
        }
    }
}

fn reject_not_ready(command: CallCommand) {
    let err = || VoiceError::NotFound("voice call is not ready".into());
    match command {
        CallCommand::End { done, .. } => {
            let _ = done.send(());
        }
        CallCommand::SetCreateResponse { done, .. } | CallCommand::CreateResponse { done } => {
            let _ = done.send(Err(err()));
        }
        CallCommand::Client(_) => {}
    }
}
