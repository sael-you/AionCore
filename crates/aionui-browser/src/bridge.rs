//! The agents' CDP bridge (C30, bridge model approved 2026-11-16): per user,
//! one loopback listener per possible instance (1..=5), each a token-gated
//! proxy to that instance's REAL Chrome endpoint. Agents see and drive the
//! real tabs; the bridge refuses what [`crate::filter`] refuses, `createTarget`
//! beyond `max_tabs`, and everything while "Agents can use the browser" is
//! off.
//!
//! Every endpoint, discovery included, requires the header
//! `X-AionUi-Bridge-Token`; no URL ever carries the token. A local process
//! that only knows (or scans) the port gets 401. playwright-cli sends the
//! header from `PLAYWRIGHT_MCP_CDP_HEADERS` on both the discovery request and
//! the WebSocket upgrade (verified: playwright-core 1.66 in @playwright/cli
//! 0.1.24, coreBundle.js `configFromEnv` → `cdpHeaders` merged under
//! `--cdp`; `connectOverCDP` passes them to `urlToWSEndpoint` and
//! `WebSocketTransport.connect`), so `attach --cdp http://127.0.0.1:<port>`
//! puts only the port in argv.
//!
//! Agent env (never argv): `AIONUI_CDP_ACTIVE_PORT` / `AIONUI_CDP_ACTIVE_TOKEN`
//! = instance 1, `AIONUI_CDP_PORTS` = `1:<port>,…,5:<port>`,
//! `PLAYWRIGHT_MCP_CDP_HEADERS` = `X-AionUi-Bridge-Token: <token>`.
//! Neither the bridge ports, the token nor Chrome's own port is ever logged.

use std::sync::Arc;

use aionui_api_types::{BROWSER_MAX_INSTANCES_LIMIT, BrowserErrorCode};
use axum::Router;
use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as TMessage;

use crate::error::BrowserError;
use crate::filter::{FilterDecision, decide, tokens_match};
use crate::instance::ChromeInstance;

pub const ENV_ACTIVE_PORT: &str = "AIONUI_CDP_ACTIVE_PORT";
pub const ENV_ACTIVE_TOKEN: &str = "AIONUI_CDP_ACTIVE_TOKEN";
pub const ENV_PORTS: &str = "AIONUI_CDP_PORTS";
/// Read by playwright-cli / Playwright MCP: one `Name: value` header.
pub const ENV_PLAYWRIGHT_HEADERS: &str = "PLAYWRIGHT_MCP_CDP_HEADERS";
pub const TOKEN_HEADER: &str = "x-aionui-bridge-token";

/// What the bridge needs from the service.
#[async_trait::async_trait]
pub trait BridgeBackend: Send + Sync {
    /// The running instance for an agent (started on demand). Errors carry
    /// `BROWSER_AGENTS_DISABLED` / `BROWSER_INSTANCE_LIMIT` / start failures.
    async fn agent_instance(&self, user_id: &str, instance: u8) -> Result<Arc<ChromeInstance>, BrowserError>;
    /// Checked on every agent message (the switch can flip mid-connection).
    fn agents_enabled(&self, user_id: &str) -> bool;
}

/// The listeners of one user.
#[derive(Clone)]
pub struct AgentBridge {
    token: Arc<String>,
    ports: Vec<(u8, u16)>,
}

impl std::fmt::Debug for AgentBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentBridge").finish_non_exhaustive()
    }
}

impl AgentBridge {
    /// Bind 127.0.0.1:0 for every possible instance and serve them.
    pub async fn start(user_id: &str, backend: Arc<dyn BridgeBackend>) -> Result<Self, BrowserError> {
        let token = Arc::new(new_token());
        let mut ports = Vec::new();
        for instance in 1..=BROWSER_MAX_INSTANCES_LIMIT {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let port = listener.local_addr()?.port();
            let ctx = BridgeCtx {
                user_id: Arc::new(user_id.to_owned()),
                instance,
                port,
                token: token.clone(),
                backend: backend.clone(),
            };
            let app = router(ctx);
            tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::warn!(instance, error = %e, "browser bridge listener stopped");
                }
            });
            ports.push((instance, port));
        }
        tracing::info!(user_id, "browser agent bridge listening (loopback)");
        Ok(Self { token, ports })
    }

    /// The agent env pairs (see the module docs).
    pub fn env(&self) -> Vec<(String, String)> {
        let first = self.ports.first().map(|(_, p)| p.to_string()).unwrap_or_default();
        let all = self
            .ports
            .iter()
            .map(|(n, p)| format!("{n}:{p}"))
            .collect::<Vec<_>>()
            .join(",");
        vec![
            (ENV_ACTIVE_PORT.to_owned(), first),
            (ENV_ACTIVE_TOKEN.to_owned(), self.token.as_str().to_owned()),
            (ENV_PORTS.to_owned(), all),
            (
                ENV_PLAYWRIGHT_HEADERS.to_owned(),
                format!("X-AionUi-Bridge-Token: {}", self.token.as_str()),
            ),
        ]
    }
}

fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

#[derive(Clone)]
struct BridgeCtx {
    user_id: Arc<String>,
    instance: u8,
    port: u16,
    token: Arc<String>,
    backend: Arc<dyn BridgeBackend>,
}

fn router(ctx: BridgeCtx) -> Router {
    Router::new()
        .route("/json/version", get(version))
        .route("/json/version/", get(version))
        .route("/json", get(list))
        .route("/json/", get(list))
        .route("/json/list", get(list))
        .route("/json/list/", get(list))
        .route("/devtools/browser/{id}", get(ws_browser))
        .route("/devtools/page/{id}", get(ws_page))
        .with_state(ctx)
}

fn error_response(status: StatusCode, code: BrowserErrorCode, message: &str) -> Response {
    (status, axum::Json(json!({"code": code, "message": message}))).into_response()
}

fn backend_error(e: BrowserError) -> Response {
    let code = e.code().unwrap_or(BrowserErrorCode::BrowserStartFailed);
    let status = match code {
        BrowserErrorCode::BrowserAgentsDisabled => StatusCode::FORBIDDEN,
        BrowserErrorCode::BrowserInstanceLimit => StatusCode::FORBIDDEN,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    error_response(status, code, "the server browser is not available to agents")
}

impl BridgeCtx {
    fn ws_url(&self, path: &str) -> String {
        format!("ws://127.0.0.1:{}{}", self.port, path)
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        headers
            .get(TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| tokens_match(t.trim(), &self.token))
    }
}

async fn version(State(ctx): State<BridgeCtx>, headers: HeaderMap) -> Response {
    if !ctx.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match ctx.backend.agent_instance(&ctx.user_id, ctx.instance).await {
        Ok(inst) => {
            inst.mark_agent(None);
            axum::Json(json!({
                "Browser": format!("Chrome/{}", inst.version),
                "Protocol-Version": "1.3",
                "webSocketDebuggerUrl": ctx.ws_url("/devtools/browser/aionui"),
            }))
            .into_response()
        }
        Err(e) => backend_error(e),
    }
}

async fn list(State(ctx): State<BridgeCtx>, headers: HeaderMap) -> Response {
    if !ctx.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match ctx.backend.agent_instance(&ctx.user_id, ctx.instance).await {
        Ok(inst) => {
            let tabs = inst.tabs().borrow().clone();
            let rows: Vec<Value> = tabs
                .into_iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "type": "page",
                        "title": t.title,
                        "url": t.url,
                        "webSocketDebuggerUrl": ctx.ws_url(&format!("/devtools/page/{}", t.id)),
                    })
                })
                .collect();
            axum::Json(Value::Array(rows)).into_response()
        }
        Err(e) => backend_error(e),
    }
}

async fn ws_browser(State(ctx): State<BridgeCtx>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    upgrade(ctx, &headers, ws, None).await
}

async fn ws_page(
    State(ctx): State<BridgeCtx>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return error_response(
            StatusCode::BAD_REQUEST,
            BrowserErrorCode::BrowserTabNotFound,
            "bad target id",
        );
    }
    upgrade(ctx, &headers, ws, Some(id)).await
}

async fn upgrade(ctx: BridgeCtx, headers: &HeaderMap, ws: WebSocketUpgrade, page: Option<String>) -> Response {
    if !ctx.authorized(headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let inst = match ctx.backend.agent_instance(&ctx.user_id, ctx.instance).await {
        Ok(i) => i,
        Err(e) => return backend_error(e),
    };
    let upstream_path = match &page {
        Some(id) => format!("/devtools/page/{id}"),
        None => inst.browser_path().to_owned(),
    };
    let upstream = match tokio_tungstenite::connect_async(inst.upstream_url(&upstream_path)).await {
        Ok((s, _)) => s,
        Err(_) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                BrowserErrorCode::BrowserNotRunning,
                "the browser endpoint is not reachable",
            );
        }
    };
    ws.max_message_size(256 * 1024 * 1024)
        .on_upgrade(move |client| pump(ctx, inst, page, client, upstream))
}

/// What to do with one agent frame.
#[derive(Debug, PartialEq)]
pub(crate) enum Verdict {
    Forward,
    /// Reply this error frame to the agent instead of forwarding.
    Refuse(Value),
}

