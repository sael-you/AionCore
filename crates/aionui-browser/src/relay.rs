//! The extension relay (C34, TASK-125): `WS /api/browser/extension` from the
//! AionUi browser extension in the owner's own Chrome.
//!
//! Wire (agreed with the extension builder): the first frame is `pair {code,
//! device_name, browser}` (answered by `welcome {device_id, token}`, the only
//! time the token is sent) or `hello {token, browser, version}` (answered by
//! `welcome {device_id}`); a bad first frame gets `error {code}` and a close.
//! Then `state` (the FULL list of shared tabs on every change), `result`,
//! `event`, `detached`, `ping` from the extension; `command`, `create_tab`,
//! `close_tab`, `pong` from AionCore.
//!
//! Fetch guard: before a shared tab becomes visible to agents, AionCore sends
//! `Fetch.enable` on it (the navigation guard's patterns plus top-level
//! navigations to high-value hosts) and answers every `Fetch.requestPaused`
//! itself (continue / `BlockedByClient`). Agents never see `Fetch.*`. Tabs
//! whose URL the guard refuses (the AionUi origin, private addresses, local
//! schemes) are hidden from agents.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aionui_api_types::{
    BrowserActivityOutcome, BrowserDevice, BrowserDeviceActivity, BrowserErrorCode, BrowserPairingCode, ExtensionError,
    ExtensionInbound, ExtensionOutbound, ExtensionTab,
};
use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::approvals::{ApprovalBroker, EVENT_DEVICES_CHANGED, high_value_fetch_patterns, is_high_value_url};
use crate::devices::{Authenticated, DeviceStore, now_ms};
use crate::error::BrowserError;
use crate::guard::{GuardDecision, NavigationGuard};

pub const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
pub const ACTIVITY_LIMIT: usize = 500;
/// Largest frame from the extension (screenshots come as results).
pub const MAX_EXTENSION_FRAME: usize = 64 * 1024 * 1024;

/// A CDP event or tab change, fanned out to the agent links of a device.
#[derive(Debug, Clone)]
pub enum RelayEvent {
    Cdp {
        tab_id: i64,
        method: Arc<str>,
        params: Arc<Value>,
    },
}

/// What agents may see: guarded tabs on allowed URLs, and the new-tabs switch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedTabs {
    pub new_tabs: bool,
    pub tabs: Vec<ExtensionTab>,
}

#[derive(Default)]
struct TabTrack {
    /// What the extension reported last (its order is kept).
    reported: Vec<ExtensionTab>,
    /// Tabs whose Fetch guard is on → main frame id when known.
    guarded: HashMap<i64, Option<String>>,
    /// Guard setup in flight.
    guarding: HashSet<i64>,
    new_tabs: bool,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, ExtensionError>>>;

/// One connected extension.
pub struct DeviceConn {
    pub user_id: String,
    pub device_id: String,
    pub name: String,
    pub browser: String,
    out: mpsc::UnboundedSender<ExtensionOutbound>,
    next_id: AtomicU64,
    pending: std::sync::Mutex<Pending>,
    track: std::sync::Mutex<TabTrack>,
    /// tab → frame id → URL (from `Page.getFrameTree`, frame events and
    /// paused document requests): a high-value IFRAME makes the tab
    /// high-value too (train-9 review).
    frames: std::sync::Mutex<HashMap<i64, HashMap<String, String>>>,
    shared: watch::Sender<SharedTabs>,
    events: broadcast::Sender<RelayEvent>,
    alive: watch::Sender<bool>,
}

impl std::fmt::Debug for DeviceConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceConn")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

fn closed_error() -> ExtensionError {
    ExtensionError {
        message: "the device went offline".into(),
        code: Some("offline".into()),
    }
}

impl DeviceConn {
    fn new(auth: &Authenticated, out: mpsc::UnboundedSender<ExtensionOutbound>) -> Self {
        Self {
            user_id: auth.user_id.clone(),
            device_id: auth.device.device_id.clone(),
            name: auth.device.name.clone(),
            browser: auth.device.browser.clone(),
            out,
            next_id: AtomicU64::new(1),
            pending: std::sync::Mutex::new(HashMap::new()),
            track: std::sync::Mutex::new(TabTrack::default()),
            frames: std::sync::Mutex::new(HashMap::new()),
            shared: watch::channel(SharedTabs::default()).0,
            events: broadcast::channel(1024).0,
            alive: watch::channel(true).0,
        }
    }

