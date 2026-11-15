//! `openai-realtime` adapter: WebRTC call created by the backend (multipart
//! SDP + server-owned session), control over the sideband WebSocket, hangup
//! endpoint. Wire facts verified live in S0-A (`design/voice/s0/openai.md`):
//! `POST /v1/realtime/calls` → 201 + `Location`, sideband
//! `wss://…/v1/realtime?call_id=…` (no `session.created` on attach — the
//! config is confirmed by `session.updated`), hangup → 200 then 404.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::openai_config::{
    assistant_item, create_response_update, full_update, function_call_output, response_cancel, response_create,
    speak_exact,
};
use super::openai_events::map_event;
use super::{
    EngineCall, EngineError, EngineEvent, EngineSessionConfig, EngineTimings, OpenCallRequest, SpeakMode, VoiceEngine,
};
use crate::credentials::ResolvedCredential;
use crate::ports::ApiKey;

pub const ENGINE_KIND: &str = "openai-realtime";

pub(crate) type Sideband = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Creates OpenAI Realtime WebRTC calls.
#[derive(Clone)]
pub struct OpenAiRealtimeEngine {
    http: reqwest::Client,
    timings: EngineTimings,
}

impl OpenAiRealtimeEngine {
    pub fn new(timings: EngineTimings) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timings.http_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http, timings }
    }
}

#[async_trait::async_trait]
impl VoiceEngine for OpenAiRealtimeEngine {
    fn kind(&self) -> &'static str {
        ENGINE_KIND
    }

    async fn open(&self, request: OpenCallRequest) -> Result<(String, Box<dyn EngineCall>), EngineError> {
        let OpenCallRequest {
            credential,
            sdp_offer,
            config,
        } = request;
        let url = format!("{}/v1/realtime/calls", credential.base_url);
        let session = super::openai_config::session_object(&config).to_string();
        let form = reqwest::multipart::Form::new()
            .text("sdp", sdp_offer)
            .text("session", session);
        let response = self
            .http
            .post(&url)
            .bearer_auth(credential.api_key.expose())
            .multipart(form)
            .send()
            .await
            .map_err(|e| EngineError::Transport(transport_message(&e)))?;
        let status = response.status().as_u16();
        if status != 201 && status != 200 {
            return Err(EngineError::Rejected(status));
        }
        let call_id = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(call_id_from_location)
            .ok_or_else(|| EngineError::Transport("call created without a Location call id".into()))?;
        let answer = response
            .text()
            .await
            .map_err(|e| EngineError::Transport(transport_message(&e)))?;
        if answer.trim().is_empty() {
            return Err(EngineError::Transport("call created without an SDP answer".into()));
        }
        let call = OpenAiCall::new(self.http.clone(), self.timings, credential, call_id, config);
        Ok((answer, Box::new(call)))
    }

    fn resume(
        &self,
        credential: ResolvedCredential,
        call_id: String,
        config: EngineSessionConfig,
    ) -> Box<dyn EngineCall> {
        Box::new(OpenAiCall::new(
            self.http.clone(),
            self.timings,
            credential,
            call_id,
            config,
        ))
    }
}

/// `Location: /v1/realtime/calls/rtc_…` → `rtc_…`.
fn call_id_from_location(location: &str) -> Option<String> {
    let path = location.split(['?', '#']).next()?;
    let id = path.trim_end_matches('/').rsplit('/').next()?;
    let valid =
        !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    valid.then(|| id.to_owned())
}

/// reqwest errors without the URL query (no secrets are in URLs, but keep it short).
fn transport_message(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "request timed out".into()
    } else if err.is_connect() {
        "connection failed".into()
    } else {
        "request failed".into()
    }
}

enum AttachOutcome {
    Retry,
    Gone,
    Failed(String),
}

pub struct OpenAiCall {
    http: reqwest::Client,
    timings: EngineTimings,
    base_url: String,
    api_key: ApiKey,
    call_id: String,
    config: EngineSessionConfig,
    ws: Option<Sideband>,
    pending: VecDeque<EngineEvent>,
    closed: bool,
}