/// Decide one client → Chrome text frame (pure; the tab count and switch
/// state are passed in).
pub(crate) fn judge(text: &str, agents_enabled: bool, tab_count: usize, max_tabs: u8) -> Verdict {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return Verdict::Refuse(json!({"error": {"code": -32700, "message": "BROWSER_BAD_MESSAGE"}}));
    };
    let method = frame.get("method").and_then(Value::as_str).unwrap_or_default();
    let refuse = |code: BrowserErrorCode, detail: &str| {
        let code_text = serde_json::to_value(code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned));
        let mut reply = json!({
            "id": frame.get("id").cloned().unwrap_or(Value::Null),
            "error": {"code": -32000, "message": format!("{}: {detail}", code_text.unwrap_or_default())},
        });
        if let Some(sid) = frame.get("sessionId") {
            reply["sessionId"] = sid.clone();
        }
        Verdict::Refuse(reply)
    };
    if !agents_enabled {
        return refuse(
            BrowserErrorCode::BrowserAgentsDisabled,
            "agents may not use the browser",
        );
    }
    match decide(method, frame.get("params").unwrap_or(&Value::Null)) {
        FilterDecision::Forward => Verdict::Forward,
        FilterDecision::NeedsTabSlot if tab_count < usize::from(max_tabs) => Verdict::Forward,
        FilterDecision::NeedsTabSlot => refuse(BrowserErrorCode::BrowserTabLimit, "the tab limit is reached"),
        FilterDecision::Refuse(code) => refuse(code, &format!("{method} is not available to agents")),
    }
}

/// The tab a frame acts on, when it names one.
fn frame_tab(text: &str, page: Option<&str>) -> Option<String> {
    if let Some(p) = page {
        return Some(p.to_owned());
    }
    let frame: Value = serde_json::from_str(text).ok()?;
    frame["params"]["targetId"].as_str().map(str::to_owned)
}

