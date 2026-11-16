//! The WebUI panel's socket (`WS /api/browser/ws?instance=n`, C30): one CDP
//! session of AionCore's own on the active tab, a client-paced JPEG
//! screencast, input forwarding, dialogs, tabs and history. Navigations from
//! the panel pass the same guard as everything else.
//!
//! Binary frame: `[u32 BE meta_len][BrowserFrameMeta JSON][JPEG]`.

use std::sync::Arc;

use aionui_api_types::{
    BrowserClientMessage, BrowserDialog, BrowserDialogKind, BrowserErrorCode, BrowserFileChooser,
    BrowserFileChooserMode, BrowserFrameMeta, BrowserKeyEventKind, BrowserKeyInput, BrowserMouseButton,
    BrowserMouseEventKind, BrowserMouseInput, BrowserRunStatus, BrowserServerMessage, BrowserState, BrowserTab,
    BrowserWsError,
};
use axum::extract::ws::{Message, WebSocket};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::cdp::CdpEvent;
use crate::error::BrowserError;
use crate::guard::{GuardDecision, NavigationGuard};
use crate::instance::ChromeInstance;

/// What the panel needs from the service.
#[async_trait::async_trait]
pub trait PanelBackend: Send + Sync {
    /// The running instance, if any (no start).
    fn running(&self, user_id: &str, instance: u8) -> Option<Arc<ChromeInstance>>;
    /// Start (or return) the instance for the owner's panel.
    async fn start(&self, user_id: &str, instance: u8) -> Result<Arc<ChromeInstance>, BrowserError>;
    fn guard(&self) -> Arc<NavigationGuard>;
    fn max_tabs(&self, user_id: &str) -> u8;
}

// ---------------------------------------------------------------------------
// Pure mapping (unit-tested)
// ---------------------------------------------------------------------------

pub(crate) fn mouse_params(m: &BrowserMouseInput) -> Value {
    let kind = match m.event {
        BrowserMouseEventKind::Move => "mouseMoved",
        BrowserMouseEventKind::Down => "mousePressed",
        BrowserMouseEventKind::Up => "mouseReleased",
        BrowserMouseEventKind::Wheel => "mouseWheel",
    };
    let button = match m.button {
        BrowserMouseButton::None => "none",
        BrowserMouseButton::Left => "left",
        BrowserMouseButton::Middle => "middle",
        BrowserMouseButton::Right => "right",
    };
    let mut p = json!({
        "type": kind,
        "x": m.x,
        "y": m.y,
        "button": button,
        "clickCount": m.click_count,
        "modifiers": m.modifiers,
    });
    if m.event == BrowserMouseEventKind::Wheel {
        p["deltaX"] = json!(m.delta_x.unwrap_or(0.0));
        p["deltaY"] = json!(m.delta_y.unwrap_or(0.0));
    }
    p
}

pub(crate) fn key_params(k: &BrowserKeyInput) -> Value {
    let kind = match k.event {
        // keyDown with text types the character; rawKeyDown does not.
        BrowserKeyEventKind::Down if k.text.as_deref().is_some_and(|t| !t.is_empty()) => "keyDown",
        BrowserKeyEventKind::Down => "rawKeyDown",
        BrowserKeyEventKind::Up => "keyUp",
        BrowserKeyEventKind::Char => "char",
    };
    let mut p = json!({
        "type": kind,
        "key": k.key,
        "code": k.code,
        "windowsVirtualKeyCode": k.key_code,
        "nativeVirtualKeyCode": k.key_code,
        "modifiers": k.modifiers,
    });
    if let Some(text) = &k.text {
        p["text"] = json!(text);
        p["unmodifiedText"] = json!(text);
    }
    p
}

