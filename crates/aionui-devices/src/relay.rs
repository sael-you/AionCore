//! The device relay (C38 W2, TASK-136 K2): `WS /api/devices/relay`, used by
//! the Mac helper and the Android companion app.
//!
//! - Non-browser clients only: the route refuses any `Origin` header (the
//!   check is in `routes.rs`, before the upgrade). No session either: the
//!   first frame authenticates, within [`FIRST_FRAME_TIMEOUT`].
//! - `pair {code, kind, …}` redeems a one-time code from
//!   `POST /api/devices/pairing-codes` (the shared C34 implementation,
//!   `aionui_common::pairing`) and answers `welcome {client_id, token}` —
//!   the only time the token leaves AionCore; only its SHA-256 is stored.
//!   `hello {token, kind, …}` answers `welcome {client_id}`.
//! - `inventory` refreshes the client's phones / Mac / add-ons (rows of
//!   migration 911). A device missing from the latest inventory is offline.
//! - `request {id, device, verb, args}` → `response {id, ok, …}`; the caller
//!   waits at most [`REQUEST_TIMEOUT`].
//! - A switch going off (owner, timeout, revoke) sends `stop {device}`.
//! - A client silent for 60 s is closed; `ping` → `pong`.
//!
//! Tokens and codes are never logged.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aionui_api_types::devices::{
    DeviceErrorCode, DevicePairingCode, DevicePairingKind, RELAY_DEVICE_MAC, RELAY_DEVICE_SELF, RELAY_IDLE_CLOSE_SECS,
    RelayAddon, RelayClientKind, RelayError, RelayInbound, RelayMac, RelayOutbound, RelayPhone, relay_error,
    relay_event,
};
use aionui_common::now_ms;
use aionui_common::pairing::{PairingCodes, RedeemOutcome, new_device_token, token_sha256_hex};
use aionui_db::{DevicePairingRow, UpsertDeviceParams};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{info, warn};

use crate::error::DevicesError;
use crate::service::DevicesService;

pub const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_LABEL: usize = 64;

/// One event from a relay client: `(device, name, data)`.
#[derive(Debug, Clone)]
pub struct RelayEvent {
    pub pairing_id: String,
    pub device: String,
    pub name: String,
    pub data: Arc<Value>,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, RelayError>>>;

/// One connected relay client.
pub struct RelayClient {
    pub pairing_id: String,
    pub user_id: String,
    pub kind: RelayClientKind,
    /// Verbs the client said it serves (`pair` / `hello` capabilities).
    capabilities: HashSet<String>,
    out: mpsc::UnboundedSender<RelayOutbound>,
    next_id: AtomicU64,
    pending: std::sync::Mutex<Pending>,
    alive: watch::Sender<bool>,
    /// Device names (`serial`, `self`, `mac`) in the latest inventory.
    present: std::sync::Mutex<HashSet<String>>,
    /// Latest foreground package per device (from `foreground` events).
    foreground: std::sync::Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for RelayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayClient")
            .field("pairing_id", &self.pairing_id)
            .finish_non_exhaustive()
    }
}

impl RelayClient {
    fn is_alive(&self) -> bool {
        *self.alive.borrow()
    }

    async fn closed(&self) {
        let mut rx = self.alive.subscribe();
        let _ = rx.wait_for(|alive| !*alive).await;
    }

