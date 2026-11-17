//! Agents in the owner's own browser (C34, TASK-125): one more bridge
//! instance per paired device ("Mac · Chrome"), on its own loopback port
//! with the SAME header token, filter ([`crate::bridge::judge`]), cookie
//! stripping ([`crate::filter::sanitize`]) and switch as the server Chrome.
//!
//! The extension only reaches single tabs (`chrome.debugger`), so this module
//! synthesizes the browser-level Target layer from the shared-tab list, with
//! the rules of the desktop single-target emulation (cdpTargetProtocol
//! fixture) generalized to several tabs: `getTargets` / discovery / auto-attach
//! list ONLY shared tabs; `attachToTarget` (flat) gives one session per tab;
//! a session-scoped `setAutoAttach` is only acknowledged (no child sessions in
//! v1); `createTarget` / `closeTarget` need the extension's "new tabs" switch;
//! everything else browser-level is refused.
//!
//! On a high-value origin, `Input.*` / `Runtime.evaluate` / `callFunctionOn`
//! wait for the owner ([`crate::approvals`]); commands stay in order because
//! one task reads the agent's frames and only that task waits.
//!
//! Env (never argv): `AIONUI_CDP_DEVICES` = JSON `[{device_id, name, port}]`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aionui_api_types::{BrowserActivityOutcome, BrowserErrorCode, ExtensionTab};
use axum::Router;
use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::approvals::{ApprovalAsk, GateOutcome, is_read_only_method, summarize};
use crate::bridge::{TOKEN_HEADER, Verdict, judge};
use crate::filter::tokens_match;
use crate::guard::GuardDecision;
use crate::relay::{DeviceConn, DeviceHub, RelayEvent, SharedTabs};

pub const ENV_DEVICES: &str = "AIONUI_CDP_DEVICES";
pub const DEVICE_CONTEXT: &str = "aionui-device-context";
const NEW_TAB_WAIT: Duration = Duration::from_secs(10);
/// Rate limits per agent connection (C34 §26).
const NAVIGATIONS_PER_MIN: usize = 30;
const INPUT_EVENTS_PER_SEC: usize = 60;

/// What the device bridge needs from the service.
pub trait DeviceBackend: Send + Sync {
    /// "Agents can use the browser" AND "Agents may use my Mac browser".
    fn devices_allowed(&self, user_id: &str) -> bool;
    fn max_tabs(&self, user_id: &str) -> u8;
}

/// A running listener; dropping it stops accepting connections.
pub struct DeviceListener {
    pub user_id: String,
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for DeviceListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for DeviceListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceListener").finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct Ctx {
    user_id: Arc<String>,
    device_id: Arc<String>,
    port: u16,
    token: Arc<String>,
    hub: Arc<DeviceHub>,
    backend: Arc<dyn DeviceBackend>,
}

pub async fn start_listener(
    user_id: &str,
    device_id: &str,
    token: Arc<String>,
    hub: Arc<DeviceHub>,
    backend: Arc<dyn DeviceBackend>,
) -> Result<DeviceListener, crate::error::BrowserError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let ctx = Ctx {
        user_id: Arc::new(user_id.to_owned()),
        device_id: Arc::new(device_id.to_owned()),
        port,
        token,
        hub,
        backend,
    };
    let app = Router::new()
        .route("/json/version", get(version))
        .route("/json/version/", get(version))
        .route("/json", get(list))
        .route("/json/", get(list))
        .route("/json/list", get(list))
        .route("/json/list/", get(list))
        .route("/devtools/browser/{id}", get(ws_browser))
        .route("/devtools/page/{id}", get(ws_page))
        .with_state(ctx);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(error = %e, "browser device bridge listener stopped");
        }
    });
    Ok(DeviceListener {
        user_id: user_id.to_owned(),
        port,
        task,
    })
}

pub fn target_id(tab_id: i64) -> String {
    format!("tab-{tab_id}")
}