/// What the address bar typed → a URL (`discord.com` → `https://discord.com`).
pub(crate) fn normalize_url(input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    // A recognised scheme stays as typed, so the guard judges it (`file:`,
    // `chrome:` …); `localhost:3000` parses with scheme "localhost" and gets
    // the https prefix instead.
    const SCHEMES: &[&str] = &[
        "http",
        "https",
        "about",
        "data",
        "blob",
        "javascript",
        "file",
        "chrome",
        "chrome-untrusted",
        "chrome-extension",
        "chrome-search",
        "devtools",
        "view-source",
        "filesystem",
    ];
    if url::Url::parse(t).is_ok_and(|u| u.has_host() || SCHEMES.contains(&u.scheme())) {
        return Some(t.to_owned());
    }
    let candidate = format!("https://{t}");
    url::Url::parse(&candidate)
        .ok()
        .filter(|u| u.has_host())
        .map(|_| candidate)
}

/// Encode one screencast frame for the panel.
pub(crate) fn encode_frame(meta: &BrowserFrameMeta, jpeg: &[u8]) -> Vec<u8> {
    let meta_json = serde_json::to_vec(meta).unwrap_or_default();
    let mut out = Vec::with_capacity(4 + meta_json.len() + jpeg.len());
    out.extend_from_slice(&(meta_json.len() as u32).to_be_bytes());
    out.extend_from_slice(&meta_json);
    out.extend_from_slice(jpeg);
    out
}

fn dialog_kind(kind: &str) -> BrowserDialogKind {
    match kind {
        "confirm" => BrowserDialogKind::Confirm,
        "prompt" => BrowserDialogKind::Prompt,
        "beforeunload" => BrowserDialogKind::Beforeunload,
        _ => BrowserDialogKind::Alert,
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Attached {
    inst: Arc<ChromeInstance>,
    session: String,
    target: String,
}

struct Panel {
    user_id: String,
    instance: u8,
    backend: Arc<dyn PanelBackend>,
    attached: Option<Attached>,
    width: u32,
    height: u32,
    dsf: f64,
    seq: u64,
    in_flight: u32,
    /// Chrome's frame number waiting for an ack (client pacing).
    pending_ack: Option<i64>,
    loading: bool,
    can_back: bool,
    can_forward: bool,
    history: Option<(i64, Vec<i64>)>,
}

type Out = futures_util::stream::SplitSink<WebSocket, Message>;

async fn send_json(out: &mut Out, msg: &BrowserServerMessage) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => out.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => true,
    }
}

async fn send_error(out: &mut Out, code: BrowserErrorCode, message: Option<String>) -> bool {
    send_json(out, &BrowserServerMessage::Error(BrowserWsError { code, message })).await
}

impl Panel {
    fn state(&self) -> BrowserState {
        let max_tabs = self.backend.max_tabs(&self.user_id);
        let Some(a) = &self.attached else {
            let running = self.backend.running(&self.user_id, self.instance);
            return BrowserState {
                instance: self.instance,
                max_tabs,
                status: if running.is_some() {
                    BrowserRunStatus::Starting
                } else {
                    BrowserRunStatus::Stopped
                },
                error_code: None,
                chrome_version: running.map(|i| i.version.clone()),
                tabs: Vec::new(),
                can_go_back: false,
                can_go_forward: false,
                agent_active: false,
                agent_tab_id: None,
            };
        };
        let tabs = a
            .inst
            .tabs()
            .borrow()
            .iter()
            .map(|t| BrowserTab {
                id: t.id.clone(),
                url: t.url.clone(),
                title: t.title.clone(),
                active: t.id == a.target,
                loading: t.id == a.target && self.loading,
            })
            .collect();
        BrowserState {
            instance: self.instance,
            max_tabs,
            status: if a.inst.is_running() {
                BrowserRunStatus::Running
            } else {
                BrowserRunStatus::Stopped
            },
            error_code: None,
            chrome_version: Some(a.inst.version.clone()),
            tabs,
            can_go_back: self.can_back,
            can_go_forward: self.can_forward,
            agent_active: a.inst.agent_active(),
            agent_tab_id: a.inst.agent_tab(),
        }
    }

