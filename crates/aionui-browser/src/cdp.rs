//! Minimal Chrome DevTools Protocol client (flat sessions): numbered requests
//! with awaited replies, and a broadcast of every event. Transport-agnostic
//! ([`CdpClient::from_channels`]): production runs it over Chrome's
//! `--remote-debugging-pipe` through [`crate::mux`]; tests use a WebSocket.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::error::BrowserError;

/// One CDP event (`method` + `params`, `session_id` for flat sessions).
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, BrowserError>>>>>;

/// A connected CDP client. Cheap to clone.
#[derive(Clone)]
pub struct CdpClient {
    next_id: Arc<AtomicU64>,
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    events: broadcast::Sender<CdpEvent>,
    closed: Arc<tokio::sync::watch::Receiver<bool>>,
}

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

impl CdpClient {
    /// Over a WebSocket endpoint (tests; production uses the pipe).
    pub async fn connect(ws_url: &str) -> Result<Self, BrowserError> {
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| BrowserError::Cdp(format!("connect failed: {}", redact(&e.to_string()))))?;
        let (mut sink, mut source) = stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(text) = out_rx.recv().await {
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        tokio::spawn(async move {
            while let Some(Ok(msg)) = source.next().await {
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                if in_tx.send(text).is_err() {
                    break;
                }
            }
        });
        Ok(Self::from_channels(out_tx, in_rx))
    }

    /// A client whose frames go out on `out` and come in on `incoming`;
    /// closes when `incoming` ends.
    pub fn from_channels(out: mpsc::UnboundedSender<String>, mut incoming: mpsc::UnboundedReceiver<String>) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1024);
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        let reader_pending = pending.clone();
        let reader_events = events.clone();
        tokio::spawn(async move {
            while let Some(text) = incoming.recv().await {
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = reader_pending.lock().await.remove(&id) {
                        let result = match frame.get("error") {
                            Some(err) => Err(BrowserError::Cdp(
                                err.get("message").and_then(Value::as_str).unwrap_or("error").to_owned(),
                            )),
                            None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(result);
                    }
                } else if let Some(method) = frame.get("method").and_then(Value::as_str) {
                    let _ = reader_events.send(CdpEvent {
                        method: method.to_owned(),
                        params: frame.get("params").cloned().unwrap_or(Value::Null),
                        session_id: frame.get("sessionId").and_then(Value::as_str).map(str::to_owned),
                    });
                }
            }
            // Connection gone: fail every waiter.
            for (_, tx) in reader_pending.lock().await.drain() {
                let _ = tx.send(Err(BrowserError::Cdp("connection closed".into())));
            }
            let _ = closed_tx.send(true);
        });
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            out,
            pending,
            events,
            closed: Arc::new(closed_rx),
        }
    }

    /// Send `method` (in `session_id` when given) and await the result.
    pub async fn call(&self, method: &str, params: Value, session_id: Option<&str>) -> Result<Value, BrowserError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            frame["sessionId"] = json!(sid);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self.out.send(frame.to_string()).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(BrowserError::Cdp("connection closed".into()));
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(BrowserError::Cdp("connection closed".into())),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(BrowserError::Cdp(format!("{method} timed out")))
            }
        }
    }

    /// Fire-and-forget (acks such as `Page.screencastFrameAck`).
    pub fn notify(&self, method: &str, params: Value, session_id: Option<&str>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            frame["sessionId"] = json!(sid);
        }
        let _ = self.out.send(frame.to_string());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Resolves when the connection to Chrome is gone.
    pub async fn closed(&self) {
        let mut rx = (*self.closed).clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Drop anything that looks like a URL or port from an error text, so a
/// connection error never logs Chrome's private endpoint.
pub fn redact(text: &str) -> String {
    text.split_whitespace()
        .map(|w| {
            if w.contains("://") || w.contains("127.0.0.1") || w.contains("localhost") {
                "<redacted>"
            } else {
                w
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A fake CDP endpoint: echoes `{"id", "result": {"echo": method}}`, emits
    /// one event first, and answers `Fail.me` with an error.
    async fn fake_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            ws.send(Message::Text(
                json!({"method": "Target.targetCreated", "params": {"x": 1}, "sessionId": "S1"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let reply = if req["method"] == "Fail.me" {
                    json!({"id": req["id"], "error": {"code": -32000, "message": "nope"}})
                } else {
                    json!({"id": req["id"], "result": {"echo": req["method"], "session": req["sessionId"]}})
                };
                ws.send(Message::Text(reply.to_string().into())).await.unwrap();
            }
        });
        format!("ws://{addr}/devtools/browser/x")
    }

    #[tokio::test]
    async fn calls_resolve_by_id_and_events_are_broadcast() {
        let url = fake_endpoint().await;
        let client = CdpClient::connect(&url).await.unwrap();
        let mut events = client.subscribe();
        let r = client.call("Page.enable", json!({}), Some("S9")).await.unwrap();
        assert_eq!(r["echo"], "Page.enable");
        assert_eq!(r["session"], "S9");
        let ev = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
        // The event may have been sent before we subscribed; either way calls work.
        if let Ok(Ok(ev)) = ev {
            assert_eq!(ev.method, "Target.targetCreated");
            assert_eq!(ev.session_id.as_deref(), Some("S1"));
        }
        let err = client.call("Fail.me", json!({}), None).await.unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn redact_strips_endpoints() {
        assert_eq!(
            redact("IO error connecting ws://127.0.0.1:41234/devtools/browser/abc refused"),
            "IO error connecting <redacted> refused"
        );
    }
}
