//! The only CDP channel to a Chrome instance (train-8 review): Chrome runs
//! with `--remote-debugging-pipe`, so there is NO debugging TCP port; AionCore
//! reads fd 4 / writes fd 3 (NUL-delimited JSON, verified: Chrome's
//! `--remote-debugging-pipe` contract as used by Playwright/Puppeteer
//! `PipeTransport`) and multiplexes:
//!
//! - the ROOT client (AionCore: guard, tabs, panel sessions) uses ids below
//!   [`AGENT_ID_BASE`] and receives every frame no agent owns;
//! - each agent LINK (one per bridge connection) gets its own flat session as
//!   its default: `Target.attachToBrowserTarget` for a browser connection,
//!   `Target.attachToTarget{flatten}` for a page connection. Session-less
//!   agent frames are sent in that session and the session id is stripped
//!   from what comes back, so agents see an ordinary connection and never
//!   share DevTools client state (auto-attach, discovery, domains) with
//!   AionCore. Agent ids are rewritten into their own range and restored.
//!
//! A link may only address sessions it owns (its default session, sessions it
//! attached, children auto-attached in them) — including as the `sessionId`
//! param of `Target.detachFromTarget` — so an agent can neither hijack nor
//! detach AionCore's sessions.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::cdp::CdpClient;
use crate::error::BrowserError;

/// Agent message ids start here (the root client counts up from 1). Chrome
/// parses a CDP id as a 32-bit int: agent ids stay in `[2^30, 2^31)`.
pub const AGENT_ID_BASE: u64 = 1 << 30;
const AGENT_ID_SPAN: u64 = (1 << 31) - 1 - AGENT_ID_BASE;

struct Pending {
    link: u64,
    original_id: Value,
}

struct Link {
    tx: mpsc::UnboundedSender<String>,
    default_session: String,
}

#[derive(Default)]
struct Routes {
    pending: HashMap<u64, Pending>,
    links: HashMap<u64, Link>,
    /// sessionId → link that owns it.
    owner: HashMap<String, u64>,
}

struct Inner {
    write: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    next_link: AtomicU64,
    routes: std::sync::Mutex<Routes>,
}

/// The multiplexer of one Chrome instance. Cheap to clone.
#[derive(Clone)]
pub struct CdpMux {
    inner: Arc<Inner>,
    root: CdpClient,
}

/// What an agent link is bound to.
#[derive(Debug, Clone)]
pub enum LinkTarget {
    Browser,
    Page(String),
}

/// One agent connection over the mux.
pub struct AgentLink {
    id: u64,
    mux: CdpMux,
    pub incoming: mpsc::UnboundedReceiver<String>,
}