fn session_id(tab_id: i64) -> String {
    format!("aionui-tab-{tab_id}")
}

fn parse_target(id: &str) -> Option<i64> {
    id.strip_prefix("tab-")?.parse().ok()
}

fn target_info(tab: &ExtensionTab) -> Value {
    json!({
        "targetId": target_id(tab.tab_id),
        "type": "page",
        "title": tab.title,
        "url": tab.url,
        "attached": true,
        "canAccessOpener": false,
        "browserContextId": DEVICE_CONTEXT,
    })
}

fn error_response(status: StatusCode, code: BrowserErrorCode, message: &str) -> Response {
    (status, axum::Json(json!({"code": code, "message": message}))).into_response()
}

impl Ctx {
    fn authorized(&self, headers: &HeaderMap) -> bool {
        headers
            .get(TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| tokens_match(t.trim(), &self.token))
    }

    /// The device connection, or the refusal for the agent.
    fn conn(&self) -> Result<Arc<DeviceConn>, Box<Response>> {
        if !self.backend.devices_allowed(&self.user_id) {
            return Err(Box::new(error_response(
                StatusCode::FORBIDDEN,
                BrowserErrorCode::BrowserAgentsDisabled,
                "agents may not use this browser",
            )));
        }
        self.hub.conn(&self.user_id, &self.device_id).ok_or_else(|| {
            Box::new(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                BrowserErrorCode::BrowserUnavailable,
                "the device is offline",
            ))
        })
    }
}

async fn version(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if !ctx.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match ctx.conn() {
        Ok(conn) => axum::Json(json!({
            "Browser": conn.browser,
            "Protocol-Version": "1.3",
            "webSocketDebuggerUrl": format!("ws://127.0.0.1:{}/devtools/browser/aionui", ctx.port),
        }))
        .into_response(),
        Err(r) => *r,
    }
}

async fn list(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if !ctx.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match ctx.conn() {
        Ok(conn) => {
            let rows: Vec<Value> = conn
                .shared_now()
                .tabs
                .iter()
                .map(|t| {
                    json!({
                        "id": target_id(t.tab_id),
                        "type": "page",
                        "title": t.title,
                        "url": t.url,
                        "webSocketDebuggerUrl": format!("ws://127.0.0.1:{}/devtools/page/{}", ctx.port, target_id(t.tab_id)),
                    })
                })
                .collect();
            axum::Json(Value::Array(rows)).into_response()
        }
        Err(r) => *r,
    }
}

async fn ws_browser(State(ctx): State<Ctx>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    upgrade(ctx, &headers, ws, None)
}

async fn ws_page(State(ctx): State<Ctx>, Path(id): Path<String>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(tab) = parse_target(&id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            BrowserErrorCode::BrowserTabNotFound,
            "bad target id",
        );
    };
    upgrade(ctx, &headers, ws, Some(tab))
}