    pub fn is_alive(&self) -> bool {
        *self.alive.borrow()
    }

    /// Resolves when the extension disconnects (or was replaced / revoked).
    pub async fn closed(&self) {
        let mut rx = self.alive.subscribe();
        let _ = rx.wait_for(|alive| !*alive).await;
    }

    pub fn shared(&self) -> watch::Receiver<SharedTabs> {
        self.shared.subscribe()
    }

    pub fn shared_now(&self) -> SharedTabs {
        self.shared.borrow().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RelayEvent> {
        self.events.subscribe()
    }

    /// "Mac · Chrome": the name agents see for this instance.
    pub fn instance_name(&self) -> String {
        instance_name(&self.name, &self.browser)
    }

    fn register(&self) -> (u64, oneshot::Receiver<Result<Value, ExtensionError>>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        (id, rx)
    }

    async fn wait(
        &self,
        id: u64,
        rx: oneshot::Receiver<Result<Value, ExtensionError>>,
    ) -> Result<Value, ExtensionError> {
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(closed_error()),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(ExtensionError {
                    message: "the device did not answer in time".into(),
                    code: Some("timeout".into()),
                })
            }
        }
    }

    fn send(&self, frame: ExtensionOutbound) -> Result<(), ExtensionError> {
        if !self.is_alive() {
            return Err(closed_error());
        }
        self.out.send(frame).map_err(|_| closed_error())
    }

    /// Send a CDP command to a tab and get a receiver for its result (the
    /// command is on the wire when this returns: callers keep their order).
    pub fn dispatch(
        &self,
        tab_id: i64,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value, ExtensionError>> + Send + '_ {
        let (id, rx) = self.register();
        let sent = self.send(ExtensionOutbound::Command {
            id,
            tab_id,
            method: method.to_owned(),
            params,
        });
        async move {
            if let Err(e) = sent {
                self.pending.lock().expect("pending lock").remove(&id);
                return Err(e);
            }
            self.wait(id, rx).await
        }
    }

    pub async fn call(&self, tab_id: i64, method: &str, params: Value) -> Result<Value, ExtensionError> {
        self.dispatch(tab_id, method, params).await
    }

    /// Fire and forget (the result is dropped when it arrives).
    fn notify(&self, tab_id: i64, method: &str, params: Value) {
        let (id, _rx) = self.register();
        if self
            .send(ExtensionOutbound::Command {
                id,
                tab_id,
                method: method.to_owned(),
                params,
            })
            .is_err()
        {
            self.pending.lock().expect("pending lock").remove(&id);
        }
    }

    /// Ask the extension to open (and auto-share) a tab; its tab id.
    pub async fn create_tab(&self, url: &str) -> Result<i64, ExtensionError> {
        let (id, rx) = self.register();
        self.send(ExtensionOutbound::CreateTab {
            id,
            url: url.to_owned(),
        })?;
        let v = self.wait(id, rx).await?;
        v.get("tab_id").and_then(Value::as_i64).ok_or_else(|| ExtensionError {
            message: "the device did not return a tab id".into(),
            code: None,
        })
    }

    pub async fn close_tab(&self, tab_id: i64) -> Result<(), ExtensionError> {
        let (id, rx) = self.register();
        self.send(ExtensionOutbound::CloseTab { id, tab_id })?;
        self.wait(id, rx).await.map(|_| ())
    }