    fn shutdown(&self) {
        self.alive.send_replace(false);
        let pending: Vec<_> = self.pending.lock().expect("pending lock").drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Err(RelayError {
                code: relay_error::FAILED.into(),
                message: "the device went offline".into(),
            }));
        }
    }

    pub fn serves(&self, verb: &str) -> bool {
        self.capabilities.contains(verb)
    }

    pub fn is_present(&self, device: &str) -> bool {
        self.present.lock().expect("present lock").contains(device)
    }

    pub fn foreground(&self, device: &str) -> Option<String> {
        self.foreground.lock().expect("foreground lock").get(device).cloned()
    }

    fn send(&self, frame: RelayOutbound) -> bool {
        self.is_alive() && self.out.send(frame).is_ok()
    }

    /// Send one request and wait for its response.
    pub async fn call(&self, device: &str, verb: &str, args: Value) -> Result<Value, RelayError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        if !self.send(RelayOutbound::Request {
            id,
            device: device.to_owned(),
            verb: verb.to_owned(),
            args,
        }) {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(RelayError {
                code: relay_error::FAILED.into(),
                message: "the device went offline".into(),
            });
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RelayError {
                code: relay_error::FAILED.into(),
                message: "the device went offline".into(),
            }),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(RelayError {
                    code: relay_error::TIMEOUT.into(),
                    message: "the device did not answer in time".into(),
                })
            }
        }
    }

    fn resolve(&self, id: u64, ok: bool, result: Option<Value>, error: Option<RelayError>) {
        let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) else {
            return;
        };
        let _ = tx.send(if ok {
            Ok(result.unwrap_or_else(|| json!({})))
        } else {
            Err(error.unwrap_or(RelayError {
                code: relay_error::FAILED.into(),
                message: "the device reported an error".into(),
            }))
        });
    }
}

/// Relay state kept by [`DevicesService`].
pub struct RelayState {
    codes: PairingCodes<(String, DevicePairingKind)>,
    clients: dashmap::DashMap<String, Arc<RelayClient>>,
    events: broadcast::Sender<RelayEvent>,
}

impl Default for RelayState {
    fn default() -> Self {
        Self {
            codes: PairingCodes::new(PAIRING_TTL),
            clients: dashmap::DashMap::new(),
            events: broadcast::channel(1024).0,
        }
    }
}

fn clean_label(raw: &str, fallback: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL)
        .collect::<String>()
        .trim()
        .to_owned();
    if s.is_empty() { fallback.to_owned() } else { s }
}

fn pairing_kind_of(kind: RelayClientKind) -> DevicePairingKind {
    match kind {
        RelayClientKind::MacHelper => DevicePairingKind::MacHelper,
        RelayClientKind::Companion => DevicePairingKind::Companion,
    }
}

fn kind_str(kind: RelayClientKind) -> &'static str {
    match kind {
        RelayClientKind::MacHelper => "mac_helper",
        RelayClientKind::Companion => "companion",
    }
}

/// The device row id of `(pairing, device name)`: stable across reconnects.
pub fn relay_device_id(kind: &str, pairing_id: &str, source_id: &str) -> String {
    format!("{kind}:{pairing_id}:{source_id}")
}

/// Map a peer error to the registry's codes.
pub fn relay_error_code(error: &RelayError) -> DeviceErrorCode {
    match error.code.as_str() {
        relay_error::SWITCH_OFF => DeviceErrorCode::DeviceDisabled,
        relay_error::UNKNOWN_DEVICE => DeviceErrorCode::DeviceOffline,
        relay_error::REFUSED => DeviceErrorCode::DeviceVerbRefused,
        relay_error::BUSY => DeviceErrorCode::DeviceBusy,
        relay_error::BAD_ARGS => DeviceErrorCode::DeviceBadMessage,
        relay_error::TIMEOUT => DeviceErrorCode::DeviceOffline,
        _ => DeviceErrorCode::DeviceVerbRefused,
    }
}

impl DevicesService {
    // -- pairing codes ----------------------------------------------------------

    /// `POST /api/devices/pairing-codes`. `public_host` is the host the owner
    /// reaches AionUi under (for the companion's QR payload).
    pub fn pairing_code(
        &self,
        user_id: &str,
        kind: DevicePairingKind,
        public_host: Option<&str>,
    ) -> Result<DevicePairingCode, DevicesError> {
        if kind == DevicePairingKind::WifiDirect {
            return Err(DevicesError::coded(
                DeviceErrorCode::DeviceVerbRefused,
                "Wi-Fi direct needs a test first",
            ));
        }
        let expires_at = now_ms() + i64::try_from(PAIRING_TTL.as_millis()).unwrap_or(0);
        let code = self.relay().codes.issue((user_id.to_owned(), kind));
        let qr_payload = (kind == DevicePairingKind::Companion)
            .then(|| format!("aionui-pair:v1?host={}&code={}", public_host.unwrap_or_default(), code));
        let download_url = (kind == DevicePairingKind::Companion)
            .then(|| self.companion_downloads().and_then(|d| d.link(public_host)))
            .flatten();
        Ok(DevicePairingCode {
            code,
            expires_at,
            download_url,
            qr_payload,
        })
    }