fn upgrade(ctx: Ctx, headers: &HeaderMap, ws: WebSocketUpgrade, page: Option<i64>) -> Response {
    if !ctx.authorized(headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let conn = match ctx.conn() {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if let Some(tab) = page
        && !conn.shared_now().tabs.iter().any(|t| t.tab_id == tab)
    {
        return error_response(
            StatusCode::NOT_FOUND,
            BrowserErrorCode::BrowserTabNotFound,
            "the tab is not shared",
        );
    }
    ws.max_message_size(256 * 1024 * 1024)
        .on_upgrade(move |socket| run_link(ctx, conn, page, socket))
}

// ---------------------------------------------------------------------------
// One agent connection
// ---------------------------------------------------------------------------

#[derive(Default)]
struct LinkState {
    discover: bool,
    auto_attach: bool,
    /// tab → session id (browser endpoint only).
    attached: HashMap<i64, String>,
    /// The shared tabs this link has announced (for diffs).
    known: Vec<ExtensionTab>,
}

type SharedLinkState = Arc<std::sync::Mutex<LinkState>>;

struct Link {
    ctx: Ctx,
    conn: Arc<DeviceConn>,
    page: Option<i64>,
    state: SharedLinkState,
    out: mpsc::UnboundedSender<String>,
    navigations: std::collections::VecDeque<Instant>,
    inputs: std::collections::VecDeque<Instant>,
}

pub(crate) fn refusal(frame: &Value, code: BrowserErrorCode, detail: &str) -> Value {
    let code_text = serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    error_reply(frame, &format!("{code_text}: {detail}"))
}

fn error_reply(frame: &Value, message: &str) -> Value {
    let mut reply = json!({
        "id": frame.get("id").cloned().unwrap_or(Value::Null),
        "error": {"code": -32000, "message": message},
    });
    if let Some(sid) = frame
        .get("sessionId")
        .filter(|s| s.as_str().is_some_and(|s| !s.is_empty()))
    {
        reply["sessionId"] = sid.clone();
    }
    reply
}

fn ok_reply(frame: &Value, result: Value) -> Value {
    let mut reply = json!({"id": frame.get("id").cloned().unwrap_or(Value::Null), "result": result});
    if let Some(sid) = frame
        .get("sessionId")
        .filter(|s| s.as_str().is_some_and(|s| !s.is_empty()))
    {
        reply["sessionId"] = sid.clone();
    }
    reply
}

fn send(out: &mpsc::UnboundedSender<String>, v: &Value) {
    let text = v.to_string();
    let clean = crate::filter::sanitize(&text);
    if !clean.is_empty() {
        let _ = out.send(clean.into_owned());
    }
}

/// Attach `tab` (idempotent) and announce it.
fn attach(state: &mut LinkState, out: &mpsc::UnboundedSender<String>, tab: &ExtensionTab) -> String {
    if let Some(s) = state.attached.get(&tab.tab_id) {
        return s.clone();
    }
    let sid = session_id(tab.tab_id);
    state.attached.insert(tab.tab_id, sid.clone());
    send(
        out,
        &json!({"method": "Target.attachedToTarget", "params": {
            "sessionId": sid, "targetInfo": target_info(tab), "waitingForDebugger": false
        }}),
    );
    sid
}

async fn run_link(ctx: Ctx, conn: Arc<DeviceConn>, page: Option<i64>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
    let state: SharedLinkState = Arc::new(std::sync::Mutex::new(LinkState {
        known: conn.shared_now().tabs,
        ..Default::default()
    }));
    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sink.send(AxMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let events = tokio::spawn(pump_events(conn.clone(), page, state.clone(), out.clone()));
    let mut link = Link {
        ctx,
        conn: conn.clone(),
        page,
        state,
        out,
        navigations: Default::default(),
        inputs: Default::default(),
    };
    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                match msg {
                    AxMessage::Text(t) => link.on_frame(&t).await,
                    AxMessage::Close(_) => break,
                    _ => {}
                }
            }
            () = conn.closed() => break,
        }
        if events.is_finished() {
            break;
        }
    }
    events.abort();
    drop(link);
    let _ = writer.await;
}

/// Device events and tab changes → this agent.
async fn pump_events(
    conn: Arc<DeviceConn>,
    page: Option<i64>,
    state: SharedLinkState,
    out: mpsc::UnboundedSender<String>,
) {
    let mut events = conn.subscribe();
    let mut shared = conn.shared();
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(RelayEvent::Cdp { tab_id, method, params }) => {
                    if page == Some(tab_id) {
                        send(&out, &json!({"method": &*method, "params": &*params}));
                    } else if page.is_none() {
                        let sid = state.lock().expect("link state").attached.get(&tab_id).cloned();
                        if let Some(sid) = sid {
                            send(&out, &json!({"method": &*method, "params": &*params, "sessionId": sid}));
                        }
                    }
                }
                // A link that cannot keep up would see a broken protocol: drop it.
                Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => return,
            },
            changed = shared.changed() => {
                if changed.is_err() { return }
                let now = shared.borrow_and_update().clone();
                if !on_tabs(&state, page, &out, &now) {
                    return;
                }
            }
        }
    }
}

