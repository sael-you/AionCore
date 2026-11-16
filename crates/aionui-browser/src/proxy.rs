//! Egress proxy of one Chrome instance (C30, train-7 review blocker 3).
//!
//! Chrome runs inside this container, where AionUi answers on loopback
//! without auth in local identity. URL matching (the Fetch guard) cannot see
//! WebSocket handshakes or names that RESOLVE to loopback (`localhost`,
//! `*.localhost`, DNS rebinding). So ALL of Chrome's traffic goes through this
//! proxy (`--proxy-server=http://127.0.0.1:<port>` +
//! `--proxy-bypass-list=<-loopback>`, which removes Chrome's implicit
//! loopback bypass): it resolves the destination itself, refuses when ANY
//! resolved address is internal ([`crate::guard`]'s classifier: loopback,
//! private, link-local, CGNAT, ULA, unspecified, v4-mapped) or the name is an
//! AionUi host, and connects to the exact address it checked (no second
//! resolution, so no rebinding). HTTP (absolute-form) and `CONNECT` (TLS,
//! WebSockets) are both handled.
//!
//! Who may use it: Chrome cannot hand proxy credentials over without
//! intercepting every request, so the proxy checks the caller instead: the
//! peer socket of each accepted loopback connection is looked up in
//! `/proc/net/tcp` and must belong to the browser account's uid. Pages cannot
//! reach the proxy port at all (loopback itself goes through the proxy and is
//! refused).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::guard::{NavigationGuard, is_internal_ip};

const MAX_HEAD: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A running proxy (its port goes into Chrome's argv; it is useless to
/// anyone but the browser account).
pub struct EgressProxy {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl EgressProxy {
    /// Bind 127.0.0.1:0 and serve callers whose socket belongs to `uid`.
    pub async fn start(guard: Arc<NavigationGuard>, uid: u32) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local = listener.local_addr()?;
        let task = tokio::spawn(async move {
            loop {
                let Ok((client, peer)) = listener.accept().await else {
                    continue;
                };
                let guard = guard.clone();
                tokio::spawn(async move {
                    if peer_uid(peer, local) != Some(uid) {
                        tracing::warn!("browser egress proxy: refused a caller that is not the browser account");
                        return;
                    }
                    let _ = serve(client, &guard).await;
                });
            }
        });
        Ok(Self {
            port: local.port(),
            task,
        })
    }
}

/// `/proc/net/tcp` encoding of an IPv4 socket address (`0100007F:1F90`).
fn proc_addr(addr: SocketAddr) -> Option<String> {
    match addr.ip() {
        IpAddr::V4(v4) => Some(format!("{:08X}:{:04X}", u32::from_le_bytes(v4.octets()), addr.port())),
        IpAddr::V6(_) => None,
    }
}

/// The uid owning the client socket `peer` → `local` (from `/proc/net/tcp`).
pub(crate) fn peer_uid(peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    let table = std::fs::read_to_string("/proc/net/tcp").ok()?;
    peer_uid_in(&table, peer, local)
}

pub(crate) fn peer_uid_in(table: &str, peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    let (want_local, want_remote) = (proc_addr(peer)?, proc_addr(local)?);
    table.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        // sl local_address rem_address st tx:rx tr:tm retrnsmt uid …
        (f.len() > 7 && f[1] == want_local && f[2] == want_remote)
            .then(|| f[7].parse().ok())
            .flatten()
    })
}

/// Why a destination is refused (for the log; never the full URL).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    AionUiHost,
    Internal,
    Unresolvable,
}

/// Resolve `host:port` and return the address to connect to, or why not.
pub(crate) async fn checked_destination(guard: &NavigationGuard, host: &str, port: u16) -> Result<SocketAddr, Refusal> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if guard.is_aionui_host(bare) {
        return Err(Refusal::AionUiHost);
    }
    let addrs: Vec<SocketAddr> = match bare.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((bare, port))
            .await
            .map_err(|_| Refusal::Unresolvable)?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(Refusal::Unresolvable);
    }
    // Any internal answer refuses the name (a mixed answer is a rebinding attempt).
    if addrs.iter().any(|a| is_internal_ip(&a.ip())) {
        return Err(Refusal::Internal);
    }
    Ok(addrs[0])
}

/// `host[:port]` → (host, port) with `default` (IPv6 in brackets).
pub(crate) fn split_host_port(authority: &str, default: u16) -> Option<(String, u16)> {
    let authority = authority.trim();
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let port = match rest[end + 1..].strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default,
        };
        return Some((format!("[{host}]"), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_owned(), p.parse().ok()?)),
        _ => Some((authority.to_owned(), default)),
    }
}