    // -- relay clients ----------------------------------------------------------

    pub fn relay_client(&self, user_id: &str, pairing_id: &str) -> Option<Arc<RelayClient>> {
        self.relay()
            .clients
            .get(pairing_id)
            .map(|c| c.clone())
            .filter(|c| c.user_id == user_id && c.is_alive())
    }

    pub fn subscribe_relay_events(&self) -> broadcast::Receiver<RelayEvent> {
        self.relay().events.subscribe()
    }

    /// Whether `(pairing, device)` is reachable right now.
    pub(crate) fn relay_present(&self, pairing_id: &str, device: &str) -> bool {
        self.relay()
            .clients
            .get(pairing_id)
            .is_some_and(|c| c.is_alive() && c.is_present(device))
    }

    /// Tell the client that reaches `pairing_id/device` to stop at once.
    pub(crate) fn relay_stop(&self, pairing_id: &str, device: &str) {
        if let Some(c) = self.relay().clients.get(pairing_id) {
            c.send(RelayOutbound::Stop {
                device: device.to_owned(),
            });
        }
    }

    /// Close a client's socket (revoke).
    pub(crate) fn relay_disconnect(&self, pairing_id: &str) {
        if let Some((_, c)) = self.relay().clients.remove(pairing_id) {
            c.send(RelayOutbound::Error {
                code: DeviceErrorCode::DeviceRevoked,
            });
            c.shutdown();
        }
    }