/// Announce tab changes; false when a page link lost its tab.
fn on_tabs(state: &SharedLinkState, page: Option<i64>, out: &mpsc::UnboundedSender<String>, now: &SharedTabs) -> bool {
    let mut st = state.lock().expect("link state");
    if let Some(p) = page {
        st.known = now.tabs.clone();
        return now.tabs.iter().any(|t| t.tab_id == p);
    }
    let before = std::mem::take(&mut st.known);
    for tab in &now.tabs {
        match before.iter().find(|b| b.tab_id == tab.tab_id) {
            None => {
                if st.discover {
                    send(
                        out,
                        &json!({"method": "Target.targetCreated", "params": {"targetInfo": target_info(tab)}}),
                    );
                }
                if st.auto_attach {
                    attach(&mut st, out, tab);
                }
            }
            Some(old) if old != tab && (st.discover || st.attached.contains_key(&tab.tab_id)) => {
                send(
                    out,
                    &json!({"method": "Target.targetInfoChanged", "params": {"targetInfo": target_info(tab)}}),
                );
            }
            Some(_) => {}
        }
    }
    for gone in before.iter().filter(|b| !now.tabs.iter().any(|t| t.tab_id == b.tab_id)) {
        if let Some(sid) = st.attached.remove(&gone.tab_id) {
            send(
                out,
                &json!({"method": "Target.detachedFromTarget", "params": {
                    "sessionId": sid, "targetId": target_id(gone.tab_id)
                }}),
            );
        }
        if st.discover {
            send(
                out,
                &json!({"method": "Target.targetDestroyed", "params": {"targetId": target_id(gone.tab_id)}}),
            );
        }
    }
    st.known = now.tabs.clone();
    true
}

fn within(window: &mut std::collections::VecDeque<Instant>, per: Duration, max: usize) -> bool {
    let now = Instant::now();
    while window.front().is_some_and(|t| now.duration_since(*t) >= per) {
        window.pop_front();
    }
    if window.len() >= max {
        return false;
    }
    window.push_back(now);
    true
}

impl Link {
    fn reply(&self, v: &Value) {
        send(&self.out, v);
    }

    fn tab(&self, tab_id: i64) -> Option<ExtensionTab> {
        self.conn.shared_now().tabs.into_iter().find(|t| t.tab_id == tab_id)
    }