    /// Attach our own session to `target` (or the first tab) and start the screencast.
    async fn attach(&mut self, inst: Arc<ChromeInstance>, target: Option<String>) -> Result<(), BrowserError> {
        self.detach().await;
        let target = match target
            .or_else(|| inst.agent_tab())
            .filter(|t| inst.tabs().borrow().iter().any(|x| &x.id == t))
        {
            Some(t) => t,
            None => match inst.tabs().borrow().first() {
                Some(t) => t.id.clone(),
                None => String::new(),
            },
        };
        let target = if target.is_empty() {
            let created = inst
                .cdp()
                .call("Target.createTarget", json!({"url": "about:blank"}), None)
                .await?;
            created["targetId"].as_str().unwrap_or_default().to_owned()
        } else {
            target
        };
        let cdp = inst.cdp().clone();
        let attached = cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
                None,
            )
            .await?;
        let session = attached["sessionId"].as_str().unwrap_or_default().to_owned();
        let s = Some(session.as_str());
        let _ = cdp
            .call("Target.activateTarget", json!({"targetId": target}), None)
            .await;
        cdp.call("Page.enable", json!({}), s).await?;
        let _ = cdp
            .call("Page.setInterceptFileChooserDialog", json!({"enabled": true}), s)
            .await;
        self.attached = Some(Attached { inst, session, target });
        self.apply_viewport().await?;
        self.refresh_history().await;
        Ok(())
    }

    async fn detach(&mut self) {
        if let Some(a) = self.attached.take() {
            let s = Some(a.session.as_str());
            let _ = a.inst.cdp().call("Page.stopScreencast", json!({}), s).await;
            let _ = a
                .inst
                .cdp()
                .call("Target.detachFromTarget", json!({"sessionId": a.session}), None)
                .await;
        }
        self.in_flight = 0;
        self.pending_ack = None;
        self.loading = false;
    }

    async fn apply_viewport(&mut self) -> Result<(), BrowserError> {
        let Some(a) = &self.attached else { return Ok(()) };
        let s = Some(a.session.as_str());
        let cdp = a.inst.cdp();
        cdp.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": self.width, "height": self.height, "deviceScaleFactor": self.dsf, "mobile": false}),
            s,
        )
        .await?;
        let _ = cdp.call("Page.stopScreencast", json!({}), s).await;
        let px = |v: u32| ((f64::from(v) * self.dsf).round() as u32).clamp(64, 4096);
        cdp.call(
            "Page.startScreencast",
            json!({"format": "jpeg", "quality": 70, "maxWidth": px(self.width), "maxHeight": px(self.height), "everyNthFrame": 1}),
            s,
        )
        .await?;
        self.in_flight = 0;
        self.pending_ack = None;
        Ok(())
    }

    async fn refresh_history(&mut self) {
        let Some(a) = &self.attached else { return };
        if let Ok(h) = a
            .inst
            .cdp()
            .call("Page.getNavigationHistory", json!({}), Some(a.session.as_str()))
            .await
        {
            let index = h["currentIndex"].as_i64().unwrap_or(0);
            let ids: Vec<i64> = h["entries"]
                .as_array()
                .map(|e| e.iter().filter_map(|x| x["id"].as_i64()).collect())
                .unwrap_or_default();
            self.can_back = index > 0;
            self.can_forward = (index as usize) + 1 < ids.len();
            self.history = Some((index, ids));
        }
    }

    async fn go_history(&mut self, delta: i64) -> Result<(), BrowserError> {
        self.refresh_history().await;
        let (Some(a), Some((index, ids))) = (&self.attached, &self.history) else {
            return Ok(());
        };
        let next = index + delta;
        if next < 0 || next as usize >= ids.len() {
            return Ok(());
        }
        a.inst
            .cdp()
            .call(
                "Page.navigateToHistoryEntry",
                json!({"entryId": ids[next as usize]}),
                Some(a.session.as_str()),
            )
            .await?;
        Ok(())
    }

    fn require(&self) -> Result<&Attached, BrowserError> {
        self.attached
            .as_ref()
            .ok_or_else(|| BrowserError::coded(BrowserErrorCode::BrowserNotRunning, "start the browser first"))
    }

    fn guarded(&self, raw: &str) -> Result<String, BrowserError> {
        let url = normalize_url(raw)
            .ok_or_else(|| BrowserError::coded(BrowserErrorCode::BrowserBadMessage, "invalid URL"))?;
        if self.backend.guard().decide(&url) == GuardDecision::Block {
            return Err(BrowserError::coded(
                BrowserErrorCode::BrowserUrlBlocked,
                "this address is blocked",
            ));
        }
        Ok(url)
    }

    async fn handle(&mut self, msg: BrowserClientMessage, out: &mut Out) -> Result<(), BrowserError> {
        if let Some(a) = &self.attached {
            a.inst.touch();
        }
        match msg {
            BrowserClientMessage::Start => {
                let inst = self.backend.start(&self.user_id, self.instance).await?;
                inst.touch();
                self.attach(inst, None).await?;
            }
            BrowserClientMessage::Navigate { url } => {
                let url = self.guarded(&url)?;
                let a = self.require()?;
                a.inst
                    .cdp()
                    .call("Page.navigate", json!({"url": url}), Some(a.session.as_str()))
                    .await?;
            }
            BrowserClientMessage::Back => self.go_history(-1).await?,
            BrowserClientMessage::Forward => self.go_history(1).await?,
            BrowserClientMessage::Reload => {
                let a = self.require()?;
                a.inst
                    .cdp()
                    .call("Page.reload", json!({}), Some(a.session.as_str()))
                    .await?;
            }
            BrowserClientMessage::Stop => {
                let a = self.require()?;
                a.inst
                    .cdp()
                    .call("Page.stopLoading", json!({}), Some(a.session.as_str()))
                    .await?;
            }
            BrowserClientMessage::TabNew { url } => {
                let url = match url {
                    Some(u) => self.guarded(&u)?,
                    None => "about:blank".to_owned(),
                };
                let inst = self.require()?.inst.clone();
                if inst.tab_count() >= usize::from(inst.max_tabs()) {
                    return Err(BrowserError::coded(
                        BrowserErrorCode::BrowserTabLimit,
                        "the tab limit is reached",
                    ));
                }
                let created = inst
                    .cdp()
                    .call("Target.createTarget", json!({"url": url}), None)
                    .await?;
                let target = created["targetId"].as_str().map(str::to_owned);
                self.attach(inst, target).await?;
            }
            BrowserClientMessage::TabSelect { id } => {
                let inst = self.require()?.inst.clone();
                if !inst.tabs().borrow().iter().any(|t| t.id == id) {
                    return Err(BrowserError::coded(BrowserErrorCode::BrowserTabNotFound, "no such tab"));
                }
                self.attach(inst, Some(id)).await?;
            }
            BrowserClientMessage::TabClose { id } => {
                let a = self.require()?;
                let inst = a.inst.clone();
                let was_active = a.target == id;
                inst.cdp()
                    .call("Target.closeTarget", json!({"targetId": id}), None)
                    .await?;
                if was_active {
                    self.attached = None;
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    self.attach(inst, None).await?;
                }
            }
            BrowserClientMessage::Resize {
                width,
                height,
                device_scale_factor,
            } => {
                if !(100..=4096).contains(&width)
                    || !(100..=4096).contains(&height)
                    || !(0.5..=4.0).contains(&device_scale_factor)
                {
                    return Err(BrowserError::coded(
                        BrowserErrorCode::BrowserBadMessage,
                        "viewport out of range",
                    ));
                }
                self.width = width;
                self.height = height;
                self.dsf = device_scale_factor;
                self.apply_viewport().await?;
            }
            BrowserClientMessage::Mouse(m) => {
                let a = self.require()?;
                a.inst
                    .cdp()
                    .call("Input.dispatchMouseEvent", mouse_params(&m), Some(a.session.as_str()))
                    .await?;
            }
            BrowserClientMessage::Key(k) => {
                let a = self.require()?;
                a.inst
                    .cdp()
                    .call("Input.dispatchKeyEvent", key_params(&k), Some(a.session.as_str()))
                    .await?;
            }
            BrowserClientMessage::InsertText { text } => {
                let a = self.require()?;
                a.inst
                    .cdp()
                    .call("Input.insertText", json!({"text": text}), Some(a.session.as_str()))
                    .await?;
            }
            BrowserClientMessage::DialogReply {
                tab_id: _,
                accept,
                prompt_text,
            } => {
                let a = self.require()?;
                let mut p = json!({"accept": accept});
                if let Some(t) = prompt_text {
                    p["promptText"] = json!(t);
                }
                a.inst
                    .cdp()
                    .call("Page.handleJavaScriptDialog", p, Some(a.session.as_str()))
                    .await?;
            }
            BrowserClientMessage::FrameAck { seq: _ } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                if let (Some(frame), Some(a)) = (self.pending_ack.take(), &self.attached) {
                    a.inst.cdp().notify(
                        "Page.screencastFrameAck",
                        json!({"sessionId": frame}),
                        Some(a.session.as_str()),
                    );
                }
            }
        }
        let _ = send_json(out, &BrowserServerMessage::State(self.state())).await;
        Ok(())
    }

    /// A CDP event of our session. Returns `false` when the socket is gone.
    async fn on_event(&mut self, ev: &CdpEvent, out: &mut Out) -> bool {
        let Some(a) = &self.attached else { return true };
        if ev.session_id.as_deref() != Some(a.session.as_str()) {
            if ev.method == "Target.detachedFromTarget" && ev.params["sessionId"].as_str() == Some(a.session.as_str()) {
                let inst = a.inst.clone();
                self.attached = None;
                let _ = self.attach(inst, None).await;
                return send_json(out, &BrowserServerMessage::State(self.state())).await;
            }
            return true;
        }
        match ev.method.as_str() {
            "Page.screencastFrame" => {
                let frame_id = ev.params["sessionId"].as_i64().unwrap_or_default();
                let Ok(jpeg) =
                    base64::engine::general_purpose::STANDARD.decode(ev.params["data"].as_str().unwrap_or(""))
                else {
                    return true;
                };
                let m = &ev.params["metadata"];
                self.seq += 1;
                let meta = BrowserFrameMeta {
                    seq: self.seq,
                    tab_id: a.target.clone(),
                    width: m["deviceWidth"].as_f64().unwrap_or(f64::from(self.width)),
                    height: m["deviceHeight"].as_f64().unwrap_or(f64::from(self.height)),
                    device_scale_factor: self.dsf,
                    page_scale_factor: m["pageScaleFactor"].as_f64().unwrap_or(1.0),
                    offset_top: m["offsetTop"].as_f64().unwrap_or(0.0),
                    scroll_x: m["scrollOffsetX"].as_f64(),
                    scroll_y: m["scrollOffsetY"].as_f64(),
                };
                if out
                    .send(Message::Binary(encode_frame(&meta, &jpeg).into()))
                    .await
                    .is_err()
                {
                    return false;
                }
                self.in_flight += 1;
                if self.in_flight < 2 {
                    a.inst.cdp().notify(
                        "Page.screencastFrameAck",
                        json!({"sessionId": frame_id}),
                        Some(a.session.as_str()),
                    );
                } else {
                    self.pending_ack = Some(frame_id);
                }
                true
            }
            "Page.javascriptDialogOpening" => {
                let d = BrowserDialog {
                    tab_id: a.target.clone(),
                    kind: dialog_kind(ev.params["type"].as_str().unwrap_or("alert")),
                    message: ev.params["message"].as_str().unwrap_or_default().to_owned(),
                    default_prompt: ev.params["defaultPrompt"].as_str().map(str::to_owned),
                };
                send_json(out, &BrowserServerMessage::Dialog(d)).await
            }
            "Page.fileChooserOpened" => {
                let mode = if ev.params["mode"].as_str() == Some("selectMultiple") {
                    BrowserFileChooserMode::Multiple
                } else {
                    BrowserFileChooserMode::Single
                };
                send_json(
                    out,
                    &BrowserServerMessage::FileChooser(BrowserFileChooser {
                        tab_id: a.target.clone(),
                        mode,
                    }),
                )
                .await
            }
            "Page.frameStartedLoading" | "Page.frameStoppedLoading" => {
                self.loading = ev.method == "Page.frameStartedLoading";
                send_json(out, &BrowserServerMessage::State(self.state())).await
            }
            "Page.frameNavigated" | "Page.navigatedWithinDocument" => {
                self.refresh_history().await;
                send_json(out, &BrowserServerMessage::State(self.state())).await
            }
            _ => true,
        }
    }
}

