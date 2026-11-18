//! The agents' Playwright endpoint of a Camoufox instance (TASK-136 K6):
//! `ws://127.0.0.1:<bridge port>/playwright`, same listener, token header and
//! "Agents can use the browser" switch as the CDP bridge ([`crate::bridge`]).
//!
//! One agent connection = one upstream connection to the sidecar's own
//! endpoint; every frame passes [`crate::pwfilter`]:
//! - agent → sidecar: decided by [`PwFilter::on_client_message`]; refused
//!   calls are answered in Playwright's error format, `Browser.close` is
//!   answered locally and ends the connection (detach, the instance keeps
//!   running for the owner);
//! - sidecar → agent: forwarded after the filter learned the object types
//!   it creates;
//! - both directions: `Cookie`/`Set-Cookie` headers are removed from every
//!   headers list or map in the frame (response initializers and raw-header
//!   results would otherwise carry HttpOnly cookies).
//!
//! Playwright refuses a client of another minor version
//! (`playwrightServer.ts` `userAgentVersionMatchesErrorMessage`), so the
//! agent's `User-Agent` and `x-playwright-*` headers are passed upstream as
//! sent: the pinned client (`@playwright/cli` 0.1.17) matches the sidecar.

use std::sync::Arc;

use axum::extract::ws::{Message as AxMessage, WebSocket};
use axum::http::HeaderMap;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message as TgMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::guard::NavigationGuard;
use crate::juggler::JugglerInstance;
use crate::pwfilter::{PwFilter, Verdict, empty_result, refusal, strip_cookie_headers};

/// Largest frame either way (screenshots travel base64 in JSON).
pub const MAX_FRAME: usize = 256 * 1024 * 1024;

/// Headers of the agent's upgrade request that go upstream.
pub fn forwarded_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            n == "user-agent" || n.starts_with("x-playwright-")
        })
        .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned())))
        .collect()
}

/// Serve one agent connection until either side closes.
pub async fn pump(
    client: WebSocket,
    inst: Arc<JugglerInstance>,
    headers: Vec<(String, String)>,
    agents_enabled: impl Fn() -> bool + Send + 'static,
) {
    let guard: Arc<NavigationGuard> = inst.guard();
    let Ok(mut request) = inst.endpoint().into_client_request() else {
        return;
    };
    for (name, value) in &headers {
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(value),
        ) {
            request.headers_mut().insert(n, v);
        }
    }
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let upstream = match tokio_tungstenite::connect_async_with_config(request, Some(config), false).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            tracing::warn!(user_id = %inst.user_id, instance = inst.instance, error = %e, "camoufox: sidecar endpoint refused the bridge");
            return;
        }
    };
    tracing::info!(user_id = %inst.user_id, instance = inst.instance, "camoufox: agent attached");
    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut ag_tx, mut ag_rx) = client.split();
    let mut filter = PwFilter::new(inst.max_tabs());
    let mut refused: u32 = 0;
    loop {
        tokio::select! {
            from_agent = ag_rx.next() => {
                let Some(Ok(frame)) = from_agent else { break };
                let text = match frame {
                    AxMessage::Text(t) => t.to_string(),
                    AxMessage::Close(_) => break,
                    AxMessage::Ping(_) | AxMessage::Pong(_) => continue,
                    // The Playwright protocol is JSON text only.
                    AxMessage::Binary(_) => break,
                };
                inst.mark_agent();
                let Ok(mut msg) = serde_json::from_str::<Value>(&text) else { break };
                // Agents never set Cookie/Set-Cookie through headers params.
                let text = if strip_cookie_headers(&mut msg) > 0 { msg.to_string() } else { text };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let verdict = if agents_enabled() {
                    filter.on_client_message(&msg, &guard)
                } else {
                    Verdict::Refuse("agents may not use the browser".into())
                };
                match verdict {
                    Verdict::Allow => {
                        if up_tx.send(TgMessage::text(text)).await.is_err() {
                            break;
                        }
                    }
                    Verdict::Refuse(reason) => {
                        refused += 1;
                        let method = msg.get("method").and_then(serde_json::Value::as_str).unwrap_or("");
                        tracing::info!(
                            user_id = %inst.user_id,
                            instance = inst.instance,
                            method,
                            "camoufox: agent call refused"
                        );
                        let reply = refusal(&id, &reason).to_string();
                        if ag_tx.send(AxMessage::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                    Verdict::Detach => {
                        let _ = ag_tx.send(AxMessage::Text(empty_result(&id).to_string().into())).await;
                        break;
                    }
                }
            }
            from_up = up_rx.next() => {
                let Some(Ok(frame)) = from_up else { break };
                match frame {
                    TgMessage::Text(t) => {
                        // Only JSON is forwarded, and never with a Cookie or
                        // Set-Cookie header in it (HttpOnly cookies included).
                        let Ok(mut msg) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                        filter.on_server_message(&msg);
                        let out = if strip_cookie_headers(&mut msg) > 0 {
                            msg.to_string()
                        } else {
                            t.as_str().to_owned()
                        };
                        if ag_tx.send(AxMessage::Text(out.into())).await.is_err() {
                            break;
                        }
                    }
                    TgMessage::Close(_) => break,
                    _ => continue,
                }
            }
        }
    }
    let _ = up_tx.close().await;
    let _ = ag_tx.close().await;
    tracing::info!(user_id = %inst.user_id, instance = inst.instance, refused, "camoufox: agent detached");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_version_headers_go_upstream() {
        let mut h = HeaderMap::new();
        h.insert(
            "user-agent",
            "Playwright/1.62.0-alpha (x64; debian 12) node/22.22".parse().unwrap(),
        );
        h.insert("x-playwright-browser", "firefox".parse().unwrap());
        h.insert("x-aionui-bridge-token", "secret".parse().unwrap());
        h.insert("cookie", "a=b".parse().unwrap());
        let mut fwd = forwarded_headers(&h);
        fwd.sort();
        assert_eq!(fwd.len(), 2);
        assert_eq!(fwd[0].0, "user-agent");
        assert_eq!(fwd[1], ("x-playwright-browser".to_owned(), "firefox".to_owned()));
    }
}