    fn resolve(&self, id: u64, result: Option<Value>, error: Option<ExtensionError>) {
        let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) else {
            return;
        };
        let _ = tx.send(match error {
            Some(e) => Err(e),
            None => Ok(result.unwrap_or_else(|| json!({}))),
        });
    }

    fn shutdown(&self) {
        self.alive.send_replace(false);
        let pending: Vec<_> = self.pending.lock().expect("pending lock").drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Err(closed_error()));
        }
    }

    /// Recompute what agents see.
    fn publish(&self, guard: &NavigationGuard) {
        let track = self.track.lock().expect("track lock");
        let tabs = track
            .reported
            .iter()
            .filter(|t| track.guarded.contains_key(&t.tab_id))
            .filter(|t| t.url.is_empty() || guard.decide(&t.url) == GuardDecision::Allow)
            .cloned()
            .collect();
        let next = SharedTabs {
            new_tabs: track.new_tabs,
            tabs,
        };
        drop(track);
        self.shared.send_if_modified(|cur| {
            if *cur == next {
                false
            } else {
                *cur = next;
                true
            }
        });
    }

    fn set_url(&self, tab_id: i64, url: &str, guard: &NavigationGuard) {
        {
            let mut track = self.track.lock().expect("track lock");
            let Some(tab) = track.reported.iter_mut().find(|t| t.tab_id == tab_id) else {
                return;
            };
            if tab.url == url {
                return;
            }
            tab.url = url.to_owned();
        }
        self.publish(guard);
    }

    fn set_frame(&self, tab_id: i64, frame_id: &str, url: &str) {
        self.frames
            .lock()
            .expect("frames lock")
            .entry(tab_id)
            .or_default()
            .insert(frame_id.to_owned(), url.to_owned());
    }

    fn remove_frame(&self, tab_id: i64, frame_id: &str) {
        if let Some(f) = self.frames.lock().expect("frames lock").get_mut(&tab_id) {
            f.remove(frame_id);
        }
    }

    fn replace_frames(&self, tab_id: i64, frames: HashMap<String, String>) {
        self.frames.lock().expect("frames lock").insert(tab_id, frames);
    }

    /// The URL that makes this tab high-value: the tab's own, else any
    /// frame's. `None` = an ordinary tab.
    pub fn high_value_url(&self, tab_id: i64, tab_url: &str) -> Option<String> {
        if is_high_value_url(tab_url) {
            return Some(tab_url.to_owned());
        }
        self.frames
            .lock()
            .expect("frames lock")
            .get(&tab_id)
            .and_then(|f| f.values().find(|u| is_high_value_url(u)).cloned())
    }

    fn main_frame(&self, tab_id: i64) -> Option<String> {
        self.track
            .lock()
            .expect("track lock")
            .guarded
            .get(&tab_id)
            .cloned()
            .flatten()
    }
}

pub fn instance_name(name: &str, browser: &str) -> String {
    let family = browser.split_whitespace().next().unwrap_or("Chrome");
    format!("{name} · {family}")
}

/// All devices and their live connections.
pub struct DeviceHub {
    store: DeviceStore,
    approvals: Arc<ApprovalBroker>,
    guard: Arc<NavigationGuard>,
    conns: DashMap<String, Arc<DeviceConn>>,
    activity: DashMap<String, std::sync::Mutex<VecDeque<BrowserDeviceActivity>>>,
}

impl std::fmt::Debug for DeviceHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceHub").finish_non_exhaustive()
    }
}

impl DeviceHub {
    pub fn new(store: DeviceStore, approvals: Arc<ApprovalBroker>, guard: Arc<NavigationGuard>) -> Self {
        Self {
            store,
            approvals,
            guard,
            conns: DashMap::new(),
            activity: DashMap::new(),
        }
    }

    pub fn approvals(&self) -> &Arc<ApprovalBroker> {
        &self.approvals
    }

    pub fn guard(&self) -> &Arc<NavigationGuard> {
        &self.guard
    }

    pub fn store(&self) -> &DeviceStore {
        &self.store
    }

