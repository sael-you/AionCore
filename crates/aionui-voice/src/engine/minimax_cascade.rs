//! `minimax-cascade` engine (PLAN §3.1, S4; S0-B "cascade GO as the second engine"):
//! an "ear" (the `openai-realtime` call in transcribe-only mode: semantic VAD,
//! streaming `gpt-transcribe` with keywords, no responses — the C26 profile) →
//! the MiniMax brain (`MiniMax-M3` chat completions, thinking off, priority tier,
//! the same tools) → MiniMax TTS clips voiced by the runner sentence by sentence
//! ([`EngineEvent::AssistantSpeech`]).
//!
//! MiniMax ASR is not used (S0-B #17: file-only, ≈ 2.5 s fixed). The browser
//! keeps its WebRTC call to the ear, so the device protocol does not change.
//! The brain's context lives in this call only: a restart hangs a cascade call
//! up (the client re-dials), it is never resumed.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::minimax_chat::{ChatStream, SentenceSplitter, ToolCallAcc, chat_tools, request_body};
use super::{
    EngineCall, EngineError, EngineEvent, EngineSessionConfig, OpenCallRequest, SpeakMode, TranscriptRole, VoiceEngine,
};
use crate::conversation_mode::TRANSCRIBE_ONLY_INSTRUCTIONS;
use crate::credentials::ResolvedCredential;
use crate::pricing::Usage;

pub const ENGINE_KIND: &str = "minimax-cascade";
/// The ear only transcribes (no responses), so it runs on the cheapest realtime model.
const EAR_MODEL: &str = crate::settings::MODEL_MINI;
/// Brain history kept per call (messages, oldest dropped first).
pub(crate) const MAX_HISTORY: usize = 40;
/// No first byte / no next chunk from the brain for this long = failed turn.
const BRAIN_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Silence after a final transcript before the brain answers (TASK-112, owner:
/// "give a bit more time"): a fragment arriving meanwhile joins the same user
/// turn. `semantic_vad` has no silence parameter, so the cascade adds it.
pub const ANSWER_AFTER_SILENCE: Duration = Duration::from_millis(1500);
/// Spoken when the brain fails, so the user is never left in silence.
const BRAIN_FAILED_LINE: &str = "Sorry, I could not answer that. Please try again.";

/// Creates cascade calls on top of a realtime engine used as the ear.
pub struct MiniMaxCascadeEngine {
    ear: Arc<dyn VoiceEngine>,
    http: reqwest::Client,
    answer_silence: Duration,
}

impl MiniMaxCascadeEngine {
    pub fn new(ear: Arc<dyn VoiceEngine>) -> Self {
        Self::with_answer_silence(ear, ANSWER_AFTER_SILENCE)
    }

    /// `answer_silence`: see [`ANSWER_AFTER_SILENCE`].
    pub fn with_answer_silence(ear: Arc<dyn VoiceEngine>, answer_silence: Duration) -> Self {
        // Streamed answers: no total timeout, an idle timeout per chunk instead.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            ear,
            http,
            answer_silence,
        }
    }
}

/// The ear's config: transcribe-only (C26 §2.3 1), the cascade keeps the
/// instructions, tools and model for its brain.
pub(crate) fn ear_config(config: &EngineSessionConfig) -> EngineSessionConfig {
    EngineSessionConfig {
        model: EAR_MODEL.to_owned(),
        instructions: TRANSCRIBE_ONLY_INSTRUCTIONS.to_owned(),
        // The most tolerant end-of-turn the Realtime API documents ("lets the
        // user take their time"); the cascade adds its own silence on top.
        eagerness: "low".to_owned(),
        create_response: false,
        tools: Vec::new(),
        ..config.clone()
    }
}