impl CdpMux {
    /// Over Chrome's pipe: `to_chrome` = our end of Chrome's fd 3,
    /// `from_chrome` = our end of Chrome's fd 4.
    pub fn over_pipe(to_chrome: OwnedFd, from_chrome: OwnedFd) -> std::io::Result<Self> {
        let mut writer = tokio::net::unix::pipe::Sender::from_owned_fd(to_chrome)?;
        let mut reader = tokio::net::unix::pipe::Receiver::from_owned_fd(from_chrome)?;
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
        let (frames_tx, frames_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(frame) = write_rx.recv().await {
                let mut bytes = frame.into_bytes();
                bytes.push(0);
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                while let Some(pos) = buf.iter().position(|b| *b == 0) {
                    let frame: Vec<u8> = buf.drain(..=pos).take(pos).collect();
                    if frames_tx.send(String::from_utf8_lossy(&frame).into_owned()).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Self::over_channels(write_tx, frames_rx))
    }

    /// The router over any frame transport (the pipe, or tests).
    pub fn over_channels(write: mpsc::UnboundedSender<String>, mut frames: mpsc::UnboundedReceiver<String>) -> Self {
        let (root_in_tx, root_in_rx) = mpsc::unbounded_channel::<String>();
        let root = CdpClient::from_channels(write.clone(), root_in_rx);
        let inner = Arc::new(Inner {
            write,
            next_id: AtomicU64::new(0),
            next_link: AtomicU64::new(1),
            routes: std::sync::Mutex::new(Routes::default()),
        });
        let router = inner.clone();
        tokio::spawn(async move {
            while let Some(text) = frames.recv().await {
                match route(&router, &text) {
                    Some((link, out)) => {
                        let routes = router.routes.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(l) = routes.links.get(&link) {
                            let _ = l.tx.send(out);
                        }
                    }
                    None => {
                        if root_in_tx.send(text).is_err() {
                            break;
                        }
                    }
                }
            }
            // Chrome is gone: close every link.
            router.routes.lock().unwrap_or_else(|e| e.into_inner()).links.clear();
        });
        Self { inner, root }
    }

    /// AionCore's own client.
    pub fn root(&self) -> &CdpClient {
        &self.root
    }

    /// Open an agent link with its own default session.
    pub async fn open_link(&self, target: LinkTarget) -> Result<AgentLink, BrowserError> {
        let session = match &target {
            LinkTarget::Browser => self.root.call("Target.attachToBrowserTarget", json!({}), None).await?,
            LinkTarget::Page(id) => {
                self.root
                    .call("Target.attachToTarget", json!({"targetId": id, "flatten": true}), None)
                    .await?
            }
        }["sessionId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| BrowserError::Cdp("no session for the agent link".into()))?;
        let id = self.inner.next_link.fetch_add(1, Ordering::Relaxed);
        let (tx, incoming) = mpsc::unbounded_channel();
        {
            let mut routes = self.inner.routes.lock().unwrap_or_else(|e| e.into_inner());
            routes.owner.insert(session.clone(), id);
            routes.links.insert(
                id,
                Link {
                    tx,
                    default_session: session,
                },
            );
        }
        Ok(AgentLink {
            id,
            mux: self.clone(),
            incoming,
        })
    }
}

/// Decide where a frame from Chrome goes: `Some((link, frame for the agent))`
/// or `None` = the root client.
fn route(inner: &Inner, text: &str) -> Option<(u64, String)> {
    let mut frame: Value = serde_json::from_str(text).ok()?;
    let mut routes = inner.routes.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = frame.get("id").and_then(Value::as_u64) {
        if id < AGENT_ID_BASE {
            return None;
        }
        let pending = routes.pending.remove(&id)?;
        let default = routes.links.get(&pending.link)?.default_session.clone();
        // A session the agent opened (attachToTarget / attachToBrowserTarget).
        if let Some(sid) = frame["result"]["sessionId"].as_str() {
            routes.owner.insert(sid.to_owned(), pending.link);
        }
        frame["id"] = pending.original_id;
        if frame.get("sessionId").and_then(Value::as_str) == Some(default.as_str())
            && let Some(obj) = frame.as_object_mut()
        {
            obj.remove("sessionId");
        }
        return Some((pending.link, frame.to_string()));
    }
    let sid = frame.get("sessionId").and_then(Value::as_str)?.to_owned();
    let link = *routes.owner.get(&sid)?;
    let method = frame.get("method").and_then(Value::as_str).unwrap_or_default();
    let child = frame["params"]["sessionId"].as_str().map(str::to_owned);
    match (method, child) {
        ("Target.attachedToTarget", Some(c)) => {
            routes.owner.insert(c, link);
        }
        ("Target.detachedFromTarget", Some(c)) => {
            routes.owner.remove(&c);
        }
        _ => {}
    }
    let default = routes.links.get(&link)?.default_session.clone();
    if sid == default
        && let Some(obj) = frame.as_object_mut()
    {
        obj.remove("sessionId");
    }
    Some((link, frame.to_string()))
}

impl AgentLink {
    /// Send one (already filtered) agent frame. Refuses a frame that names a
    /// session this link does not own.
    pub fn send(&self, text: &str) -> Result<(), BrowserError> {
        let mut frame: Value = serde_json::from_str(text).map_err(|_| BrowserError::Cdp("not a CDP message".into()))?;
        let inner = &self.mux.inner;
        let mut routes = inner.routes.lock().unwrap_or_else(|e| e.into_inner());
        let default = routes
            .links
            .get(&self.id)
            .map(|l| l.default_session.clone())
            .ok_or_else(|| BrowserError::Cdp("the browser is gone".into()))?;
        let owns = |routes: &Routes, s: &str| routes.owner.get(s) == Some(&self.id);
        let session = frame.get("sessionId").and_then(Value::as_str).map(str::to_owned);
        match session {
            Some(s) if !owns(&routes, &s) => return Err(BrowserError::Cdp("unknown session".into())),
            Some(_) => {}
            None => frame["sessionId"] = json!(default),
        }
        let target_session = frame["params"]["sessionId"].as_str().map(str::to_owned);
        if let Some(t) = target_session
            && !owns(&routes, &t)
        {
            return Err(BrowserError::Cdp("unknown session".into()));
        }
        let original_id = frame.get("id").cloned().unwrap_or(Value::Null);
        let id = AGENT_ID_BASE + inner.next_id.fetch_add(1, Ordering::Relaxed) % AGENT_ID_SPAN;
        frame["id"] = json!(id);
        routes.pending.insert(
            id,
            Pending {
                link: self.id,
                original_id,
            },
        );
        drop(routes);
        inner
            .write
            .send(frame.to_string())
            .map_err(|_| BrowserError::Cdp("the browser is gone".into()))
    }
}

impl Drop for AgentLink {
    fn drop(&mut self) {
        let inner = &self.mux.inner;
        let mut routes = inner.routes.lock().unwrap_or_else(|e| e.into_inner());
        routes.links.remove(&self.id);
        routes.pending.retain(|_, p| p.link != self.id);
        let owned: Vec<String> = routes
            .owner
            .iter()
            .filter(|(_, l)| **l == self.id)
            .map(|(s, _)| s.clone())
            .collect();
        routes.owner.retain(|_, l| *l != self.id);
        drop(routes);
        for session in owned {
            self.mux
                .root
                .notify("Target.detachFromTarget", json!({"sessionId": session}), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake Chrome on channels: answers attach calls with fixed sessions,
    /// echoes everything else, and lets the test inject events.
    fn fake() -> (CdpMux, mpsc::UnboundedReceiver<String>, mpsc::UnboundedSender<String>) {
        let (to_chrome_tx, to_chrome_rx) = mpsc::unbounded_channel();
        let (from_chrome_tx, from_chrome_rx) = mpsc::unbounded_channel();
        (
            CdpMux::over_channels(to_chrome_tx, from_chrome_rx),
            to_chrome_rx,
            from_chrome_tx,
        )
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<String>) -> Value {
        let t = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&t).unwrap()
    }

    #[tokio::test]
    async fn links_get_their_own_session_ids_and_routing() {
        let (mux, mut chrome_rx, chrome_tx) = fake();
        let open = tokio::spawn({
            let mux = mux.clone();
            async move { mux.open_link(LinkTarget::Browser).await.unwrap() }
        });
        let attach = next(&mut chrome_rx).await;
        assert_eq!(attach["method"], "Target.attachToBrowserTarget");
        assert!(attach["id"].as_u64().unwrap() < AGENT_ID_BASE, "the root asks");
        chrome_tx
            .send(json!({"id": attach["id"], "result": {"sessionId": "B1"}}).to_string())
            .unwrap();
        let mut link = open.await.unwrap();

        // A session-less agent frame goes out in its browser session, with a
        // rewritten id; the reply comes back with the agent's id, no session.
        link.send(r#"{"id":7,"method":"Target.getTargets"}"#).unwrap();
        let out = next(&mut chrome_rx).await;
        assert_eq!(out["sessionId"], "B1");
        assert!(out["id"].as_u64().unwrap() >= AGENT_ID_BASE);
        chrome_tx
            .send(json!({"id": out["id"], "sessionId": "B1", "result": {"targetInfos": []}}).to_string())
            .unwrap();
        let reply = next(&mut link.incoming).await;
        assert_eq!(reply["id"], 7);
        assert!(reply.get("sessionId").is_none());

        // The agent attaches to a page: it owns the child session, its events route to it.
        link.send(r#"{"id":8,"method":"Target.attachToTarget","params":{"targetId":"T","flatten":true}}"#)
            .unwrap();
        let out = next(&mut chrome_rx).await;
        chrome_tx
            .send(json!({"id": out["id"], "sessionId": "B1", "result": {"sessionId": "C1"}}).to_string())
            .unwrap();
        assert_eq!(next(&mut link.incoming).await["result"]["sessionId"], "C1");
        chrome_tx
            .send(json!({"method": "Page.loadEventFired", "sessionId": "C1", "params": {}}).to_string())
            .unwrap();
        let ev = next(&mut link.incoming).await;
        assert_eq!(ev["sessionId"], "C1");
        link.send(r#"{"id":9,"method":"Page.reload","sessionId":"C1"}"#)
            .unwrap();
        assert_eq!(next(&mut chrome_rx).await["sessionId"], "C1");

        // Root-owned sessions are off limits, also as a detach parameter.
        assert!(
            link.send(r#"{"id":10,"method":"Page.reload","sessionId":"ROOT-PANEL"}"#)
                .is_err()
        );
        assert!(
            link.send(r#"{"id":11,"method":"Target.detachFromTarget","params":{"sessionId":"ROOT-PANEL"}}"#)
                .is_err()
        );

        // Events of sessions nobody owns go to the root client, not to agents.
        let mut root_events = mux.root().subscribe();
        chrome_tx
            .send(json!({"method": "Fetch.requestPaused", "params": {"requestId": "r"}}).to_string())
            .unwrap();
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), root_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.method, "Fetch.requestPaused");
        assert!(
            link.incoming.try_recv().is_err(),
            "the agent never sees AionCore's events"
        );

        // Dropping the link detaches its sessions.
        drop(link);
        let mut detached = Vec::new();
        for _ in 0..2 {
            let f = next(&mut chrome_rx).await;
            assert_eq!(f["method"], "Target.detachFromTarget");
            detached.push(f["params"]["sessionId"].as_str().unwrap().to_owned());
        }
        detached.sort();
        assert_eq!(detached, vec!["B1", "C1"]);
    }
}