    pub fn pairing_code(&self, user_id: &str) -> Result<BrowserPairingCode, BrowserError> {
        self.store.pairing_code(user_id)
    }

    /// The live connection of a device of `user_id`.
    pub fn conn(&self, user_id: &str, device_id: &str) -> Option<Arc<DeviceConn>> {
        self.conns
            .get(device_id)
            .map(|c| c.clone())
            .filter(|c| c.user_id == user_id && c.is_alive())
    }

    pub fn devices(&self, user_id: &str) -> Vec<BrowserDevice> {
        self.store
            .devices(user_id)
            .into_iter()
            .map(|d| {
                let conn = self.conn(user_id, &d.device_id);
                let shared = conn.as_ref().map(|c| c.shared_now()).unwrap_or_default();
                BrowserDevice {
                    online: conn.is_some(),
                    new_tabs: shared.new_tabs,
                    shared_tabs: u16::try_from(shared.tabs.len()).unwrap_or(u16::MAX),
                    device_id: d.device_id,
                    name: d.name,
                    browser: d.browser,
                    created_at: d.created_at,
                    last_seen_at: d.last_seen_at,
                    extension_version: d.extension_version,
                }
            })
            .collect()
    }

    fn devices_changed(&self, user_id: &str) {
        self.approvals.emit(user_id, EVENT_DEVICES_CHANGED, json!({}));
    }

    /// Revoke: forget the device, close its socket, drop its approvals.
    pub fn revoke(&self, user_id: &str, device_id: &str) -> Result<(), BrowserError> {
        self.store.revoke(user_id, device_id)?;
        if let Some((_, conn)) = self.conns.remove_if(device_id, |_, c| c.user_id == user_id) {
            let _ = conn.out.send(ExtensionOutbound::Error {
                code: BrowserErrorCode::BrowserDeviceRevoked,
            });
            conn.shutdown();
        }
        self.approvals.forget_device(user_id, device_id);
        self.activity.remove(device_id);
        self.devices_changed(user_id);
        Ok(())
    }

    pub fn record(&self, device_id: &str, tab_id: i64, url: &str, method: &str, outcome: BrowserActivityOutcome) {
        let entry = BrowserDeviceActivity {
            at: now_ms(),
            conversation_id: None,
            tab_id,
            origin: crate::approvals::origin_of(url).unwrap_or_default(),
            method: method.to_owned(),
            outcome,
        };
        let ring = self
            .activity
            .entry(device_id.to_owned())
            .or_insert_with(|| std::sync::Mutex::new(VecDeque::new()));
        let mut ring = ring.lock().expect("activity lock");
        if ring.len() >= ACTIVITY_LIMIT {
            ring.pop_front();
        }
        ring.push_back(entry);
    }