async fn pump(
    ctx: BridgeCtx,
    inst: Arc<ChromeInstance>,
    page: Option<String>,
    client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let (mut c_tx, mut c_rx) = client.split();
    let (mut u_tx, mut u_rx) = upstream.split();
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let to_chrome = {
        let ctx = ctx.clone();
        let inst = inst.clone();
        async move {
            while let Some(Ok(msg)) = c_rx.next().await {
                let text = match msg {
                    AxMessage::Text(t) => t.to_string(),
                    AxMessage::Close(_) => break,
                    _ => continue,
                };
                let verdict = judge(
                    &text,
                    ctx.backend.agents_enabled(&ctx.user_id),
                    inst.tab_count(),
                    inst.max_tabs(),
                );
                match verdict {
                    Verdict::Forward => {
                        inst.mark_agent(frame_tab(&text, page.as_deref()).as_deref());
                        if u_tx.send(TMessage::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Verdict::Refuse(reply) => {
                        let _ = reply_tx.send(reply.to_string());
                    }
                }
            }
            let _ = u_tx.close().await;
        }
    };

    let to_agent = async move {
        loop {
            tokio::select! {
                up = u_rx.next() => {
                    let Some(Ok(msg)) = up else { break };
                    // Every Chrome -> agent frame loses cookies and cookie
                    // headers (Network.* events expose them, HttpOnly included).
                    let text = match msg {
                        TMessage::Text(t) => t.to_string(),
                        TMessage::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                        TMessage::Close(_) => break,
                        _ => continue,
                    };
                    let clean = crate::filter::sanitize(&text);
                    if clean.is_empty() {
                        continue;
                    }
                    let out = AxMessage::Text(clean.into_owned().into());
                    if c_tx.send(out).await.is_err() {
                        break;
                    }
                }
                reply = reply_rx.recv() => {
                    let Some(reply) = reply else { break };
                    if c_tx.send(AxMessage::Text(reply.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = c_tx.close().await;
    };

    tokio::select! {
        _ = to_chrome => {}
        _ = to_agent => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_applies_switch_filter_and_tab_slots() {
        let nav = r#"{"id":1,"method":"Page.navigate","params":{"url":"https://a.b"},"sessionId":"S"}"#;
        assert_eq!(judge(nav, true, 0, 10), Verdict::Forward);

        let Verdict::Refuse(off) = judge(nav, false, 0, 10) else {
            panic!()
        };
        assert_eq!(off["id"], 1);
        assert_eq!(off["sessionId"], "S");
        assert!(
            off["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("BROWSER_AGENTS_DISABLED")
        );

        let cookies = r#"{"id":2,"method":"Network.getAllCookies"}"#;
        let Verdict::Refuse(blocked) = judge(cookies, true, 0, 10) else {
            panic!()
        };
        assert!(
            blocked["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("BROWSER_METHOD_BLOCKED")
        );

        let create = r#"{"id":3,"method":"Target.createTarget","params":{"url":"about:blank"}}"#;
        assert_eq!(judge(create, true, 9, 10), Verdict::Forward);
        let Verdict::Refuse(full) = judge(create, true, 10, 10) else {
            panic!()
        };
        assert!(
            full["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("BROWSER_TAB_LIMIT")
        );

        assert!(matches!(judge("not json", true, 0, 10), Verdict::Refuse(_)));

        // Train-7 review: tunnels and non-flat sessions are refused by params.
        let tunnel = r#"{"id":4,"method":"Target.sendMessageToTarget","params":{"message":"{}"}}"#;
        assert!(matches!(judge(tunnel, true, 0, 10), Verdict::Refuse(_)));
        let non_flat = r#"{"id":5,"method":"Target.attachToTarget","params":{"targetId":"T"}}"#;
        assert!(matches!(judge(non_flat, true, 0, 10), Verdict::Refuse(_)));
        let flat = r#"{"id":6,"method":"Target.attachToTarget","params":{"targetId":"T","flatten":true}}"#;
        assert_eq!(judge(flat, true, 0, 10), Verdict::Forward);
        let fetch = r#"{"id":7,"method":"Fetch.continueRequest","params":{"requestId":"r"}}"#;
        assert!(matches!(judge(fetch, true, 0, 10), Verdict::Refuse(_)));
    }

    #[test]
    fn env_lists_instance_one_and_every_port() {
        let bridge = AgentBridge {
            token: Arc::new("t0k".into()),
            ports: vec![(1, 4001), (2, 4002), (3, 4003)],
        };
        let env: std::collections::HashMap<_, _> = bridge.env().into_iter().collect();
        assert_eq!(env[ENV_ACTIVE_PORT], "4001");
        assert_eq!(env[ENV_ACTIVE_TOKEN], "t0k");
        assert_eq!(env[ENV_PORTS], "1:4001,2:4002,3:4003");
        assert_eq!(env[ENV_PLAYWRIGHT_HEADERS], "X-AionUi-Bridge-Token: t0k");
        assert!(!format!("{bridge:?}").contains("t0k"), "Debug never prints the token");
    }

    struct DisabledBackend;

    #[async_trait::async_trait]
    impl BridgeBackend for DisabledBackend {
        async fn agent_instance(&self, _: &str, _: u8) -> Result<Arc<ChromeInstance>, BrowserError> {
            Err(BrowserError::coded(BrowserErrorCode::BrowserAgentsDisabled, "off"))
        }
        fn agents_enabled(&self, _: &str) -> bool {
            false
        }
    }

    /// Discovery and upgrades need the header; a port alone gets 401.
    #[tokio::test]
    async fn every_endpoint_requires_the_token_header() {
        let bridge = AgentBridge::start("u1", Arc::new(DisabledBackend)).await.unwrap();
        let env: std::collections::HashMap<_, _> = bridge.env().into_iter().collect();
        let port = &env[ENV_ACTIVE_PORT];
        let token = &env[ENV_ACTIVE_TOKEN];
        let http = reqwest::Client::new();
        for path in ["/json/version", "/json/version/", "/json/list", "/json"] {
            let url = format!("http://127.0.0.1:{port}{path}");
            assert_eq!(
                http.get(&url).send().await.unwrap().status(),
                401,
                "{path} without header"
            );
            let wrong = http
                .get(&url)
                .header("X-AionUi-Bridge-Token", "nope")
                .send()
                .await
                .unwrap();
            assert_eq!(wrong.status(), 401, "{path} with a wrong token");
            let ok = http
                .get(&url)
                .header("X-AionUi-Bridge-Token", token)
                .send()
                .await
                .unwrap();
            assert_eq!(ok.status(), 403, "{path}: authorized, then refused by the switch");
            let body: Value = ok.json().await.unwrap();
            assert_eq!(body["code"], "BROWSER_AGENTS_DISABLED");
        }
        // A query token is not a credential.
        let q = http
            .get(format!("http://127.0.0.1:{port}/json/version?token={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(q.status(), 401);
        // WebSocket upgrade without the header.
        let ws = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/devtools/browser/aionui")).await;
        assert!(ws.is_err(), "upgrade refused without the header");
    }

    #[test]
    fn frame_tab_prefers_the_page_connection() {
        assert_eq!(frame_tab("{}", Some("P1")).as_deref(), Some("P1"));
        assert_eq!(
            frame_tab(r#"{"method":"Target.activateTarget","params":{"targetId":"T2"}}"#, None).as_deref(),
            Some("T2")
        );
        assert_eq!(frame_tab(r#"{"method":"Page.reload"}"#, None), None);
    }
}
