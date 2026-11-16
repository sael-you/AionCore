//! In-process fake OpenAI Realtime provider (`--features voice-test` only).
//!
//! Mirrors the wire behaviour verified live in S0-A: multipart call creation
//! → 201 + `Location`, sideband WebSocket with no `session.created` (the first
//! event answers our first command), `session.update` → `session.updated`,
//! `response.create` → `response.created` + `response.done` with usage,
//! `output_audio_buffer.started` ≈ 0.5 s after `response.create` and
//! `output_audio_buffer.stopped` 2.9–6.2 s after `response.done` (real-time
//! paced playout), hangup → 200 then 404 with the sideband closed ≈ 2 s later,
//! SDP answer as `text/plain`. Knobs let tests inject 503s on attach, a missing
//! `session.updated`, dropped sidebands, events and shorter delays.
#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Multipart, Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Usage block of a real `response.done` (S0-A capture, gpt-realtime-2.1-mini).
pub fn captured_usage() -> Value {
    json!({"total_tokens": 519, "input_tokens": 356, "output_tokens": 163,
        "input_token_details": {"text_tokens": 356, "audio_tokens": 0, "image_tokens": 0, "cached_tokens": 0,
            "cached_tokens_details": {"text_tokens": 0, "audio_tokens": 0, "image_tokens": 0}},
        "output_token_details": {"text_tokens": 90, "audio_tokens": 73, "reasoning_tokens": 64}})
}

enum Outbound {
    Event(Value),
    Close,
}

#[derive(Default)]
struct FakeCall {
    hung_up: bool,
    sidebands: Vec<mpsc::UnboundedSender<Outbound>>,
    received: Vec<Value>,
    hangups: u32,
}

struct Knobs {
    /// JSON text returned by `/v1/responses` (refiner).
    refinement: Option<Value>,
    /// Text returned by `/v1/audio/transcriptions`; `None` echoes the audio bytes.
    transcript: Option<String>,
    attach_503: u32,
    ack_session_update: bool,
    create_status: Option<u16>,
    usage: Value,
    audio_started_after: Duration,
    audio_stopped_after_done: Duration,
    hangup_close_after: Duration,
    create_delay: Duration,
    /// MiniMax chat completions: scripted answers, one per request, in order.
    chat_script: std::collections::VecDeque<Value>,
    /// Delay before the first streamed chunk and between chunks.
    chat_first_token: Duration,
    chat_chunk_gap: Duration,
}

struct FakeState {
    calls: Mutex<HashMap<String, FakeCall>>,
    created_sessions: Mutex<Vec<Value>>,
    created_sdps: Mutex<Vec<String>>,
    bearer_seen: Mutex<Vec<String>>,
    knobs: Mutex<Knobs>,
    chat_requests: Mutex<Vec<Value>>,
    next_id: AtomicU64,
    attach_attempts: AtomicU32,
}

/// A running fake provider bound to `127.0.0.1:<random>`.
pub struct FakeOpenAi {
    base_url: String,
    state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeOpenAi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeOpenAi {
    pub async fn start() -> Self {
        let state = Arc::new(FakeState {
            calls: Mutex::new(HashMap::new()),
            created_sessions: Mutex::new(Vec::new()),
            created_sdps: Mutex::new(Vec::new()),
            bearer_seen: Mutex::new(Vec::new()),
            knobs: Mutex::new(Knobs {
                refinement: None,
                transcript: None,
                attach_503: 0,
                ack_session_update: true,
                create_status: None,
                usage: captured_usage(),
                audio_started_after: Duration::from_millis(500),
                audio_stopped_after_done: Duration::from_millis(2_900),
                hangup_close_after: Duration::from_secs(2),
                create_delay: Duration::ZERO,
                chat_script: Default::default(),
                chat_first_token: Duration::ZERO,
                chat_chunk_gap: Duration::ZERO,
            }),
            chat_requests: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            attach_attempts: AtomicU32::new(0),
        });
        let app = Router::new()
            .route("/v1/realtime/calls", post(create_call))
            .route("/v1/realtime/calls/{id}/hangup", post(hangup))
            .route("/v1/realtime", get(sideband))
            .route("/v1/responses", post(responses))
            .route("/v1/audio/transcriptions", post(transcriptions))
            .route("/ws/v1/t2a_v2", get(minimax_tts))
            .route("/v1/chat/completions", post(chat_completions))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake provider");
        let addr = listener.local_addr().expect("fake provider addr");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{addr}"),
            state,
            task,
        }
    }