/// Run one panel socket until it closes.
pub async fn run_panel(socket: WebSocket, backend: Arc<dyn PanelBackend>, user_id: String, instance: u8) {
    let (mut out, mut incoming) = socket.split();
    let mut panel = Panel {
        user_id,
        instance,
        backend: backend.clone(),
        attached: None,
        width: 1280,
        height: 800,
        dsf: 1.0,
        seq: 0,
        in_flight: 0,
        pending_ack: None,
        loading: false,
        can_back: false,
        can_forward: false,
        history: None,
    };
    if let Some(inst) = backend.running(&panel.user_id, instance)
        && let Err(e) = panel.attach(inst, None).await
    {
        let _ = send_error(&mut out, e.code().unwrap_or(BrowserErrorCode::BrowserNotRunning), None).await;
    }
    if !send_json(&mut out, &BrowserServerMessage::State(panel.state())).await {
        return;
    }

    loop {
        let (mut events, mut tabs, mut notices, inst) = match &panel.attached {
            Some(a) => (
                Some(a.inst.cdp().subscribe()),
                Some(a.inst.tabs()),
                Some(a.inst.notices()),
                Some(a.inst.clone()),
            ),
            None => (None, None, None, None),
        };
        let reattach = loop {
            tokio::select! {
                msg = incoming.next() => {
                    let Some(Ok(msg)) = msg else { panel.detach().await; return };
                    let text = match msg {
                        Message::Text(t) => t.to_string(),
                        Message::Close(_) => { panel.detach().await; return }
                        _ => continue,
                    };
                    let before = panel.attached.as_ref().map(|a| a.session.clone());
                    match serde_json::from_str::<BrowserClientMessage>(&text) {
                        Ok(msg) => {
                            if let Err(e) = panel.handle(msg, &mut out).await {
                                let code = e.code().unwrap_or(BrowserErrorCode::BrowserNotRunning);
                                if !send_error(&mut out, code, None).await { panel.detach().await; return }
                            }
                        }
                        Err(_) => {
                            if !send_error(&mut out, BrowserErrorCode::BrowserBadMessage, None).await { panel.detach().await; return }
                        }
                    }
                    if panel.attached.as_ref().map(|a| a.session.clone()) != before {
                        break true;
                    }
                }
                ev = recv_event(&mut events) => {
                    match ev {
                        Some(ev) => {
                            let before = panel.attached.as_ref().map(|a| a.session.clone());
                            if !panel.on_event(&ev, &mut out).await { panel.detach().await; return }
                            if panel.attached.as_ref().map(|a| a.session.clone()) != before { break true; }
                        }
                        None => { panel.attached = None; break true; }
                    }
                }
                changed = recv_tabs(&mut tabs) => {
                    if changed && !send_json(&mut out, &BrowserServerMessage::State(panel.state())).await { panel.detach().await; return }
                }
                notice = recv_notice(&mut notices) => {
                    if let Some(n) = notice
                        && !send_error(&mut out, n.code, Some(n.message)).await { panel.detach().await; return }
                }
                _ = wait_exit(&inst) => {
                    panel.attached = None;
                    let _ = send_json(&mut out, &BrowserServerMessage::State(panel.state())).await;
                    break true;
                }
            }
        };
        if !reattach {
            return;
        }
    }
}

