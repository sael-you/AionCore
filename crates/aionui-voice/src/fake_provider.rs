//! In-process fake OpenAI Realtime provider (`--features voice-test` only).
//!
//! Mirrors the wire behaviour verified live in S0-A: multipart call creation
//! → 201 + `Location`, sideband WebSocket with no `session.created` (the first
//! event answers our first command), `session.update` → `session.updated`,
//! `response.create` → `response.created` + `response.done` with usage,
//! hangup → 200 then 404 and the sideband is closed. Knobs let tests inject
//! 503s on attach, a missing `session.updated`, dropped sidebands and events.
#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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
    attach_503: u32,
    ack_session_update: bool,
    create_status: Option<u16>,
    usage: Value,
}

struct FakeState {
    calls: Mutex<HashMap<String, FakeCall>>,
    created_sessions: Mutex<Vec<Value>>,
    created_sdps: Mutex<Vec<String>>,
    bearer_seen: Mutex<Vec<String>>,
    knobs: Mutex<Knobs>,
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
                attach_503: 0,
                ack_session_update: true,
                create_status: None,
                usage: captured_usage(),
            }),
            next_id: AtomicU64::new(1),
            attach_attempts: AtomicU32::new(0),
        });
        let app = Router::new()
            .route("/v1/realtime/calls", post(create_call))
            .route("/v1/realtime/calls/{id}/hangup", post(hangup))
            .route("/v1/realtime", get(sideband))
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
    (
        StatusCode::CREATED,
        [
            (header::LOCATION, format!("/v1/realtime/calls/{id}")),
            (header::CONTENT_TYPE, "application/sdp".to_owned()),
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
            for tx in call.sidebands.drain(..) {
                let _ = tx.send(Outbound::Close);
            }
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