#[async_trait::async_trait]
impl VoiceEngine for MiniMaxCascadeEngine {
    fn kind(&self) -> &'static str {
        ENGINE_KIND
    }

    async fn open(&self, request: OpenCallRequest) -> Result<(String, Box<dyn EngineCall>), EngineError> {
        let OpenCallRequest {
            credential,
            sdp_offer,
            config,
            brain,
        } = request;
        let brain = brain.ok_or_else(|| EngineError::Transport("no MiniMax credential".into()))?;
        let (answer, ear) = self
            .ear
            .open(OpenCallRequest {
                credential,
                sdp_offer,
                config: ear_config(&config),
                brain: None,
            })
            .await?;
        let brain = Brain {
            http: self.http.clone(),
            credential: brain,
            model: config.model.clone(),
            instructions: config.instructions.clone(),
            tools: chat_tools(&config.tools),
            max_output_tokens: config.max_output_tokens,
        };
        Ok((
            answer,
            Box::new(CascadeCall::new(
                ear,
                Some(Arc::new(brain)),
                &config,
                self.answer_silence,
            )),
        ))
    }

    /// Without a brain: the ear alone, so a stray resume can still hang the
    /// provider call up. Restart recovery uses [`Self::resume_with_brain`].
    fn resume(
        &self,
        credential: ResolvedCredential,
        call_id: String,
        config: EngineSessionConfig,
    ) -> Box<dyn EngineCall> {
        let ear = self.ear.resume(credential, call_id, ear_config(&config));
        Box::new(CascadeCall::new(ear, None, &config, self.answer_silence))
    }
}

impl MiniMaxCascadeEngine {
    /// Restart recovery (TASK-37): re-attach the ear like a realtime call and
    /// give the brain its credential and the call's dialogue back, so the
    /// next answer still knows what was said before the restart.
    pub(crate) fn resume_with_brain(
        &self,
        credential: ResolvedCredential,
        call_id: String,
        config: EngineSessionConfig,
        brain: ResolvedCredential,
        history: Vec<Value>,
    ) -> Box<dyn EngineCall> {
        let ear = self.ear.resume(credential, call_id, ear_config(&config));
        let brain = Brain {
            http: self.http.clone(),
            credential: brain,
            model: config.model.clone(),
            instructions: config.instructions.clone(),
            tools: chat_tools(&config.tools),
            max_output_tokens: config.max_output_tokens,
        };
        let mut call = CascadeCall::new(ear, Some(Arc::new(brain)), &config, self.answer_silence);
        for message in history {
            call.push_history(message);
        }
        Box::new(call)
    }
}

/// The brain's history rebuilt from a session's persisted transcript turns
/// (`voice_transcripts`, oldest first): the user's words and Jarvis's
/// answers; tool calls and results of the dead process are not kept.
pub(crate) fn history_from_transcripts(turns: &[(String, String)]) -> Vec<Value> {
    turns
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .filter_map(|(role, text)| match role.as_str() {
            "user" | "assistant" => Some(json!({"role": role, "content": text})),
            _ => None,
        })
        .collect()
}

/// One streamed MiniMax completion.
struct Brain {
    http: reqwest::Client,
    credential: ResolvedCredential,
    model: String,
    instructions: String,
    tools: Vec<Value>,
    max_output_tokens: u64,
}

enum TurnMsg {
    Sentence(String),
    Done {
        text: String,
        tool_calls: Vec<ToolCallAcc>,
        usage: Option<Usage>,
    },
    Failed,
}