async fn refuse(client: &mut TcpStream, status: &str) {
    let body = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = client.write_all(body.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// Read the request head (up to the blank line); returns (head, rest).
async fn read_head(client: &mut TcpStream) -> std::io::Result<Option<(String, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(pos + 4);
            return Ok(Some((String::from_utf8_lossy(&buf).into_owned(), rest)));
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
    }
}

async fn serve(mut client: TcpStream, guard: &NavigationGuard) -> std::io::Result<()> {
    let Some((head, rest)) = read_head(&mut client).await? else {
        return Ok(());
    };
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut parts = request_line.split_whitespace();
    let (method, target, version) = (
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or("HTTP/1.1").to_owned(),
    );

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_host_port(&target, 443) else {
            refuse(&mut client, "400 Bad Request").await;
            return Ok(());
        };
        let addr = match checked_destination(guard, &host, port).await {
            Ok(a) => a,
            Err(why) => {
                tracing::info!(?why, "browser egress proxy: refused a destination");
                refuse(&mut client, "403 Forbidden").await;
                return Ok(());
            }
        };
        let Ok(Ok(mut upstream)) = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await else {
            refuse(&mut client, "502 Bad Gateway").await;
            return Ok(());
        };
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        if !rest.is_empty() {
            upstream.write_all(&rest).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        return Ok(());
    }

    // Absolute-form HTTP request (`GET http://host:port/path HTTP/1.1`).
    let Ok(url) = url::Url::parse(&target) else {
        refuse(&mut client, "400 Bad Request").await;
        return Ok(());
    };
    if url.scheme() != "http" {
        refuse(&mut client, "400 Bad Request").await;
        return Ok(());
    }
    let Some(host) = url.host_str().map(str::to_owned) else {
        refuse(&mut client, "400 Bad Request").await;
        return Ok(());
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = match checked_destination(guard, &host, port).await {
        Ok(a) => a,
        Err(why) => {
            tracing::info!(?why, "browser egress proxy: refused a destination");
            refuse(&mut client, "403 Forbidden").await;
            return Ok(());
        }
    };
    let Ok(Ok(mut upstream)) = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await else {
        refuse(&mut client, "502 Bad Gateway").await;
        return Ok(());
    };
    // Origin-form request line; drop proxy headers; one request per connection.
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    let mut out = format!("{method} {path} {version}\r\n");
    let upgrade = head.to_ascii_lowercase().contains("\r\nupgrade:");
    for line in lines.filter(|l| !l.is_empty()) {
        let name = line.split(':').next().unwrap_or_default().trim().to_ascii_lowercase();
        if name.starts_with("proxy-") || (!upgrade && name == "connection") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !upgrade {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    upstream.write_all(out.as_bytes()).await?;
    if !rest.is_empty() {
        upstream.write_all(&rest).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_parsing() {
        assert_eq!(
            split_host_port("discord.com:443", 80),
            Some(("discord.com".into(), 443))
        );
        assert_eq!(split_host_port("discord.com", 443), Some(("discord.com".into(), 443)));
        assert_eq!(split_host_port("[::1]:8080", 80), Some(("[::1]".into(), 8080)));
        assert_eq!(split_host_port("[::1]", 443), Some(("[::1]".into(), 443)));
        assert_eq!(split_host_port("a.b:x", 80), None);
    }

    #[tokio::test]
    async fn internal_and_aionui_destinations_are_refused_after_resolution() {
        let g = NavigationGuard::from_origins(["https://y.talkseeds.com"]);
        for host in [
            "127.0.0.1",
            "localhost",
            "[::1]",
            "10.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(checked_destination(&g, host, 80).await.is_err(), "{host}");
        }
        assert_eq!(
            checked_destination(&g, "y.talkseeds.com", 443).await,
            Err(Refusal::AionUiHost)
        );
        assert_eq!(
            checked_destination(&g, "8.8.8.8", 443).await,
            Ok("8.8.8.8:443".parse().unwrap())
        );
    }

    #[test]
    fn peer_uid_is_read_from_the_tcp_table() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:C350 0100007F:1F90 01 00000000:00000000 00:00000000 00000000   999        0 1 1 0000000000000000 20 4 30 10 -1\n   1: 0100007F:1F90 0100007F:C350 01 00000000:00000000 00:00000000 00000000     0        0 2 1 0000000000000000 20 4 30 10 -1\n";
        let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(peer_uid_in(table, peer, local), Some(999));
        assert_eq!(peer_uid_in(table, local, peer), Some(0), "the server side is root's");
        assert_eq!(peer_uid_in(table, "127.0.0.1:1".parse().unwrap(), local), None);
    }

    /// A real loopback connection from this process maps to this process's uid.
    #[tokio::test]
    async fn a_live_loopback_peer_maps_to_its_owner() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let _client = TcpStream::connect(local).await.unwrap();
        let (_server, peer) = listener.accept().await.unwrap();
        assert_eq!(peer_uid(peer, local), Some(unsafe { libc::getuid() }));
    }

    /// End to end over a socket: CONNECT to an internal address gets 403, a
    /// caller with a foreign uid is dropped.
    #[tokio::test]
    async fn connect_to_loopback_is_refused_and_foreign_callers_dropped() {
        let me = unsafe { libc::getuid() };
        let proxy = EgressProxy::start(Arc::new(NavigationGuard::default()), me)
            .await
            .unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        c.write_all(b"CONNECT localhost:25809 HTTP/1.1\r\nHost: localhost:25809\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 256];
        let n = c.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 403"));

        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        c.write_all(b"GET http://127.0.0.1:1/ HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n")
            .await
            .unwrap();
        let n = c.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 403"));

        let foreign = EgressProxy::start(Arc::new(NavigationGuard::default()), me.wrapping_add(4242))
            .await
            .unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", foreign.port)).await.unwrap();
        c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
            .await
            .unwrap()
            .unwrap_or(0);
        assert_eq!(n, 0, "a caller that is not the browser account gets nothing");
    }
}