    /// Newest first, at most `limit` (default 50, max 500).
    pub fn activity(
        &self,
        user_id: &str,
        device_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<BrowserDeviceActivity>, BrowserError> {
        if self.store.device(user_id, device_id).is_none() {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserDeviceNotFound,
                "no such device",
            ));
        }
        let limit = usize::try_from(limit.unwrap_or(50))
            .unwrap_or(ACTIVITY_LIMIT)
            .clamp(1, ACTIVITY_LIMIT);
        Ok(self
            .activity
            .get(device_id)
            .map(|r| {
                r.lock()
                    .expect("activity lock")
                    .iter()
                    .rev()
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    // -- the extension socket ---------------------------------------------

    /// Serve one extension connection until it closes.
    pub async fn run_extension(self: Arc<Self>, socket: WebSocket) {
        let (mut sink, mut stream) = socket.split();
        let auth = match tokio::time::timeout(FIRST_FRAME_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => self.authenticate_first(&t),
            _ => Err((BrowserErrorCode::BrowserBadMessage, None)),
        };
        let (auth, token) = match auth {
            Ok(a) => a,
            Err((code, _)) => {
                let _ = sink
                    .send(Message::Text(
                        serde_json::to_string(&ExtensionOutbound::Error { code })
                            .unwrap_or_default()
                            .into(),
                    ))
                    .await;
                let _ = sink.close().await;
                return;
            }
        };
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ExtensionOutbound>();
        let conn = Arc::new(DeviceConn::new(&auth, out_tx));
        let _ = conn.out.send(ExtensionOutbound::Welcome {
            device_id: conn.device_id.clone(),
            token,
        });
        if let Some(old) = self.conns.insert(conn.device_id.clone(), conn.clone()) {
            old.shutdown();
        }
        self.store.touch(&conn.user_id, &conn.device_id);
        tracing::info!(user_id = %conn.user_id, device_id = %conn.device_id, "browser device connected");
        self.devices_changed(&conn.user_id);

        let writer_conn = conn.clone();
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = out_rx.recv() => {
                        let Some(frame) = frame else { break };
                        let is_error = matches!(frame, ExtensionOutbound::Error { .. });
                        let text = serde_json::to_string(&frame).unwrap_or_default();
                        if sink.send(Message::Text(text.into())).await.is_err() || is_error {
                            break;
                        }
                    }
                    () = writer_conn.closed() => {
                        // Flush what is queued (e.g. the revoke error), then close.
                        while let Ok(frame) = out_rx.try_recv() {
                            let text = serde_json::to_string(&frame).unwrap_or_default();
                            if sink.send(Message::Text(text.into())).await.is_err() {
                                break;
                            }
                        }
                        break;
                    }
                }
            }
            let _ = sink.close().await;
        });