impl Brain {
    async fn run(self: Arc<Self>, response_id: String, history: Vec<Value>, tx: mpsc::UnboundedSender<TurnMsg>) {
        let started = Instant::now();
        let mut first_output_ms = None;
        let mut sentences = 0usize;
        let result = self
            .stream(&response_id, &history, &tx, &mut |kind| {
                if first_output_ms.is_none() {
                    first_output_ms = Some(started.elapsed().as_millis() as u64);
                }
                if kind {
                    sentences += 1;
                }
            })
            .await;
        let total_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(stream) => {
                let tool_calls = stream.finished_tool_calls(&response_id);
                tracing::info!(response_id = %response_id, model = %self.model, first_output_ms, total_ms,
                    sentences, tool_calls = tool_calls.len(), "voice cascade brain turn");
                let _ = tx.send(TurnMsg::Done {
                    text: stream.text.trim().to_owned(),
                    tool_calls,
                    usage: stream.usage,
                });
            }
            Err(reason) => {
                tracing::warn!(response_id = %response_id, model = %self.model, total_ms, reason = %reason,
                    "voice cascade brain turn failed");
                let _ = tx.send(TurnMsg::Failed);
            }
        }
    }

    /// Stream one completion; sentences go out as soon as they are complete.
    /// `on_output(true)` per sentence, `on_output(false)` on a tool-call fragment.
    async fn stream(
        &self,
        response_id: &str,
        history: &[Value],
        tx: &mpsc::UnboundedSender<TurnMsg>,
        on_output: &mut (dyn FnMut(bool) + Send),
    ) -> Result<ChatStream, String> {
        let body = request_body(
            &self.model,
            &self.instructions,
            history,
            &self.tools,
            self.max_output_tokens,
        );
        let url = format!("{}/v1/chat/completions", self.credential.base_url);
        let send = self
            .http
            .post(&url)
            .bearer_auth(self.credential.api_key.expose())
            .json(&body)
            .send();
        let mut response = tokio::time::timeout(BRAIN_IDLE_TIMEOUT, send)
            .await
            .map_err(|_| "request timed out".to_owned())?
            .map_err(|e| {
                if e.is_connect() {
                    "connection failed"
                } else {
                    "request failed"
                }
                .to_owned()
            })?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }
        let mut stream = ChatStream::default();
        let mut splitter = SentenceSplitter::default();
        let mut tool_fragments = 0usize;
        let mut emit = |stream: &ChatStream, content: &str, splitter: &mut SentenceSplitter| {
            for sentence in splitter.push(content) {
                on_output(true);
                let _ = tx.send(TurnMsg::Sentence(sentence));
            }
            if stream.tool_calls.len() > tool_fragments {
                tool_fragments = stream.tool_calls.len();
                on_output(false);
            }
        };
        'read: loop {
            let chunk = tokio::time::timeout(BRAIN_IDLE_TIMEOUT, response.chunk())
                .await
                .map_err(|_| "stream stalled".to_owned())?
                .map_err(|_| "stream broke".to_owned())?;
            let Some(bytes) = chunk else { break };
            for delta in stream.feed(&bytes) {
                if let Some(error) = delta.error {
                    return Err(error);
                }
                emit(&stream, &delta.content, &mut splitter);
                if delta.done {
                    break 'read;
                }
            }
        }
        if let Some(delta) = stream.finish() {
            if let Some(error) = delta.error {
                return Err(error);
            }
            emit(&stream, &delta.content, &mut splitter);
        }
        if let Some(rest) = splitter.flush() {
            on_output(true);
            let _ = tx.send(TurnMsg::Sentence(rest));
        }
        tracing::debug!(response_id, "voice cascade brain stream ended");
        Ok(stream)
    }
}

struct ActiveTurn {
    response_id: String,
    rx: mpsc::UnboundedReceiver<TurnMsg>,
    task: tokio::task::JoinHandle<()>,
    spoke: bool,
}

/// A live cascade call: the ear's events, plus brain turns run here.
struct CascadeCall {
    ear: Box<dyn EngineCall>,
    brain: Option<Arc<Brain>>,
    history: Vec<Value>,
    /// `create_response`: answer every final user transcript on its own.
    auto_respond: bool,
    out: VecDeque<EngineEvent>,
    active: Option<ActiveTurn>,
    /// Announced (`ResponseStarted`) but waiting for the active turn or tool results.
    queued: Option<String>,
    outstanding_tools: Vec<String>,
    seq: u64,
    answer_silence: Duration,
    /// When the held auto answer starts (no speech since the last final).
    answer_at: Option<Instant>,
    /// A final is held for the auto answer: the next one joins its user turn.
    answer_held: bool,
}

enum Next {
    Ear(Option<EngineEvent>),
    Turn(Option<TurnMsg>),
    Answer,
}

impl CascadeCall {
    fn new(
        ear: Box<dyn EngineCall>,
        brain: Option<Arc<Brain>>,
        config: &EngineSessionConfig,
        answer_silence: Duration,
    ) -> Self {
        Self {
            ear,
            brain,
            history: Vec::new(),
            auto_respond: config.create_response,
            out: VecDeque::new(),
            active: None,
            queued: None,
            outstanding_tools: Vec::new(),
            seq: 0,
            answer_silence,
            answer_at: None,
            answer_held: false,
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}_{}_{}", self.ear.call_id(), self.seq)
    }

    fn push_history(&mut self, message: Value) {
        self.history.push(message);
        if self.history.len() > MAX_HISTORY {
            let excess = self.history.len() - MAX_HISTORY;
            self.history.drain(..excess);
            // Never start with a tool result or a reply whose user turn is gone.
            while self.history.first().is_some_and(|m| m["role"] != "user") {
                self.history.remove(0);
            }
        }
    }