impl OpenAiCall {
    fn new(
        http: reqwest::Client,
        timings: EngineTimings,
        credential: ResolvedCredential,
        call_id: String,
        config: EngineSessionConfig,
    ) -> Self {
        Self {
            http,
            timings,
            base_url: credential.base_url,
            api_key: credential.api_key,
            call_id,
            config,
            ws: None,
            pending: VecDeque::new(),
            closed: false,
        }
    }

    fn sideband_url(&self) -> String {
        let ws_base = if let Some(rest) = self.base_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.base_url.clone()
        };
        format!("{ws_base}/v1/realtime?call_id={}", self.call_id)
    }

    async fn connect_once(&self) -> Result<Sideband, AttachOutcome> {
        let url = self.sideband_url();
        let mut request = url
            .into_client_request()
            .map_err(|_| AttachOutcome::Failed("invalid sideband URL".into()))?;
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose()))
            .map_err(|_| AttachOutcome::Failed("API key is not a valid header value".into()))?;
        request.headers_mut().insert("Authorization", auth);
        let connector = aionui_shell::build_ws_connector().map_err(|e| AttachOutcome::Failed(e.to_string()))?;
        let connect = tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector));
        match tokio::time::timeout(self.timings.attach_window, connect).await {
            Err(_) => Err(AttachOutcome::Failed("sideband connect timed out".into())),
            Ok(Ok((ws, _))) => Ok(ws),
            Ok(Err(tungstenite::Error::Http(response))) => match response.status().as_u16() {
                503 => Err(AttachOutcome::Retry),
                404 => Err(AttachOutcome::Gone),
                status => Err(AttachOutcome::Failed(format!("sideband handshake HTTP {status}"))),
            },
            Ok(Err(_)) => Err(AttachOutcome::Failed("sideband connect failed".into())),
        }
    }

    /// Attach the sideband, retrying 503 for up to `attach_window` (§3.6.2).
    async fn attach(&mut self) -> Result<(), EngineError> {
        self.ws = None;
        let deadline = Instant::now() + self.timings.attach_window;
        loop {
            match self.connect_once().await {
                Ok(ws) => {
                    self.ws = Some(ws);
                    return Ok(());
                }
                Err(AttachOutcome::Gone) => return Err(EngineError::CallGone),
                Err(AttachOutcome::Failed(msg)) => return Err(EngineError::AttachFailed(msg)),
                Err(AttachOutcome::Retry) => {
                    if Instant::now() + self.timings.attach_retry_delay >= deadline {
                        return Err(EngineError::AttachFailed("provider kept answering 503".into()));
                    }
                    tracing::debug!(call_id = %self.call_id, "voice sideband attach got 503; retrying");
                    tokio::time::sleep(self.timings.attach_retry_delay).await;
                }
            }
        }
    }

    async fn send(&mut self, value: Value) -> Result<(), EngineError> {
        let ws = self.ws.as_mut().ok_or(EngineError::NotAttached)?;
        ws.send(Message::Text(value.to_string().into())).await.map_err(|_| {
            self.ws = None;
            EngineError::Transport("sideband send failed".into())
        })
    }

    /// Send a `session.update` and wait for its `session.updated` (the gate).
    /// Other events that arrive meanwhile are kept for `next_event`.
    async fn apply_config(&mut self, update: Value) -> Result<(), EngineError> {
        self.send(update).await?;
        let deadline = Instant::now() + self.timings.config_gate;
        loop {
            let ws = self.ws.as_mut().ok_or(EngineError::NotAttached)?;
            let next = match tokio::time::timeout_at(deadline, ws.next()).await {
                Err(_) => return Err(EngineError::ConfigTimeout),
                Ok(next) => next,
            };
            match next {
                Some(Ok(Message::Text(text))) => match parse_and_map(&text) {
                    Some(EngineEvent::ConfigApplied) => return Ok(()),
                    Some(EngineEvent::ProviderError { code }) => {
                        return Err(EngineError::AttachFailed(format!("config rejected: {code}")));
                    }
                    Some(event) => self.pending.push_back(event),
                    None => {}
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    self.ws = None;
                    return Err(EngineError::AttachFailed("sideband closed during config".into()));
                }
                Some(Ok(_)) => {}
            }
        }
    }

    async fn attach_and_configure(&mut self) -> Result<(), EngineError> {
        self.attach().await?;
        let update = full_update(&self.config);
        if let Err(err) = self.apply_config(update).await {
            self.ws = None;
            return Err(err);
        }
        Ok(())
    }
}