async fn recv_event(events: &mut Option<broadcast::Receiver<CdpEvent>>) -> Option<CdpEvent> {
    match events {
        Some(rx) => loop {
            match rx.recv().await {
                Ok(ev) => return Some(ev),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        },
        None => std::future::pending().await,
    }
}

async fn recv_tabs(tabs: &mut Option<tokio::sync::watch::Receiver<Vec<crate::instance::TabRecord>>>) -> bool {
    match tabs {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

async fn recv_notice(
    notices: &mut Option<broadcast::Receiver<crate::instance::InstanceNotice>>,
) -> Option<crate::instance::InstanceNotice> {
    match notices {
        Some(rx) => rx.recv().await.ok(),
        None => std::future::pending().await,
    }
}

async fn wait_exit(inst: &Option<Arc<ChromeInstance>>) {
    match inst {
        Some(i) => i.exited().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_and_wheel_map_to_cdp() {
        let down = mouse_params(&BrowserMouseInput {
            event: BrowserMouseEventKind::Down,
            x: 10.0,
            y: 20.5,
            button: BrowserMouseButton::Left,
            click_count: 1,
            modifiers: 8,
            delta_x: None,
            delta_y: None,
        });
        assert_eq!(
            down,
            json!({"type": "mousePressed", "x": 10.0, "y": 20.5, "button": "left", "clickCount": 1, "modifiers": 8})
        );
        let wheel = mouse_params(&BrowserMouseInput {
            event: BrowserMouseEventKind::Wheel,
            x: 1.0,
            y: 2.0,
            button: BrowserMouseButton::None,
            click_count: 0,
            modifiers: 0,
            delta_x: None,
            delta_y: Some(-120.0),
        });
        assert_eq!(wheel["type"], "mouseWheel");
        assert_eq!(wheel["deltaX"], 0.0);
        assert_eq!(wheel["deltaY"], -120.0);
    }

    #[test]
    fn keys_type_text_only_when_they_carry_it() {
        let k = |event, text: Option<&str>| BrowserKeyInput {
            event,
            key: "a".into(),
            code: "KeyA".into(),
            text: text.map(str::to_owned),
            key_code: 65,
            modifiers: 0,
        };
        assert_eq!(key_params(&k(BrowserKeyEventKind::Down, Some("a")))["type"], "keyDown");
        assert_eq!(key_params(&k(BrowserKeyEventKind::Down, Some("a")))["text"], "a");
        assert_eq!(key_params(&k(BrowserKeyEventKind::Down, None))["type"], "rawKeyDown");
        assert_eq!(key_params(&k(BrowserKeyEventKind::Up, None))["type"], "keyUp");
        assert_eq!(
            key_params(&k(BrowserKeyEventKind::Char, Some("a")))["windowsVirtualKeyCode"],
            65
        );
    }

    #[test]
    fn address_bar_input_becomes_a_url() {
        assert_eq!(normalize_url("discord.com").as_deref(), Some("https://discord.com"));
        assert_eq!(normalize_url(" https://a.b/c ").as_deref(), Some("https://a.b/c"));
        assert_eq!(normalize_url("about:blank").as_deref(), Some("about:blank"));
        assert_eq!(
            normalize_url("file:///etc/passwd").as_deref(),
            Some("file:///etc/passwd")
        );
        assert_eq!(normalize_url("chrome://settings").as_deref(), Some("chrome://settings"));
        assert_eq!(
            normalize_url("localhost:3000").as_deref(),
            Some("https://localhost:3000")
        );
        assert_eq!(normalize_url(""), None);
    }

    #[test]
    fn frame_layout_is_len_meta_jpeg() {
        let meta = BrowserFrameMeta {
            seq: 1,
            tab_id: "T".into(),
            width: 800.0,
            height: 600.0,
            device_scale_factor: 2.0,
            page_scale_factor: 1.0,
            offset_top: 0.0,
            scroll_x: None,
            scroll_y: Some(10.0),
        };
        let bytes = encode_frame(&meta, &[0xFF, 0xD8, 0xFF]);
        let len = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let parsed: BrowserFrameMeta = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
        assert_eq!(parsed, meta);
        assert_eq!(&bytes[4 + len..], &[0xFF, 0xD8, 0xFF]);
    }
}