    /// Start (or queue behind the active turn / pending tool results) a turn
    /// announced as `response_id`.
    fn announce_turn(&mut self) -> Result<(), EngineError> {
        if self.brain.is_none() {
            return Err(EngineError::NotAttached);
        }
        if self.queued.is_some() {
            return Err(EngineError::Transport("a response is already queued".into()));
        }
        let response_id = self.next_id("mmresp");
        self.out.push_back(EngineEvent::ResponseStarted {
            response_id: response_id.clone(),
        });
        if self.active.is_none() && self.outstanding_tools.is_empty() {
            self.start_turn(response_id);
        } else {
            self.queued = Some(response_id);
        }
        Ok(())
    }

    fn start_turn(&mut self, response_id: String) {
        let Some(brain) = self.brain.clone() else {
            return;
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(brain.run(response_id.clone(), self.history.clone(), tx));
        self.active = Some(ActiveTurn {
            response_id,
            rx,
            task,
            spoke: false,
        });
    }

    fn start_queued(&mut self) {
        if self.active.is_none()
            && self.outstanding_tools.is_empty()
            && let Some(response_id) = self.queued.take()
        {
            self.start_turn(response_id);
        }
    }

    /// Barge-in / interrupt: stop the brain; the active turn is charged at its
    /// reservation (tokens may already be billed), a queued one never ran.
    fn cancel_turns(&mut self) {
        if let Some(turn) = self.active.take() {
            turn.task.abort();
            if turn.spoke {
                self.out.push_back(EngineEvent::AssistantAudioFinished {
                    response_id: turn.response_id.clone(),
                });
            }
            self.out.push_back(EngineEvent::ResponseDone {
                response_id: turn.response_id,
                status: "cancelled".into(),
                usage: None,
            });
        }
        if let Some(response_id) = self.queued.take() {
            self.out.push_back(EngineEvent::ResponseDone {
                response_id,
                status: "cancelled".into(),
                usage: Some(Usage::default()),
            });
        }
    }

    fn on_ear(&mut self, event: EngineEvent) {
        match &event {
            EngineEvent::UserSpeechStarted { .. } => {
                self.cancel_turns();
                // Still speaking: the held answer waits for this speech to end.
                self.answer_at = None;
                self.out.push_back(event);
            }
            EngineEvent::UserSpeechStopped { .. } => {
                if self.answer_held {
                    self.answer_at = Some(Instant::now() + self.answer_silence);
                }
                self.out.push_back(event);
            }
            EngineEvent::Transcript {
                role: TranscriptRole::User,
                text,
                is_final: true,
                ..
            } if !text.trim().is_empty() => {
                let text = text.trim().to_owned();
                self.out.push_back(event);
                self.add_user_text(&text);
                if self.auto_respond && self.brain.is_some() && self.outstanding_tools.is_empty() {
                    // A new utterance supersedes an answer still being written;
                    // the answer starts after the silence (TASK-112).
                    self.cancel_turns();
                    self.answer_held = true;
                    self.answer_at = Some(Instant::now() + self.answer_silence);
                }
            }
            _ => self.out.push_back(event),
        }
    }

    /// A final joins the held user turn, or starts a new one.
    fn add_user_text(&mut self, text: &str) {
        if self.answer_held
            && let Some(last) = self.history.last_mut()
            && last["role"] == "user"
            && let Some(previous) = last["content"].as_str()
        {
            last["content"] = Value::String(format!("{previous} {text}"));
            return;
        }
        self.push_history(json!({"role": "user", "content": text}));
    }

    /// The silence after the held final passed: answer now.
    fn answer_held_turn(&mut self) {
        self.answer_at = None;
        self.answer_held = false;
        if self.auto_respond && self.outstanding_tools.is_empty() {
            let _ = self.announce_turn();
        }
    }

    fn on_turn(&mut self, message: Option<TurnMsg>) {
        let Some(turn) = self.active.as_mut() else {
            return;
        };
        let response_id = turn.response_id.clone();
        match message {
            Some(TurnMsg::Sentence(text)) => {
                if !turn.spoke {
                    turn.spoke = true;
                    self.out.push_back(EngineEvent::AssistantAudioStarted {
                        response_id: Some(response_id.clone()),
                    });
                }
                self.out.push_back(EngineEvent::AssistantSpeech { response_id, text });
            }
            Some(TurnMsg::Done {
                text,
                tool_calls,
                usage,
            }) => {
                let spoke = turn.spoke;
                self.active = None;
                let mut message = json!({"role": "assistant", "content": text});
                if !tool_calls.is_empty() {
                    message["tool_calls"] = tool_calls
                        .iter()
                        .map(|c| {
                            json!({"id": c.id, "type": "function",
                            "function": {"name": c.name, "arguments": c.arguments}})
                        })
                        .collect();
                }
                self.push_history(message);
                if !text.is_empty() {
                    self.out.push_back(EngineEvent::Transcript {
                        role: TranscriptRole::Assistant,
                        text,
                        is_final: true,
                        stt_seconds: None,
                    });
                }
                if spoke {
                    self.out.push_back(EngineEvent::AssistantAudioFinished {
                        response_id: response_id.clone(),
                    });
                }
                for call in tool_calls {
                    self.outstanding_tools.push(call.id.clone());
                    self.out.push_back(EngineEvent::ToolCall {
                        call_id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                    });
                }
                self.out.push_back(EngineEvent::ResponseDone {
                    response_id,
                    status: "completed".into(),
                    usage,
                });
                self.start_queued();
            }
            Some(TurnMsg::Failed) | None => {
                let spoke = turn.spoke;
                self.active = None;
                self.out.push_back(EngineEvent::ProviderError {
                    code: "minimax_brain_failed".into(),
                });
                if !spoke {
                    self.out.push_back(EngineEvent::AssistantSpeech {
                        response_id: response_id.clone(),
                        text: BRAIN_FAILED_LINE.into(),
                    });
                }
                self.out.push_back(EngineEvent::ResponseDone {
                    response_id,
                    status: "failed".into(),
                    usage: None,
                });
                self.start_queued();
            }
        }
    }
}

#[async_trait::async_trait]
impl EngineCall for CascadeCall {
    fn call_id(&self) -> &str {
        self.ear.call_id()
    }