fn parse_and_map(text: &str) -> Option<EngineEvent> {
    let value: Value = serde_json::from_str(text).ok()?;
    if let Some(kind) = value.get("type").and_then(Value::as_str) {
        tracing::debug!(event_type = kind, "voice provider event");
    }
    map_event(&value)
}

#[async_trait::async_trait]
impl EngineCall for OpenAiCall {
    fn call_id(&self) -> &str {
        &self.call_id
    }

    async fn listen(&mut self) -> Result<(), EngineError> {
        self.attach_and_configure().await
    }

    async fn reattach(&mut self) -> Result<(), EngineError> {
        self.attach_and_configure().await
    }

    async fn next_event(&mut self) -> Option<EngineEvent> {
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        loop {
            let ws = self.ws.as_mut()?;
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Some(event) = parse_and_map(&text) {
                        return Some(event);
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    self.ws = None;
                    return (!self.closed).then_some(EngineEvent::ControlLost);
                }
                Some(Ok(_)) => {}
            }
        }
    }

    async fn set_create_response(&mut self, enabled: bool) -> Result<(), EngineError> {
        let previous = self.config.create_response;
        self.config.create_response = enabled;
        let result = self.apply_config(create_response_update(&self.config)).await;
        if result.is_err() {
            self.config.create_response = previous;
        }
        result
    }

    async fn create_response(&mut self) -> Result<(), EngineError> {
        self.send(response_create(self.config.max_output_tokens)).await
    }

    async fn speak(&mut self, text: &str, mode: SpeakMode) -> Result<(), EngineError> {
        match mode {
            SpeakMode::Exact => self.send(speak_exact(text, self.config.max_output_tokens)).await,
        }
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
        self.send(response_cancel()).await
    }

    async fn tool_result(&mut self, call_id: &str, output: &str) -> Result<(), EngineError> {
        self.send(function_call_output(call_id, output)).await
    }

    async fn add_context(&mut self, text: &str) -> Result<(), EngineError> {
        self.send(assistant_item(text)).await
    }

    async fn close(&mut self, reason: &str) -> Result<(), EngineError> {
        self.closed = true;
        let url = format!("{}/v1/realtime/calls/{}/hangup", self.base_url, self.call_id);
        let result = self
            .http
            .post(&url)
            .bearer_auth(self.api_key.expose())
            .send()
            .await
            .map_err(|e| EngineError::Transport(transport_message(&e)));
        if let Some(mut ws) = self.ws.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), ws.close(None)).await;
        }
        let status = result?.status().as_u16();
        tracing::debug!(call_id = %self.call_id, status, reason, "voice provider hangup");
        match status {
            200..=299 | 404 => Ok(()),
            other => Err(EngineError::Rejected(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_id_is_the_last_location_segment() {
        assert_eq!(
            call_id_from_location("/v1/realtime/calls/rtc_u0_abc").as_deref(),
            Some("rtc_u0_abc")
        );
        assert_eq!(
            call_id_from_location("https://api.openai.com/v1/realtime/calls/rtc_1/").as_deref(),
            Some("rtc_1")
        );
        assert_eq!(
            call_id_from_location("/v1/realtime/calls/rtc_1?x=1").as_deref(),
            Some("rtc_1")
        );
        assert_eq!(call_id_from_location("/v1/realtime/calls/../../x y"), None);
        assert_eq!(call_id_from_location(""), None);
    }
}