    async fn on_frame(&mut self, text: &str) {
        let shared = self.conn.shared_now();
        let allowed = self.ctx.backend.devices_allowed(&self.ctx.user_id);
        let max_tabs = self.ctx.backend.max_tabs(&self.ctx.user_id);
        if let Verdict::Refuse(r) = judge(text, allowed, shared.tabs.len(), max_tabs) {
            if let Ok(frame) = serde_json::from_str::<Value>(text) {
                let method = frame["method"].as_str().unwrap_or_default();
                let tab = self.frame_tab(&frame).unwrap_or(0);
                self.ctx
                    .hub
                    .record(&self.ctx.device_id, tab, "", method, BrowserActivityOutcome::Refused);
            }
            return self.reply(&r);
        }
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let method = frame["method"].as_str().unwrap_or_default().to_owned();
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        let session = frame["sessionId"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let tab_id = match (&session, self.page) {
            (Some(sid), _) => {
                let found = self
                    .state
                    .lock()
                    .expect("link state")
                    .attached
                    .iter()
                    .find(|(_, s)| *s == sid)
                    .map(|(t, _)| *t);
                match found {
                    Some(t) => Some(t),
                    None => return self.reply(&error_reply(&frame, &format!("No session with given id: {sid}"))),
                }
            }
            (None, Some(p)) => Some(p),
            (None, None) => None,
        };
        match tab_id {
            None => self.browser_level(&frame, &method, &params, &shared).await,
            Some(t) => self.tab_level(&frame, t, &method, params).await,
        }
    }

    fn frame_tab(&self, frame: &Value) -> Option<i64> {
        if let Some(p) = self.page {
            return Some(p);
        }
        let sid = frame["sessionId"].as_str()?;
        self.state
            .lock()
            .expect("link state")
            .attached
            .iter()
            .find(|(_, s)| s.as_str() == sid)
            .map(|(t, _)| *t)
    }

    async fn browser_level(&mut self, frame: &Value, method: &str, params: &Value, shared: &SharedTabs) {
        let reply = match method {
            "Browser.getVersion" => ok_reply(
                frame,
                json!({"protocolVersion": "1.3", "product": self.conn.browser, "revision": "", "userAgent": "", "jsVersion": ""}),
            ),
            "Target.setDiscoverTargets" => {
                let on = params["discover"].as_bool().unwrap_or(false);
                let mut st = self.state.lock().expect("link state");
                let was = st.discover;
                st.discover = on;
                if on && !was {
                    for tab in &shared.tabs {
                        send(
                            &self.out,
                            &json!({"method": "Target.targetCreated", "params": {"targetInfo": target_info(tab)}}),
                        );
                    }
                }
                ok_reply(frame, json!({}))
            }
            "Target.setAutoAttach" => {
                let on = params["autoAttach"].as_bool().unwrap_or(false);
                let mut st = self.state.lock().expect("link state");
                st.auto_attach = on;
                if on {
                    for tab in &shared.tabs {
                        attach(&mut st, &self.out, tab);
                    }
                }
                ok_reply(frame, json!({}))
            }
            "Target.getTargets" => ok_reply(
                frame,
                json!({"targetInfos": shared.tabs.iter().map(target_info).collect::<Vec<_>>()}),
            ),
            "Target.getTargetInfo" => match params["targetId"].as_str() {
                None => ok_reply(
                    frame,
                    json!({"targetInfo": {"targetId": "browser", "type": "browser", "title": "", "url": "", "attached": true}}),
                ),
                Some(id) => match parse_target(id).and_then(|t| self.tab(t)) {
                    Some(tab) => ok_reply(frame, json!({"targetInfo": target_info(&tab)})),
                    None => error_reply(frame, &format!("No such target id: {id}")),
                },
            },
            "Target.getBrowserContexts" => ok_reply(frame, json!({"browserContextIds": [DEVICE_CONTEXT]})),
            "Target.attachToTarget" => {
                let id = params["targetId"].as_str().unwrap_or_default();
                match parse_target(id).and_then(|t| self.tab(t)) {
                    Some(tab) => {
                        let sid = attach(&mut self.state.lock().expect("link state"), &self.out, &tab);
                        ok_reply(frame, json!({"sessionId": sid}))
                    }
                    None => error_reply(frame, &format!("No such target id: {id}")),
                }
            }
            "Target.detachFromTarget" => {
                let mut st = self.state.lock().expect("link state");
                let sid = params["sessionId"].as_str().map(str::to_owned);
                let tab = match (&sid, params["targetId"].as_str().and_then(parse_target)) {
                    (Some(s), _) => st.attached.iter().find(|(_, v)| *v == s).map(|(t, _)| *t),
                    (None, t) => t,
                };
                if let Some(t) = tab
                    && let Some(s) = st.attached.remove(&t)
                {
                    send(
                        &self.out,
                        &json!({"method": "Target.detachedFromTarget", "params": {
                            "sessionId": s, "targetId": target_id(t)
                        }}),
                    );
                }
                ok_reply(frame, json!({}))
            }
            "Target.activateTarget" => ok_reply(frame, json!({})),
            "Target.createTarget" => self.create_target(frame, params, shared).await,
            "Target.closeTarget" => self.close_target(frame, params, shared).await,
            _ => refusal(
                frame,
                BrowserErrorCode::BrowserMethodBlocked,
                &format!("{method} is not available on a shared browser"),
            ),
        };
        self.reply(&reply);
    }

    async fn create_target(&mut self, frame: &Value, params: &Value, shared: &SharedTabs) -> Value {
        if !shared.new_tabs {
            return refusal(
                frame,
                BrowserErrorCode::BrowserMethodBlocked,
                "the owner did not allow new tabs",
            );
        }
        let url = params["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .unwrap_or("about:blank");
        if self.ctx.hub.guard().decide(url) == GuardDecision::Block {
            return refusal(frame, BrowserErrorCode::BrowserUrlBlocked, "the URL is blocked");
        }
        let mut watch = self.conn.shared();
        let tab_id = match self.conn.create_tab(url).await {
            Ok(t) => t,
            Err(e) if e.code.as_deref() == Some("new_tabs_disabled") => {
                return refusal(
                    frame,
                    BrowserErrorCode::BrowserMethodBlocked,
                    "the owner did not allow new tabs",
                );
            }
            Err(e) => return error_reply(frame, &e.message),
        };
        self.ctx.hub.record(
            &self.ctx.device_id,
            tab_id,
            url,
            "Target.createTarget",
            BrowserActivityOutcome::Ok,
        );
        // Visible once the extension shared it and its Fetch guard is on.
        let visible = tokio::time::timeout(
            NEW_TAB_WAIT,
            watch.wait_for(|s| s.tabs.iter().any(|t| t.tab_id == tab_id)),
        )
        .await;
        match visible {
            Ok(Ok(_)) => ok_reply(frame, json!({"targetId": target_id(tab_id)})),
            _ => error_reply(frame, "the new tab did not become available"),
        }
    }

    async fn close_target(&mut self, frame: &Value, params: &Value, shared: &SharedTabs) -> Value {
        if !shared.new_tabs {
            return refusal(
                frame,
                BrowserErrorCode::BrowserMethodBlocked,
                "the owner did not allow agents to manage tabs",
            );
        }
        let id = params["targetId"].as_str().unwrap_or_default();
        let Some(tab) = parse_target(id).and_then(|t| self.tab(t)) else {
            return error_reply(frame, &format!("No such target id: {id}"));
        };
        match self.conn.close_tab(tab.tab_id).await {
            Ok(()) => {
                self.ctx.hub.record(
                    &self.ctx.device_id,
                    tab.tab_id,
                    &tab.url,
                    "Target.closeTarget",
                    BrowserActivityOutcome::Ok,
                );
                ok_reply(frame, json!({"success": true}))
            }
            Err(e) => error_reply(frame, &e.message),
        }
    }

    async fn tab_level(&mut self, frame: &Value, tab_id: i64, method: &str, params: Value) {
        let Some(tab) = self.tab(tab_id) else {
            return self.reply(&refusal(
                frame,
                BrowserErrorCode::BrowserTabNotFound,
                "the tab is no longer shared",
            ));
        };
        match method {
            // No child sessions in v1: acknowledge (fixture: "session-scoped
            // setAutoAttach only acknowledges").
            "Target.setAutoAttach" | "Target.setDiscoverTargets" => return self.reply(&ok_reply(frame, json!({}))),
            "Target.getTargetInfo" => return self.reply(&ok_reply(frame, json!({"targetInfo": target_info(&tab)}))),
            "Browser.getVersion" => {
                return self.reply(&ok_reply(
                    frame,
                    json!({"protocolVersion": "1.3", "product": self.conn.browser, "revision": "", "userAgent": "", "jsVersion": ""}),
                ));
            }
            m if m.starts_with("Target.") || m.starts_with("Browser.") => {
                return self.reply(&refusal(
                    frame,
                    BrowserErrorCode::BrowserMethodBlocked,
                    &format!("{m} is not available on a shared tab"),
                ));
            }
            _ => {}
        }
        let hub = self.ctx.hub.clone();
        let device = self.ctx.device_id.clone();
        if method == "Page.navigate" && !within(&mut self.navigations, Duration::from_secs(60), NAVIGATIONS_PER_MIN) {
            hub.record(&device, tab_id, &tab.url, method, BrowserActivityOutcome::Refused);
            return self.reply(&refusal(
                frame,
                BrowserErrorCode::BrowserBusy,
                "too many navigations, slow down",
            ));
        }
        if method.starts_with("Input.") {
            while !within(&mut self.inputs, Duration::from_secs(1), INPUT_EVENTS_PER_SEC) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        // Default deny on a high-value tab (its URL or ANY frame's): only the
        // read-only allowlist passes without the owner's OK.
        let high_value = self.conn.high_value_url(tab_id, &tab.url);
        if let Some(hv_url) = high_value.filter(|_| !is_read_only_method(method)) {
            let ask = ApprovalAsk {
                user_id: self.ctx.user_id.to_string(),
                device_id: device.to_string(),
                device_name: self.conn.instance_name(),
                tab_id,
                url: hv_url,
                method: method.to_owned(),
                summary: summarize(method, &params),
            };
            let paused_hub = hub.clone();
            let (pd, pu, pm) = (device.clone(), tab.url.clone(), method.to_owned());
            let outcome = hub
                .approvals()
                .gate(ask, move || {
                    paused_hub.record(&pd, tab_id, &pu, &pm, BrowserActivityOutcome::Paused);
                })
                .await;
            match outcome {
                Ok(GateOutcome::Approved) => {
                    hub.record(&device, tab_id, &tab.url, method, BrowserActivityOutcome::Approved)
                }
                Ok(_) => {}
                Err(e) => {
                    let code = e.code().unwrap_or(BrowserErrorCode::BrowserApprovalDenied);
                    let kind = if code == BrowserErrorCode::BrowserApprovalTimeout {
                        BrowserActivityOutcome::Timeout
                    } else {
                        BrowserActivityOutcome::Denied
                    };
                    hub.record(&device, tab_id, &tab.url, method, kind);
                    return self.reply(&refusal(frame, code, &e.to_string()));
                }
            }
        }
        hub.record(&device, tab_id, &tab.url, method, BrowserActivityOutcome::Ok);
        // On the wire now (order kept); the answer comes back on its own.
        let conn = self.conn.clone();
        let out = self.out.clone();
        let frame = frame.clone();
        let method = method.to_owned();
        tokio::spawn(async move {
            let reply = match conn.dispatch(tab_id, &method, params).await {
                Ok(result) => ok_reply(&frame, result),
                Err(e) => error_reply(&frame, &e.message),
            };
            send(&out, &reply);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_refusals_keep_id_and_session() {
        assert_eq!(parse_target(&target_id(42)), Some(42));
        assert_eq!(parse_target("aionui-browser-target"), None);
        let frame = json!({"id": 3, "method": "Target.createTarget", "sessionId": "S"});
        let r = refusal(&frame, BrowserErrorCode::BrowserMethodBlocked, "no");
        assert_eq!(r["id"], 3);
        assert_eq!(r["sessionId"], "S");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("BROWSER_METHOD_BLOCKED")
        );
        let no_session = ok_reply(&json!({"id": 1, "sessionId": ""}), json!({}));
        assert!(no_session.get("sessionId").is_none());
    }

    #[test]
    fn rate_window_refuses_past_the_limit() {
        let mut w = Default::default();
        for _ in 0..3 {
            assert!(within(&mut w, Duration::from_secs(60), 3));
        }
        assert!(!within(&mut w, Duration::from_secs(60), 3));
    }
}