        loop {
            tokio::select! {
                msg = stream.next() => {
                    let Some(Ok(msg)) = msg else { break };
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    match serde_json::from_str::<ExtensionInbound>(&text) {
                        Ok(frame) => self.on_frame(&conn, frame),
                        Err(_) => tracing::warn!(device_id = %conn.device_id, "browser device sent an unreadable frame"),
                    }
                }
                () = conn.closed() => break,
            }
        }
        conn.shutdown();
        let _ = writer.await;
        let current = self
            .conns
            .remove_if(&conn.device_id, |_, c| Arc::ptr_eq(c, &conn))
            .is_some();
        if current {
            self.approvals.forget_device(&conn.user_id, &conn.device_id);
            self.store.touch(&conn.user_id, &conn.device_id);
            self.devices_changed(&conn.user_id);
        }
        tracing::info!(user_id = %conn.user_id, device_id = %conn.device_id, "browser device disconnected");
    }

    #[allow(clippy::type_complexity)]
    fn authenticate_first(
        &self,
        text: &str,
    ) -> Result<(Authenticated, Option<String>), (BrowserErrorCode, Option<String>)> {
        let (result, version) = match serde_json::from_str::<ExtensionInbound>(text) {
            Ok(ExtensionInbound::Pair {
                code,
                device_name,
                browser,
                extension_version,
            }) => (
                self.store
                    .pair(&code, &device_name, &browser)
                    .map(|(auth, token)| (auth, Some(token)))
                    .map_err(|e| (e.code().unwrap_or(BrowserErrorCode::BrowserPairingInvalid), None)),
                extension_version,
            ),
            Ok(ExtensionInbound::Hello {
                token,
                extension_version,
                ..
            }) => (
                self.store
                    .authenticate(&token)
                    .map(|auth| (auth, None))
                    .map_err(|e| (e.code().unwrap_or(BrowserErrorCode::BrowserDeviceRevoked), None)),
                extension_version,
            ),
            _ => return Err((BrowserErrorCode::BrowserBadMessage, None)),
        };
        // Only for an authenticated device, and only for display.
        if let (Ok((auth, _)), Some(version)) = (&result, version) {
            self.store
                .set_extension_version(&auth.user_id, &auth.device.device_id, &version);
        }
        result
    }

    fn on_frame(self: &Arc<Self>, conn: &Arc<DeviceConn>, frame: ExtensionInbound) {
        match frame {
            ExtensionInbound::Ping => {
                let _ = conn.send(ExtensionOutbound::Pong);
            }
            ExtensionInbound::Result { id, result, error } => conn.resolve(id, result, error),
            ExtensionInbound::State {
                new_tabs,
                tabs,
                extension_version,
            } => {
                if let Some(version) = extension_version {
                    self.store
                        .set_extension_version(&conn.user_id, &conn.device_id, &version);
                }
                self.on_state(conn, new_tabs, tabs)
            }
            ExtensionInbound::Event { tab_id, method, params } => self.on_event(conn, tab_id, method, params),
            ExtensionInbound::Detached { tab_id, .. } => {
                {
                    let mut track = conn.track.lock().expect("track lock");
                    track.reported.retain(|t| t.tab_id != tab_id);
                    track.guarded.remove(&tab_id);
                    track.guarding.remove(&tab_id);
                }
                conn.publish(&self.guard);
                self.devices_changed(&conn.user_id);
            }
            ExtensionInbound::Pair { .. } | ExtensionInbound::Hello { .. } => {
                tracing::warn!(device_id = %conn.device_id, "browser device repeated its first frame; ignored");
            }
        }
    }

    fn on_state(self: &Arc<Self>, conn: &Arc<DeviceConn>, new_tabs: bool, tabs: Vec<ExtensionTab>) {
        let (to_guard, moved): (Vec<i64>, Vec<i64>) = {
            let mut track = conn.track.lock().expect("track lock");
            let ids: HashSet<i64> = tabs.iter().map(|t| t.tab_id).collect();
            // Guarded tabs whose URL changed: re-read their frames.
            let moved: Vec<i64> = tabs
                .iter()
                .filter(|t| track.guarded.contains_key(&t.tab_id))
                .filter(|t| {
                    track
                        .reported
                        .iter()
                        .find(|o| o.tab_id == t.tab_id)
                        .is_none_or(|o| o.url != t.url)
                })
                .map(|t| t.tab_id)
                .collect();
            track.guarded.retain(|id, _| ids.contains(id));
            track.guarding.retain(|id| ids.contains(id));
            track.reported = tabs;
            track.new_tabs = new_tabs;
            let fresh: Vec<i64> = ids
                .iter()
                .copied()
                .filter(|id| !track.guarded.contains_key(id) && !track.guarding.contains(id))
                .collect();
            track.guarding.extend(fresh.iter().copied());
            (fresh, moved)
        };
        conn.frames.lock().expect("frames lock").retain(|id, _| {
            conn.track
                .lock()
                .expect("track lock")
                .reported
                .iter()
                .any(|t| t.tab_id == *id)
        });
        conn.publish(&self.guard);
        self.devices_changed(&conn.user_id);
        for tab_id in moved {
            let conn = conn.clone();
            tokio::spawn(async move { refresh_frames(&conn, tab_id).await });
        }
        for tab_id in to_guard {
            let hub = self.clone();
            let conn = conn.clone();
            tokio::spawn(async move { hub.guard_tab(&conn, tab_id).await });
        }
    }

    /// Turn the Fetch guard on for a tab, then expose it.
    async fn guard_tab(&self, conn: &Arc<DeviceConn>, tab_id: i64) {
        let mut patterns: Vec<Value> = self
            .guard
            .fetch_patterns()
            .into_iter()
            .map(|p| json!({"urlPattern": p, "requestStage": "Request"}))
            .collect();
        patterns.extend(high_value_fetch_patterns());
        let enabled = conn.call(tab_id, "Fetch.enable", json!({"patterns": patterns})).await;
        if let Err(e) = enabled {
            tracing::warn!(device_id = %conn.device_id, tab_id, error = %e.message, "browser device: Fetch guard not enabled; tab stays hidden");
            conn.track.lock().expect("track lock").guarding.remove(&tab_id);
            return;
        }
        // The main frame id equals the page's target id; best effort.
        let main_frame = conn
            .call(tab_id, "Target.getTargetInfo", json!({}))
            .await
            .ok()
            .and_then(|v| v["targetInfo"]["targetId"].as_str().map(str::to_owned));
        // Know the frames (a high-value iframe) before agents see the tab.
        refresh_frames(conn, tab_id).await;
        {
            let mut track = conn.track.lock().expect("track lock");
            if !track.guarding.remove(&tab_id) {
                return; // unshared meanwhile
            }
            track.guarded.insert(tab_id, main_frame);
        }
        conn.publish(&self.guard);
        self.devices_changed(&conn.user_id);
    }

    fn on_event(&self, conn: &Arc<DeviceConn>, tab_id: i64, method: String, params: Value) {
        match method.as_str() {
            "Fetch.requestPaused" => {
                self.on_paused(conn, tab_id, &params);
                return;
            }
            m if m.starts_with("Fetch.") => return,
            "Page.frameNavigated" => {
                let frame = &params["frame"];
                if let (Some(id), Some(url)) = (frame["id"].as_str(), frame["url"].as_str()) {
                    if frame.get("parentId").is_none() {
                        // A new document: its old child frames are gone.
                        let mut fresh = HashMap::new();
                        fresh.insert(id.to_owned(), url.to_owned());
                        conn.replace_frames(tab_id, fresh);
                        conn.set_url(tab_id, url, &self.guard);
                    } else {
                        conn.set_frame(tab_id, id, url);
                    }
                }
            }
            "Page.frameDetached" => {
                if let Some(id) = params["frameId"].as_str() {
                    conn.remove_frame(tab_id, id);
                }
            }
            _ => {}
        }
        let _ = conn.events.send(RelayEvent::Cdp {
            tab_id,
            method: method.into(),
            params: Arc::new(params),
        });
    }

    fn on_paused(&self, conn: &Arc<DeviceConn>, tab_id: i64, params: &Value) {
        let request_id = params["requestId"].as_str().unwrap_or_default().to_owned();
        let url = params["request"]["url"].as_str().unwrap_or_default();
        if self.guard.decide(url) == GuardDecision::Block {
            let host = url::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_else(|| url.split(':').next().unwrap_or("").to_owned());
            tracing::info!(user_id = %conn.user_id, device_id = %conn.device_id, %host, "browser device: blocked a request");
            conn.notify(
                tab_id,
                "Fetch.failRequest",
                json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
            );
            return;
        }
        // A top-level navigation to a high-value host: learn the origin now,
        // before the page commits (the approval check reads it).
        let is_main = params["resourceType"].as_str() == Some("Document")
            && conn
                .main_frame(tab_id)
                .is_some_and(|f| params["frameId"].as_str() == Some(f.as_str()));
        if is_main {
            conn.set_url(tab_id, url, &self.guard);
        } else if params["resourceType"].as_str() == Some("Document")
            && let Some(frame_id) = params["frameId"].as_str()
            && is_high_value_url(url)
        {
            // A high-value document loading in a (sub)frame: the tab is
            // high-value from now on (conservative; the next frame-tree
            // read corrects it).
            conn.set_frame(tab_id, frame_id, url);
        }
        conn.notify(tab_id, "Fetch.continueRequest", json!({"requestId": request_id}));
    }
}

/// Re-read a tab's frames (`Page.getFrameTree`); keeps the old ones when the
/// call fails.
async fn refresh_frames(conn: &Arc<DeviceConn>, tab_id: i64) {
    let Ok(tree) = conn.call(tab_id, "Page.getFrameTree", json!({})).await else {
        return;
    };
    let mut frames = HashMap::new();
    collect_frames(&tree["frameTree"], &mut frames);
    conn.replace_frames(tab_id, frames);
}

fn collect_frames(node: &Value, out: &mut HashMap<String, String>) {
    if let (Some(id), Some(url)) = (node["frame"]["id"].as_str(), node["frame"]["url"].as_str()) {
        out.insert(id.to_owned(), url.to_owned());
    }
    if let Some(children) = node["childFrames"].as_array() {
        for c in children {
            collect_frames(c, out);
        }
    }
}