    async fn listen(&mut self) -> Result<(), EngineError> {
        self.ear.listen().await
    }

    async fn reattach(&mut self) -> Result<(), EngineError> {
        self.ear.reattach().await
    }

    async fn next_event(&mut self) -> Option<EngineEvent> {
        loop {
            if let Some(event) = self.out.pop_front() {
                return Some(event);
            }
            // Both arms are cancel-safe; state changes only after one completed.
            let next = {
                let ear = &mut self.ear;
                let active = &mut self.active;
                let answer_at = self.answer_at;
                tokio::select! {
                    _ = async {
                        match answer_at {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending().await,
                        }
                    } => Next::Answer,
                    event = ear.next_event() => Next::Ear(event),
                    message = async {
                        match active {
                            Some(turn) => turn.rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => Next::Turn(message),
                }
            };
            match next {
                Next::Ear(None) => return None,
                Next::Ear(Some(event)) => self.on_ear(event),
                Next::Turn(message) => self.on_turn(message),
                Next::Answer => self.answer_held_turn(),
            }
        }
    }

    /// The ear never answers; this only switches the brain's auto answers.
    async fn set_create_response(&mut self, enabled: bool) -> Result<(), EngineError> {
        self.auto_respond = enabled;
        if !enabled {
            self.answer_at = None;
            self.answer_held = false;
        }
        Ok(())
    }

    async fn create_response(&mut self) -> Result<(), EngineError> {
        self.announce_turn()
    }

    async fn speak(&mut self, text: &str, _mode: SpeakMode) -> Result<(), EngineError> {
        let response_id = self.next_id("mmspeak");
        self.out.push_back(EngineEvent::AssistantSpeech {
            response_id,
            text: text.to_owned(),
        });
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
        self.cancel_turns();
        self.answer_at = None;
        self.answer_held = false;
        Ok(())
    }

    async fn commit_input(&mut self) -> Result<(), EngineError> {
        self.ear.commit_input().await
    }

    async fn tool_result(&mut self, call_id: &str, output: &str) -> Result<(), EngineError> {
        self.push_history(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
        self.outstanding_tools.retain(|id| id != call_id);
        self.start_queued();
        Ok(())
    }

    async fn add_context(&mut self, text: &str) -> Result<(), EngineError> {
        self.push_history(json!({"role": "assistant", "content": text}));
        Ok(())
    }

    async fn close(&mut self, reason: &str) -> Result<(), EngineError> {
        if let Some(turn) = self.active.take() {
            turn.task.abort();
        }
        self.queued = None;
        self.answer_at = None;
        self.ear.close(reason).await
    }
}

impl Drop for CascadeCall {
    fn drop(&mut self) {
        if let Some(turn) = self.active.take() {
            turn.task.abort();
        }
    }
}
