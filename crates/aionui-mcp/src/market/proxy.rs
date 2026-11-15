//! Filtering stdio proxy between an agent CLI and a market MCP server
//! (R1 #8). MCP's stdio transport is newline-delimited JSON-RPC (one message
//! per line; spec 2025-06-18 "Transports"). The proxy:
//! - remembers the ids of client `tools/list` requests and removes the tools
//!   the instance policy does not permit from the matching responses;
//! - answers a client `tools/call` for a tool outside the policy itself with
//!   a JSON-RPC error and never forwards it;
//! - re-checks the launch (status, assignment) before tool calls and on a
//!   timer, and stops when it no longer holds (revoke / unassign);
//! - forwards every other line byte-for-byte; lines that are not JSON are
//!   dropped (they cannot be valid MCP messages, and a filter must not guess).

use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Which tools of an instance may be listed and called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicy {
    /// `Some` = allowlist (read tier, fail closed); `None` = all but `block`.
    allow: Option<BTreeSet<String>>,
    block: BTreeSet<String>,
}

impl ToolPolicy {
    /// Read tier: allowlist (a missing list allows nothing). Write tier: blocklist.
    pub fn for_tier(tier: &str, allow: Option<&[String]>, block: &[String]) -> Self {
        let allow = if tier == "write" {
            None
        } else {
            Some(allow.unwrap_or_default().iter().cloned().collect())
        };
        Self {
            allow,
            block: block.iter().cloned().collect(),
        }
    }

    pub fn permits(&self, tool: &str) -> bool {
        !self.block.contains(tool) && self.allow.as_ref().is_none_or(|a| a.contains(tool))
    }
}

/// Why the proxy stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyExit {
    /// The client closed stdin.
    ClientClosed,
    /// The server closed stdout (it exited).
    ServerClosed,
    /// The launch is no longer allowed (revoked, dormant, unassigned, deleted).
    Revoked,
}

/// Re-check cache: a tool call within this window of a passed check skips the DB.
const GATE_CACHE: Duration = Duration::from_secs(2);

enum ClientAction {
    Forward,
    Rewrite(Value),
    Drop,
}

struct State<G> {
    policy: ToolPolicy,
    pending_lists: HashSet<String>,
    gate: G,
    last_ok: Option<Instant>,
    dropped: u64,
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn blocked_reply(id: &Value, tool: &str, reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32602, "message": format!("tool '{tool}' {reason}") }
    })
}

impl<G, F> State<G>
where
    G: Fn() -> F,
    F: Future<Output = bool>,
{
    async fn gate_ok(&mut self) -> bool {
        if self.last_ok.is_some_and(|t| t.elapsed() < GATE_CACHE) {
            return true;
        }
        let ok = (self.gate)().await;
        self.last_ok = ok.then(Instant::now);
        ok
    }

    /// One client message. `replies` collects answers the proxy sends itself.
    async fn client_message(&mut self, msg: &Value, replies: &mut Vec<Value>, revoked: &mut bool) -> bool {
        let Some(obj) = msg.as_object() else {
            return true;
        };
        let method = obj.get("method").and_then(Value::as_str);
        match (method, obj.get("id")) {
            (Some("tools/list"), Some(id)) => {
                self.pending_lists.insert(id_key(id));
                true
            }
            (Some("tools/call"), Some(id)) => {
                let tool = obj
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !self.policy.permits(tool) {
                    replies.push(blocked_reply(
                        id,
                        tool,
                        "is not available through this MCP market instance",
                    ));
                    return false;
                }
                if !self.gate_ok().await {
                    replies.push(blocked_reply(
                        id,
                        tool,
                        "is no longer allowed: the instance was revoked or unassigned",
                    ));
                    *revoked = true;
                    return false;
                }
                true
            }
            // A tools/call without an id is malformed: never forward it.
            (Some("tools/call"), None) => false,
            _ => true,
        }
    }

    async fn client_line(&mut self, line: &str, replies: &mut Vec<Value>, revoked: &mut bool) -> ClientAction {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.dropped += 1;
            return ClientAction::Drop;
        };
        match value {
            Value::Array(items) => {
                let mut kept = Vec::with_capacity(items.len());
                for item in items.iter() {
                    if self.client_message(item, replies, revoked).await {
                        kept.push(item.clone());
                    }
                }
                if kept.len() == items.len() {
                    ClientAction::Forward
                } else if kept.is_empty() {
                    ClientAction::Drop
                } else {
                    ClientAction::Rewrite(Value::Array(kept))
                }
            }
            other => {
                if self.client_message(&other, replies, revoked).await {
                    ClientAction::Forward
                } else {
                    ClientAction::Drop
                }
            }
        }
    }

    /// Filter one server message in place; returns whether it changed.
    fn server_message(&mut self, msg: &mut Value) -> bool {
        let Some(obj) = msg.as_object_mut() else {
            return false;
        };
        if obj.contains_key("method") {
            return false;
        }
        let Some(id) = obj.get("id") else {
            return false;
        };
        if !self.pending_lists.remove(&id_key(id)) {
            return false;
        }
        let Some(tools) = obj
            .get_mut("result")
            .and_then(|r| r.get_mut("tools"))
            .and_then(Value::as_array_mut)
        else {
            return false;
        };
        let before = tools.len();
        tools.retain(|t| {
            t.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| self.policy.permits(n))
        });
        tools.len() != before
    }

    fn server_line(&mut self, line: &str) -> Option<Option<String>> {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            self.dropped += 1;
            return None;
        };
        let changed = match &mut value {
            Value::Array(items) => {
                let mut any = false;
                for item in items.iter_mut() {
                    any |= self.server_message(item);
                }
                any
            }
            other => self.server_message(other),
        };
        Some(changed.then(|| value.to_string()))
    }
}