    /// Serve one relay connection until it closes.
    pub async fn run_relay(self: Arc<Self>, socket: WebSocket) {
        let (mut sink, mut stream) = socket.split();
        let first = match tokio::time::timeout(FIRST_FRAME_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<RelayInbound>(&t).ok(),
            _ => None,
        };
        let auth = match first {
            Some(frame) => self.authenticate(frame).await,
            None => Err(DeviceErrorCode::DeviceBadMessage),
        };
        let (pairing, token) = match auth {
            Ok(a) => a,
            Err(code) => {
                let text = serde_json::to_string(&RelayOutbound::Error { code }).unwrap_or_default();
                let _ = sink.send(Message::Text(text.into())).await;
                let _ = sink.close().await;
                return;
            }
        };
        let kind = if pairing.kind == "companion" {
            RelayClientKind::Companion
        } else {
            RelayClientKind::MacHelper
        };
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<RelayOutbound>();
        let client = Arc::new(RelayClient {
            pairing_id: pairing.id.clone(),
            user_id: pairing.user_id.clone(),
            kind,
            capabilities: serde_json::from_str::<Vec<String>>(&pairing.capabilities)
                .unwrap_or_default()
                .into_iter()
                .collect(),
            out: out_tx,
            next_id: AtomicU64::new(1),
            pending: std::sync::Mutex::new(HashMap::new()),
            alive: watch::channel(true).0,
            present: std::sync::Mutex::new(HashSet::new()),
            foreground: std::sync::Mutex::new(HashMap::new()),
        });
        let _ = client.out.send(RelayOutbound::Welcome {
            client_id: pairing.id.clone(),
            token,
        });
        if let Some(old) = self.relay().clients.insert(pairing.id.clone(), client.clone()) {
            old.shutdown();
        }
        self.set_pairing_online(&pairing.id, true);
        info!(user_id = %pairing.user_id, pairing_id = %pairing.id, kind = %pairing.kind, "device relay client connected");
        self.changed(&pairing.user_id, None);

        let writer_client = client.clone();
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = out_rx.recv() => {
                        let Some(frame) = frame else { break };
                        let is_error = matches!(frame, RelayOutbound::Error { .. });
                        let text = serde_json::to_string(&frame).unwrap_or_default();
                        if sink.send(Message::Text(text.into())).await.is_err() || is_error {
                            break;
                        }
                    }
                    () = writer_client.closed() => {
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

        let idle = Duration::from_secs(RELAY_IDLE_CLOSE_SECS);
        loop {
            tokio::select! {
                msg = tokio::time::timeout(idle, stream.next()) => {
                    let Ok(Some(Ok(msg))) = msg else { break };
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    match serde_json::from_str::<RelayInbound>(&text) {
                        Ok(frame) => self.on_relay_frame(&client, frame).await,
                        Err(_) => warn!(pairing_id = %client.pairing_id, "device relay client sent an unreadable frame"),
                    }
                }
                () = client.closed() => break,
            }
        }
        client.shutdown();
        let _ = writer.await;
        let current = self
            .relay()
            .clients
            .remove_if(&client.pairing_id, |_, c| Arc::ptr_eq(c, &client))
            .is_some();
        if current {
            self.set_pairing_online(&client.pairing_id, false);
            let _ = self
                .repo()
                .touch_pairing(
                    &client.pairing_id,
                    &pairing.version.clone().unwrap_or_default(),
                    &pairing.capabilities,
                    now_ms(),
                )
                .await;
            self.changed(&client.user_id, None);
        }
        info!(user_id = %client.user_id, pairing_id = %client.pairing_id, "device relay client disconnected");
    }

    async fn authenticate(&self, frame: RelayInbound) -> Result<(DevicePairingRow, Option<String>), DeviceErrorCode> {
        match frame {
            RelayInbound::Pair {
                code,
                kind,
                name,
                version,
                capabilities,
            } => {
                let (user_id, issued_kind) = match self.relay().codes.redeem(&code) {
                    RedeemOutcome::Paired(key) => key,
                    RedeemOutcome::Invalid { burned } => {
                        for (user_id, _) in burned {
                            warn!(user_id, "device pairing code burned after too many wrong attempts");
                        }
                        return Err(DeviceErrorCode::DevicePairingInvalid);
                    }
                };
                if issued_kind != pairing_kind_of(kind) {
                    return Err(DeviceErrorCode::DevicePairingInvalid);
                }
                let token = new_device_token();
                let now = now_ms();
                let row = DevicePairingRow {
                    id: format!("pair_{}", uuid::Uuid::new_v4().simple()),
                    user_id: user_id.clone(),
                    kind: kind_str(kind).to_owned(),
                    name: clean_label(
                        &name,
                        if kind == RelayClientKind::Companion {
                            "Phone"
                        } else {
                            "Mac"
                        },
                    ),
                    token_sha256: token_sha256_hex(&token),
                    version: Some(clean_label(&version, "")),
                    capabilities: serde_json::to_string(&capabilities).unwrap_or_else(|_| "[]".into()),
                    created_at: now,
                    last_seen_at: Some(now),
                    revoked_at: None,
                };
                self.repo().insert_pairing(&row).await.map_err(|error| {
                    warn!(user_id, error = %error, "device pairing not saved");
                    DeviceErrorCode::DeviceBadMessage
                })?;
                // The card that stands for the client itself.
                let (dkind, source) = match kind {
                    RelayClientKind::MacHelper => ("mac", RELAY_DEVICE_MAC),
                    RelayClientKind::Companion => ("phone", RELAY_DEVICE_SELF),
                };
                let _ = self
                    .repo()
                    .upsert_device(UpsertDeviceParams {
                        id: &relay_device_id(dkind, &row.id, source),
                        user_id: &user_id,
                        kind: dkind,
                        pairing_id: Some(&row.id),
                        source_id: source,
                        name: &row.name,
                        meta: "{}",
                        now,
                    })
                    .await;
                info!(user_id, pairing_id = %row.id, kind = %row.kind, "device relay client paired");
                Ok((row, Some(token)))
            }
            RelayInbound::Hello {
                token,
                kind,
                version,
                capabilities,
            } => {
                let hash = token_sha256_hex(&token);
                let row = self
                    .repo()
                    .find_pairing_by_token_hash(&hash)
                    .await
                    .ok()
                    .flatten()
                    .ok_or(DeviceErrorCode::DeviceRevoked)?;
                if row.kind != kind_str(kind) {
                    return Err(DeviceErrorCode::DeviceRevoked);
                }
                let caps = serde_json::to_string(&capabilities).unwrap_or_else(|_| "[]".into());
                let version = clean_label(&version, "");
                let _ = self.repo().touch_pairing(&row.id, &version, &caps, now_ms()).await;
                Ok((
                    DevicePairingRow {
                        version: Some(version),
                        capabilities: caps,
                        ..row
                    },
                    None,
                ))
            }
            _ => Err(DeviceErrorCode::DeviceBadMessage),
        }
    }

    async fn on_relay_frame(&self, client: &Arc<RelayClient>, frame: RelayInbound) {
        match frame {
            RelayInbound::Ping => {
                client.send(RelayOutbound::Pong);
            }
            RelayInbound::Response { id, ok, result, error } => client.resolve(id, ok, result, error),
            RelayInbound::Inventory { phones, mac, addons } => self.on_inventory(client, phones, mac, addons).await,
            RelayInbound::Event { device, name, data } => {
                if name == relay_event::FOREGROUND
                    && let Some(package) = data.get("package").and_then(|p| p.as_str())
                {
                    client
                        .foreground
                        .lock()
                        .expect("foreground lock")
                        .insert(device.clone(), package.to_owned());
                }
                if name == relay_event::LOCAL_SWITCH_OFF {
                    self.switch_off_relay_device(client, &device).await;
                }
                let _ = self.relay().events.send(RelayEvent {
                    pairing_id: client.pairing_id.clone(),
                    device,
                    name,
                    data: Arc::new(data),
                });
            }
            RelayInbound::Pair { .. } | RelayInbound::Hello { .. } => {
                warn!(pairing_id = %client.pairing_id, "device relay client repeated its first frame; ignored");
            }
        }
    }

    /// The owner pressed Stop on the phone / Mac: the core switch goes off too.
    async fn switch_off_relay_device(&self, client: &RelayClient, device: &str) {
        let kind = if device == RELAY_DEVICE_MAC { "mac" } else { "phone" };
        let id = relay_device_id(kind, &client.pairing_id, device);
        if let Ok(true) = self
            .repo()
            .set_device_enabled(&client.user_id, &id, false, None, now_ms())
            .await
        {
            info!(user_id = %client.user_id, device_id = %id, "device switched off on the device itself");
            self.changed(&client.user_id, Some(&id));
        }
    }

    async fn on_inventory(
        &self,
        client: &RelayClient,
        phones: Vec<RelayPhone>,
        mac: Option<RelayMac>,
        addons: Vec<RelayAddon>,
    ) {
        let now = now_ms();
        let mut present = HashSet::new();
        let upsert = |kind: &'static str, source: String, name: String, meta: Value| async move {
            let id = relay_device_id(kind, &client.pairing_id, &source);
            let result = self
                .repo()
                .upsert_device(UpsertDeviceParams {
                    id: &id,
                    user_id: &client.user_id,
                    kind,
                    pairing_id: Some(&client.pairing_id),
                    source_id: &source,
                    name: &name,
                    meta: &meta.to_string(),
                    now,
                })
                .await;
            if let Err(error) = result {
                warn!(pairing_id = %client.pairing_id, error = %error, "device inventory row not saved");
            }
        };
        for p in phones {
            let source = if client.kind == RelayClientKind::Companion {
                RELAY_DEVICE_SELF.to_owned()
            } else {
                clean_label(&p.serial, "phone")
            };
            present.insert(source.clone());
            let meta = json!({
                "model": p.model, "android_version": p.android_version, "transport": p.transport,
                "chrome": p.chrome, "peer_enabled": p.enabled,
            });
            upsert("phone", source, clean_label(&p.name, "Phone"), meta).await;
        }
        if let Some(m) = mac {
            present.insert(RELAY_DEVICE_MAC.to_owned());
            let meta = json!({
                "mac_reported": true, "os_version": m.os_version, "permissions_ok": m.permissions_ok,
                "peer_enabled": m.enabled,
            });
            upsert("mac", RELAY_DEVICE_MAC.to_owned(), clean_label(&m.name, "Mac"), meta).await;
        }
        for a in addons {
            let name = clean_label(&a.name, "addon");
            present.insert(format!("addon:{name}"));
            let meta = json!({"version": a.version, "disk_bytes": a.disk_bytes});
            upsert("addon", name.clone(), name, meta).await;
        }
        *client.present.lock().expect("present lock") = present;
        self.changed(&client.user_id, None);
    }
}