    /// Script the MiniMax brain: each entry answers one chat request, in order:
    /// `{"text": "...", "tool_calls": [{"name", "arguments"}], "status": 500}`
    /// (all optional). Unscripted requests answer `{"text": "OK."}`.
    pub fn set_chat_script(&self, answers: Vec<Value>) {
        self.state.knobs.lock().unwrap().chat_script = answers.into();
    }

    /// Streaming pace of the brain: time to first chunk, then between chunks.
    pub fn set_chat_timing(&self, first_token: Duration, chunk_gap: Duration) {
        let mut knobs = self.state.knobs.lock().unwrap();
        knobs.chat_first_token = first_token;
        knobs.chat_chunk_gap = chunk_gap;
    }

    /// Chat request bodies received, in order (with the bearer under `_bearer`).
    pub fn chat_requests(&self) -> Vec<Value> {
        self.state.chat_requests.lock().unwrap().clone()
    }

    /// Base URL to use as the provider origin (no `/v1`).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn set_attach_503(&self, count: u32) {
        self.state.knobs.lock().unwrap().attach_503 = count;
    }

    pub fn set_ack_session_update(&self, ack: bool) {
        self.state.knobs.lock().unwrap().ack_session_update = ack;
    }

    pub fn set_create_status(&self, status: Option<u16>) {
        self.state.knobs.lock().unwrap().create_status = status;
    }

    /// Playout pacing of `output_audio_buffer.started/stopped`.
    pub fn set_audio_pacing(&self, started_after: Duration, stopped_after_done: Duration) {
        let mut knobs = self.state.knobs.lock().unwrap();
        knobs.audio_started_after = started_after;
        knobs.audio_stopped_after_done = stopped_after_done;
    }

    /// Structured output the refiner endpoint returns.
    pub fn set_refinement(&self, refinement: Value) {
        self.state.knobs.lock().unwrap().refinement = Some(refinement);
    }

    /// What the transcription endpoint hears (`None` echoes the clip bytes).
    pub fn set_transcript(&self, transcript: Option<&str>) {
        self.state.knobs.lock().unwrap().transcript = transcript.map(str::to_owned);
    }

    /// Delay before the call-creation answer (call already registered).
    pub fn set_create_delay(&self, delay: Duration) {
        self.state.knobs.lock().unwrap().create_delay = delay;
    }

    /// Delay between a hangup (200) and the provider closing the sidebands.
    pub fn set_hangup_close_delay(&self, delay: Duration) {
        self.state.knobs.lock().unwrap().hangup_close_after = delay;
    }

    /// `null` simulates a response whose usage is not reported.
    pub fn set_usage(&self, usage: Value) {
        self.state.knobs.lock().unwrap().usage = usage;
    }

    pub fn call_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.state.calls.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn created_sessions(&self) -> Vec<Value> {
        self.state.created_sessions.lock().unwrap().clone()
    }

    pub fn created_sdps(&self) -> Vec<String> {
        self.state.created_sdps.lock().unwrap().clone()
    }

    /// Bearer tokens seen on call creation (tests assert the right key was used).
    pub fn bearer_tokens(&self) -> Vec<String> {
        self.state.bearer_seen.lock().unwrap().clone()
    }

    pub fn attach_attempts(&self) -> u32 {
        self.state.attach_attempts.load(Ordering::SeqCst)
    }

    /// Client events the sidebands of `call_id` received, in order.
    pub fn received(&self, call_id: &str) -> Vec<Value> {
        self.state
            .calls
            .lock()
            .unwrap()
            .get(call_id)
            .map(|c| c.received.clone())
            .unwrap_or_default()
    }

    pub fn received_types(&self, call_id: &str) -> Vec<String> {
        self.received(call_id)
            .iter()
            .filter_map(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    pub fn hangups(&self, call_id: &str) -> u32 {
        self.state.calls.lock().unwrap().get(call_id).map_or(0, |c| c.hangups)
    }

    pub fn is_hung_up(&self, call_id: &str) -> bool {
        self.state.calls.lock().unwrap().get(call_id).is_some_and(|c| c.hung_up)
    }

    pub fn open_sidebands(&self, call_id: &str) -> usize {
        self.state
            .calls
            .lock()
            .unwrap()
            .get(call_id)
            .map_or(0, |c| c.sidebands.iter().filter(|tx| !tx.is_closed()).count())
    }

    /// Push a server event to every sideband of the call.
    pub fn push_event(&self, call_id: &str, event: Value) {
        if let Some(call) = self.state.calls.lock().unwrap().get(call_id) {
            for tx in &call.sidebands {
                let _ = tx.send(Outbound::Event(event.clone()));
            }
        }
    }

    /// Close the sidebands without ending the call (network blip).
    pub fn drop_sidebands(&self, call_id: &str) {
        if let Some(call) = self.state.calls.lock().unwrap().get_mut(call_id) {
            for tx in call.sidebands.drain(..) {
                let _ = tx.send(Outbound::Close);
            }
        }
    }

    /// The provider ends the call on its own (expiry).
    pub fn end_call_from_provider(&self, call_id: &str) {
        if let Some(call) = self.state.calls.lock().unwrap().get_mut(call_id) {
            call.hung_up = true;
            for tx in call.sidebands.drain(..) {
                let _ = tx.send(Outbound::Close);
            }
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned)
}

async fn create_call(State(state): State<Arc<FakeState>>, headers: HeaderMap, mut multipart: Multipart) -> Response {
    let Some(token) = bearer(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let mut sdp = None;
    let mut session = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or_default().to_owned();
        let text = field.text().await.unwrap_or_default();
        match name.as_str() {
            "sdp" => sdp = Some(text),
            "session" => session = serde_json::from_str::<Value>(&text).ok(),
            _ => {}
        }
    }
    let (Some(sdp), Some(session)) = (sdp, session) else {
        return (StatusCode::BAD_REQUEST, "multipart sdp + session required").into_response();
    };
    state.bearer_seen.lock().unwrap().push(token);
    if let Some(status) = state.knobs.lock().unwrap().create_status {
        return StatusCode::from_u16(status)
            .unwrap_or(StatusCode::BAD_REQUEST)
            .into_response();
    }
    state.created_sdps.lock().unwrap().push(sdp);
    state.created_sessions.lock().unwrap().push(session);
    let id = format!("rtc_fake_{}", state.next_id.fetch_add(1, Ordering::SeqCst));
    state.calls.lock().unwrap().insert(id.clone(), FakeCall::default());
    let delay = state.knobs.lock().unwrap().create_delay;
    tokio::time::sleep(delay).await;
    (
        StatusCode::CREATED,
        [
            (header::LOCATION, format!("/v1/realtime/calls/{id}")),
            (header::CONTENT_TYPE, "text/plain".to_owned()),
        ],
        "v=0\r\no=fake-answer 1 1 IN IP4 127.0.0.1\r\n",
    )
        .into_response()
}

async fn hangup(State(state): State<Arc<FakeState>>, Path(id): Path<String>) -> Response {
    let mut calls = state.calls.lock().unwrap();
    match calls.get_mut(&id) {
        Some(call) if !call.hung_up => {
            call.hung_up = true;
            call.hangups += 1;
            let sidebands: Vec<_> = call.sidebands.drain(..).collect();
            let delay = state.knobs.lock().unwrap().hangup_close_after;
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                for tx in sidebands {
                    let _ = tx.send(Outbound::Close);
                }
            });
            StatusCode::OK.into_response()
        }
        Some(call) => {
            call.hangups += 1;
            (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": {"code": "call_id_not_found"}})),
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": {"code": "call_id_not_found"}})),
        )
            .into_response(),
    }
}

async fn sideband(
    State(state): State<Arc<FakeState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    state.attach_attempts.fetch_add(1, Ordering::SeqCst);
    if bearer(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !query.contains_key("call_id") && query.contains_key("model") {
        // A plain realtime session (read-back fallback), not a call sideband.
        return upgrade.on_upgrade(run_mini_session);
    }
    let Some(call_id) = query.get("call_id").cloned() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    {
        let mut knobs = state.knobs.lock().unwrap();
        if knobs.attach_503 > 0 {
            knobs.attach_503 -= 1;
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    let (tx, rx) = mpsc::unbounded_channel();
    {
        let mut calls = state.calls.lock().unwrap();
        match calls.get_mut(&call_id) {
            Some(call) if !call.hung_up => call.sidebands.push(tx),
            _ => return StatusCode::NOT_FOUND.into_response(),
        }
    }
    upgrade.on_upgrade(move |socket| run_sideband(socket, state, call_id, rx))
}

async fn run_sideband(
    socket: WebSocket,
    state: Arc<FakeState>,
    call_id: String,
    mut rx: mpsc::UnboundedReceiver<Outbound>,
) {
    let (mut sink, mut stream) = socket.split();
    let mut event_no = 0u64;
    loop {
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(Message::Text(text))) = incoming else { break };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                if let Some(call) = state.calls.lock().unwrap().get_mut(&call_id) {
                    call.received.push(value.clone());
                }
                for reply in replies(&state, &value, &mut event_no) {
                    if reply["type"] == "response.done" {
                        schedule_playout(&state, &call_id, &reply);
                    }
                    if sink.send(Message::Text(reply.to_string().into())).await.is_err() {
                        return;
                    }
                }
            }
            outbound = rx.recv() => match outbound {
                Some(Outbound::Event(event)) => {
                    if sink.send(Message::Text(event.to_string().into())).await.is_err() {
                        return;
                    }
                }
                Some(Outbound::Close) | None => {
                    let _ = sink.send(Message::Close(None)).await;
                    return;
                }
            },
        }
    }
}

/// Paced playout of a completed response, sent to every sideband of the call.
fn schedule_playout(state: &Arc<FakeState>, call_id: &str, done: &Value) {
    let Some(response_id) = done["response"]["id"].as_str().map(str::to_owned) else {
        return;
    };
    let (started_after, stopped_after) = {
        let knobs = state.knobs.lock().unwrap();
        (knobs.audio_started_after, knobs.audio_stopped_after_done)
    };
    let state = state.clone();
    let call_id = call_id.to_owned();
    tokio::spawn(async move {
        let send = |event: Value| {
            if let Some(call) = state.calls.lock().unwrap().get(&call_id) {
                for tx in &call.sidebands {
                    let _ = tx.send(Outbound::Event(event.clone()));
                }
            }
        };
        tokio::time::sleep(started_after).await;
        send(json!({"type": "output_audio_buffer.started", "response_id": response_id}));
        tokio::time::sleep(stopped_after.saturating_sub(started_after)).await;
        send(json!({"type": "output_audio_buffer.stopped", "response_id": response_id}));
    });
}

fn replies(state: &FakeState, event: &Value, event_no: &mut u64) -> Vec<Value> {
    let mut next_event_id = || {
        *event_no += 1;
        format!("event_fake_{event_no}")
    };
    match event.get("type").and_then(Value::as_str) {
        Some("session.update") => {
            if !state.knobs.lock().unwrap().ack_session_update {
                return Vec::new();
            }
            vec![json!({"type": "session.updated", "event_id": next_event_id(),
                "session": event.get("session").cloned().unwrap_or(Value::Null)})]
        }
        Some("response.create") => {
            let id = format!("resp_fake_{}", state.next_id.fetch_add(1, Ordering::SeqCst));
            let usage = state.knobs.lock().unwrap().usage.clone();
            vec![
                json!({"type": "response.created", "event_id": next_event_id(),
                    "response": {"id": id, "status": "in_progress", "usage": null}}),
                json!({"type": "response.done", "event_id": next_event_id(),
                    "response": {"id": id, "status": "completed", "usage": usage}}),
            ]
        }
        _ => Vec::new(),
    }
}

/// MiniMax chat completions (OpenAI-compatible SSE, S0-B #2): the scripted
/// answer streamed in small content pieces, tool calls in two fragments each,
/// a usage chunk, then `[DONE]`.
async fn chat_completions(State(state): State<Arc<FakeState>>, headers: HeaderMap, body: String) -> Response {
    let Some(token) = bearer(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let mut request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    request["_bearer"] = Value::String(token);
    state.chat_requests.lock().unwrap().push(request);
    let (answer, first_token, gap) = {
        let mut knobs = state.knobs.lock().unwrap();
        let answer = knobs.chat_script.pop_front().unwrap_or_else(|| json!({"text": "OK."}));
        (answer, knobs.chat_first_token, knobs.chat_chunk_gap)
    };
    if let Some(status) = answer["status"].as_u64() {
        return StatusCode::from_u16(status as u16)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }
    let mut chunks: Vec<Value> = Vec::new();
    let text = answer["text"].as_str().unwrap_or_default();
    let chars: Vec<char> = text.chars().collect();
    for piece in chars.chunks(6) {
        let piece: String = piece.iter().collect();
        chunks.push(json!({"choices": [{"index": 0, "delta": {"content": piece}}]}));
    }
    for (index, call) in answer["tool_calls"].as_array().into_iter().flatten().enumerate() {
        let args = call["arguments"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| call["arguments"].to_string());
        let (a, b) = args.split_at(args.len() / 2);
        chunks.push(
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": index,
            "id": format!("call_fake_{index}"), "type": "function",
            "function": {"name": call["name"], "arguments": a}}]}}]}),
        );
        chunks.push(
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": index,
            "function": {"arguments": b}}]}}]}),
        );
    }
    chunks.push(
        json!({"choices": [], "usage": {"prompt_tokens": 1000, "completion_tokens": 40,
        "total_tokens": 1040, "prompt_tokens_details": {"cached_tokens": 800}}}),
    );
    let mut lines: Vec<String> = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    lines.push("data: [DONE]\n\n".to_owned());
    let stream = futures_util::stream::unfold((lines.into_iter(), true), move |(mut it, first)| async move {
        let line = it.next()?;
        tokio::time::sleep(if first { first_token } else { gap }).await;
        Some((Ok::<_, std::convert::Infallible>(line), (it, false)))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

/// Responses API (refiner): the configured structured output as `output_text`.
async fn responses(State(state): State<Arc<FakeState>>, headers: HeaderMap) -> Response {
    if bearer(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(refinement) = state.knobs.lock().unwrap().refinement.clone() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    axum::Json(json!({"output": [{"type": "message", "content": [
        {"type": "output_text", "text": refinement.to_string()}
    ]}]}))
    .into_response()
}

/// Transcription (name check): echo the clip text unless a transcript is set.
async fn transcriptions(State(state): State<Arc<FakeState>>, headers: HeaderMap, mut multipart: Multipart) -> Response {
    if bearer(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut audio = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("file") {
            audio = field.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        }
    }
    let text = state
        .knobs
        .lock()
        .unwrap()
        .transcript
        .clone()
        .unwrap_or_else(|| String::from_utf8_lossy(&audio).into_owned());
    axum::Json(json!({"text": text})).into_response()
}

/// MiniMax `t2a_v2` WebSocket (S0-B wire format): the "audio" is the text's
/// bytes, hex-encoded, so tests can read back what was spoken.
async fn minimax_tts(headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    if bearer(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade.on_upgrade(|socket| async move {
        let (mut sink, mut stream) = socket.split();
        let send = |v: Value| Message::Text(v.to_string().into());
        if sink.send(send(json!({"event": "connected_success"}))).await.is_err() {
            return;
        }
        while let Some(Ok(Message::Text(text))) = stream.next().await {
            let Ok(event) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            match event["event"].as_str() {
                Some("task_start") => {
                    let _ = sink.send(send(json!({"event": "task_started"}))).await;
                }
                Some("task_continue") => {
                    let spoken = event["text"].as_str().unwrap_or_default();
                    let _ = sink
                        .send(send(
                            json!({"data": {"audio": hex::encode(spoken.as_bytes()), "status": 1}, "is_final": false}),
                        ))
                        .await;
                    let _ = sink
                        .send(send(json!({"data": {"audio": "", "status": 2}, "is_final": true})))
                        .await;
                }
                Some("task_finish") => break,
                _ => {}
            }
        }
    })
}

/// Text-in/audio-out realtime session (read-back fallback): speaks exactly
/// the text after "Say exactly the following text and nothing else: ".
async fn run_mini_session(socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let send = |v: Value| Message::Text(v.to_string().into());
    while let Some(Ok(Message::Text(text))) = stream.next().await {
        let Ok(event) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        match event["type"].as_str() {
            Some("session.update") => {
                let _ = sink
                    .send(send(json!({"type": "session.updated", "session": event["session"]})))
                    .await;
            }
            Some("response.create") => {
                let instructions = event["response"]["instructions"].as_str().unwrap_or_default();
                let spoken = instructions
                    .split_once(": ")
                    .map(|(_, t)| t)
                    .unwrap_or(instructions)
                    .to_owned();
                use base64::Engine as _;
                let audio = base64::engine::general_purpose::STANDARD.encode(spoken.as_bytes());
                let _ = sink
                    .send(send(json!({"type": "response.output_audio.delta", "delta": audio})))
                    .await;
                let _ = sink
                    .send(send(
                        json!({"type": "response.output_audio_transcript.done", "transcript": spoken}),
                    ))
                    .await;
                let _ = sink
                    .send(send(json!({"type": "response.done", "response": {"id": "resp_mini"}})))
                    .await;
            }
            _ => {}
        }
    }
}