async fn write_line<W: AsyncWrite + Unpin>(w: &mut W, line: &str) -> std::io::Result<()> {
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Run the proxy until one side closes or the launch is revoked.
///
/// `gate` re-checks the launch; it runs before tool calls (cached for 2 s) and
/// every `recheck_every`.
pub async fn run_stdio_proxy<CR, CW, SR, SW, G, F>(
    client_in: CR,
    mut client_out: CW,
    server_out: SR,
    server_in: SW,
    policy: ToolPolicy,
    gate: G,
    recheck_every: Duration,
) -> std::io::Result<ProxyExit>
where
    CR: AsyncRead + Unpin,
    CW: AsyncWrite + Unpin,
    SR: AsyncRead + Unpin,
    SW: AsyncWrite + Unpin,
    G: Fn() -> F,
    F: Future<Output = bool>,
{
    let mut client_lines = BufReader::new(client_in).lines();
    let mut server_lines = BufReader::new(server_out).lines();
    let mut server_in = server_in;
    let mut state = State {
        policy,
        pending_lists: HashSet::new(),
        gate,
        last_ok: None,
        dropped: 0,
    };
    let mut ticker = tokio::time::interval(recheck_every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    let exit = loop {
        tokio::select! {
            line = client_lines.next_line() => {
                let Some(line) = line? else { break ProxyExit::ClientClosed };
                if line.trim().is_empty() {
                    continue;
                }
                let mut replies = Vec::new();
                let mut revoked = false;
                match state.client_line(&line, &mut replies, &mut revoked).await {
                    ClientAction::Forward => write_line(&mut server_in, &line).await?,
                    ClientAction::Rewrite(v) => write_line(&mut server_in, &v.to_string()).await?,
                    ClientAction::Drop => {}
                }
                for reply in replies {
                    write_line(&mut client_out, &reply.to_string()).await?;
                }
                if revoked {
                    break ProxyExit::Revoked;
                }
            }
            line = server_lines.next_line() => {
                let Some(line) = line? else { break ProxyExit::ServerClosed };
                if line.trim().is_empty() {
                    continue;
                }
                match state.server_line(&line) {
                    Some(None) => write_line(&mut client_out, &line).await?,
                    Some(Some(rewritten)) => write_line(&mut client_out, &rewritten).await?,
                    None => {}
                }
            }
            _ = ticker.tick() => {
                state.last_ok = None;
                if !state.gate_ok().await {
                    break ProxyExit::Revoked;
                }
            }
        }
    };
    if state.dropped > 0 {
        tracing::warn!(dropped = state.dropped, "mcp market proxy: dropped non-JSON lines");
    }
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

    fn policy() -> ToolPolicy {
        ToolPolicy::for_tier(
            "read",
            Some(&["echo".to_owned(), "list".to_owned()][..]),
            &["list".to_owned()],
        )
    }

    #[test]
    fn policies() {
        let p = policy();
        assert!(p.permits("echo"));
        assert!(!p.permits("list"), "block wins over allow");
        assert!(!p.permits("forbidden"));
        assert!(
            !ToolPolicy::for_tier("read", None, &[]).permits("echo"),
            "read without allowlist = nothing"
        );
        let w = ToolPolicy::for_tier("write", Some(&["x".to_owned()][..]), &["drop".to_owned()]);
        assert!(w.permits("anything") && !w.permits("drop"));
    }

    /// Wire a proxy to in-memory pipes; returns (client writer, client reader,
    /// server reader, server writer, proxy handle).
    #[tokio::test]
    async fn filters_list_and_blocks_calls() {
        let (mut client_w, proxy_client_in) = duplex(64 * 1024);
        let (proxy_client_out, client_r) = duplex(64 * 1024);
        let (proxy_server_in, server_r) = duplex(64 * 1024);
        let (mut server_w, proxy_server_out) = duplex(64 * 1024);
        let allowed = Arc::new(AtomicBool::new(true));
        let gate_flag = allowed.clone();
        let proxy = tokio::spawn(run_stdio_proxy(
            proxy_client_in,
            proxy_client_out,
            proxy_server_out,
            proxy_server_in,
            policy(),
            move || {
                let flag = gate_flag.clone();
                async move { flag.load(Ordering::SeqCst) }
            },
            Duration::from_secs(3600),
        ));
        let mut client_lines = BufReader::new(client_r).lines();
        let mut server_lines = BufReader::new(server_r).lines();

        // tools/list round trip: filtered.
        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        let fwd = server_lines.next_line().await.unwrap().unwrap();
        assert_eq!(
            fwd, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}",
            "forwarded byte-for-byte"
        );
        server_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"echo\"},{\"name\":\"list\"},{\"name\":\"forbidden\"}]}}\n")
            .await
            .unwrap();
        let resp: Value = serde_json::from_str(&client_lines.next_line().await.unwrap().unwrap()).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["echo"]);

        // Blocked call: answered by the proxy, never forwarded.
        client_w
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"forbidden\"}}\n",
            )
            .await
            .unwrap();
        let resp: Value = serde_json::from_str(&client_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(resp["id"], 2);
        assert_eq!(resp["error"]["code"], -32602);

        // Allowed call: forwarded (and the blocked one above was not).
        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"echo\"}}\n")
            .await
            .unwrap();
        let fwd: Value = serde_json::from_str(&server_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(fwd["id"], 3);

        // A response to a non-list id passes unchanged even if it has `tools`.
        server_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"tools\":[{\"name\":\"forbidden\"}]}}\n")
            .await
            .unwrap();
        let resp: Value = serde_json::from_str(&client_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(resp["result"]["tools"][0]["name"], "forbidden");

        // Batches are filtered element-wise.
        client_w
            .write_all(b"[{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"list\"}},{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}]\n")
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&client_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reply["id"], 4);
        let fwd: Value = serde_json::from_str(&server_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(fwd.as_array().unwrap().len(), 1);

        // Revoke: the next allowed call is refused and the proxy stops.
        allowed.store(false, Ordering::SeqCst);
        tokio::time::sleep(GATE_CACHE + Duration::from_millis(100)).await;
        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"echo\"}}\n")
            .await
            .unwrap();
        let resp: Value = serde_json::from_str(&client_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(resp["id"], 5);
        assert!(resp["error"]["message"].as_str().unwrap().contains("revoked"));
        assert_eq!(proxy.await.unwrap().unwrap(), ProxyExit::Revoked);
    }

    #[tokio::test]
    async fn periodic_recheck_stops_a_revoked_proxy() {
        let (_client_w, proxy_client_in) = duplex(1024);
        let (proxy_client_out, _client_r) = duplex(1024);
        let (proxy_server_in, _server_r) = duplex(1024);
        let (_server_w, proxy_server_out) = duplex(1024);
        let exit = run_stdio_proxy(
            proxy_client_in,
            proxy_client_out,
            proxy_server_out,
            proxy_server_in,
            policy(),
            || async { false },
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(exit, ProxyExit::Revoked);
    }
}
