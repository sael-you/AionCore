//! Navigation guard (C30): which URLs the server browser may load. Applied by
//! AionCore's own CDP session on every page target (`Fetch.enable` with the
//! coarse [`fetch_patterns`], then [`NavigationGuard::decide`] on each paused
//! request) and on panel navigations, so page JS or an agent's own session
//! cannot route around it.
//!
//! Blocked: the AionUi origin itself, loopback, private, link-local and CGNAT
//! addresses (IPv4 and IPv6 literals), `localhost`, and local schemes
//! (`file:`, `chrome:`, …). Hosts on the allow-list pass. Names that merely
//! RESOLVE to a private address are not resolved here (DNS rebinding is out
//! of scope for v1).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Schemes a page may never load.
const BLOCKED_SCHEMES: &[&str] = &[
    "file",
    "chrome",
    "chrome-untrusted",
    "chrome-extension",
    "chrome-search",
    "devtools",
    "view-source",
    "filesystem",
];

/// Schemes that never reach the network and are always fine.
const LOCAL_OK_SCHEMES: &[&str] = &["about", "data", "blob", "javascript"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardDecision {
    Allow,
    Block,
}

#[derive(Debug, Default)]
pub struct NavigationGuard {
    /// Hosts of the AionUi origin(s) (lower-case, no port). Grows at runtime:
    /// production runs `--local` without a configured public origin, so the
    /// panel's own `X-Forwarded-Host` is learned on every panel connection.
    aionui_hosts: std::sync::RwLock<Vec<String>>,
    /// Hosts that pass even if they would be blocked (lower-case).
    allow_hosts: Vec<String>,
}

impl Clone for NavigationGuard {
    fn clone(&self) -> Self {
        Self {
            aionui_hosts: std::sync::RwLock::new(self.aionui_hosts()),
            allow_hosts: self.allow_hosts.clone(),
        }
    }
}

fn norm_host(h: &str) -> String {
    let h = h.trim();
    // Drop a port (`host:8080`), keep IPv6 brackets intact.
    let h = match (h.rfind(':'), h.rfind(']')) {
        (Some(c), Some(b)) if c > b => &h[..c],
        (Some(c), None) if h.matches(':').count() == 1 => &h[..c],
        _ => h,
    };
    h.trim_end_matches('.').to_ascii_lowercase()
}

impl NavigationGuard {
    pub fn new(aionui_hosts: impl IntoIterator<Item = String>, allow_hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            aionui_hosts: std::sync::RwLock::new(
                aionui_hosts
                    .into_iter()
                    .map(|h| norm_host(&h))
                    .filter(|h| !h.is_empty())
                    .collect(),
            ),
            allow_hosts: allow_hosts
                .into_iter()
                .map(|h| norm_host(&h))
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }

    /// The AionUi hosts from public origins such as `https://y.talkseeds.com`.
    pub fn from_origins<'a>(origins: impl IntoIterator<Item = &'a str>) -> Self {
        let hosts = origins
            .into_iter()
            .filter_map(|o| Url::parse(o).ok())
            .filter_map(|u| u.host_str().map(str::to_owned));
        Self::new(hosts, Vec::new())
    }

    /// Record the host the owner reaches AionUi under (from the panel request).
    pub fn learn_aionui_host(&self, host: &str) {
        let host = norm_host(host);
        if host.is_empty() || host.parse::<IpAddr>().is_ok() {
            return;
        }
        if let Ok(mut hosts) = self.aionui_hosts.write()
            && !hosts.contains(&host)
        {
            tracing::info!(%host, "server browser: blocking the AionUi host");
            hosts.push(host);
        }
    }

    /// Whether `host` (any case, trailing dot, optional port) is an AionUi host.
    pub fn is_aionui_host(&self, host: &str) -> bool {
        let host = norm_host(host);
        self.aionui_hosts().iter().any(|h| *h == host)
    }

    fn aionui_hosts(&self) -> Vec<String> {
        self.aionui_hosts.read().map(|h| h.clone()).unwrap_or_default()
    }

    pub fn decide(&self, raw: &str) -> GuardDecision {
        let Ok(url) = Url::parse(raw.trim()) else {
            // Unparseable: not something Chrome would load as a URL either.
            return GuardDecision::Block;
        };
        let scheme = url.scheme().to_ascii_lowercase();
        if LOCAL_OK_SCHEMES.contains(&scheme.as_str()) {
            return GuardDecision::Allow;
        }
        if BLOCKED_SCHEMES.contains(&scheme.as_str()) {
            return GuardDecision::Block;
        }
        let Some(host) = url.host() else {
            return GuardDecision::Block;
        };
        let name = match &host {
            Host::Domain(d) => d.trim_end_matches('.').to_ascii_lowercase(),
            Host::Ipv4(ip) => ip.to_string(),
            Host::Ipv6(ip) => ip.to_string(),
        };
        if self.allow_hosts.iter().any(|h| h == &name) {
            return GuardDecision::Allow;
        }
        let blocked = match host {
            Host::Domain(_) => {
                name == "localhost"
                    || name.ends_with(".localhost")
                    || self.aionui_hosts().iter().any(|h| h == &name)
                    || name.parse::<IpAddr>().map(|ip| is_internal_ip(&ip)).unwrap_or(false)
            }
            Host::Ipv4(ip) => is_internal_ip(&IpAddr::V4(ip)),
            Host::Ipv6(ip) => is_internal_ip(&IpAddr::V6(ip)),
        };
        if blocked {
            GuardDecision::Block
        } else {
            GuardDecision::Allow
        }
    }

    /// `Fetch.enable` patterns that pause every request [`decide`](Self::decide)
    /// might block. Deliberately coarse (`*` also spans user-info and paths):
    /// a paused request that turns out to be fine is continued.
    pub fn fetch_patterns(&self) -> Vec<String> {
        let mut patterns: Vec<String> = [
            "*://*localhost*",
            "*://*127.*",
            "*://*10.*",
            "*://*172.*",
            "*://*192.168.*",
            "*://*169.254.*",
            "*://*100.*",
            "*://*0.0.0.0*",
            "*://*[*",
        ]
        .iter()
        .map(|p| (*p).to_owned())
        .collect();
        patterns.extend(self.aionui_hosts().iter().map(|h| format!("*://*{h}*")));
        patterns.extend(BLOCKED_SCHEMES.iter().map(|s| format!("{s}:*")));
        patterns
    }
}

/// Loopback, private, link-local, CGNAT, ULA, unspecified (v4, v6, mapped).
pub(crate) fn is_internal_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_internal_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_internal_v4(&v4);
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (first & 0xffc0) == 0xfe80 // link-local fe80::/10
                || *v6 == Ipv6Addr::LOCALHOST
        }
    }
}

fn is_internal_v4(ip: &Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b)) // CGNAT 100.64/10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> NavigationGuard {
        NavigationGuard::new(vec!["y.talkseeds.com".into()], vec!["10.0.0.5".into()])
    }

    #[test]
    fn internal_targets_and_local_schemes_are_blocked() {
        let g = guard();
        for url in [
            "http://127.0.0.1:25809/api/conversations",
            "http://localhost:8080/",
            "http://app.localhost/",
            "http://user:pw@127.0.0.1/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://172.31.255.255/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "https://y.talkseeds.com/",
            "https://Y.TalkSeeds.com./settings",
            "file:///etc/passwd",
            "chrome://settings",
            "chrome-extension://abc/x.html",
            "view-source:https://example.com",
            "devtools://devtools/bundled/inspector.html",
            "not a url",
        ] {
            assert_eq!(g.decide(url), GuardDecision::Block, "{url}");
        }
    }

    #[test]
    fn public_sites_local_documents_and_the_allow_list_pass() {
        let g = guard();
        for url in [
            "https://discord.com/channels/@me",
            "https://example.com/v10.1/page",
            "http://172.32.0.1/",
            "http://100.128.0.1/",
            "https://myproject-3000.y.talkseeds.com/",
            "about:blank",
            "data:text/html,<p>x</p>",
            "blob:https://discord.com/abc",
            "http://10.0.0.5/",
        ] {
            assert_eq!(g.decide(url), GuardDecision::Allow, "{url}");
        }
    }

    #[test]
    fn the_panel_host_is_learned_and_then_blocked() {
        let g = NavigationGuard::default();
        assert_eq!(g.decide("https://y.talkseeds.com/"), GuardDecision::Allow);
        g.learn_aionui_host("Y.TalkSeeds.com:443");
        g.learn_aionui_host("127.0.0.1:25809"); // IPs are covered by the IP rules
        assert_eq!(g.decide("https://y.talkseeds.com/api"), GuardDecision::Block);
        assert!(g.fetch_patterns().contains(&"*://*y.talkseeds.com*".to_owned()));
        assert_eq!(
            g.clone().decide("https://y.talkseeds.com/"),
            GuardDecision::Block,
            "clones keep it"
        );
    }

    #[test]
    fn fetch_patterns_cover_every_blocked_network_case() {
        let g = guard();
        let patterns = g.fetch_patterns();
        // A CDP urlPattern: `*` = any run, `?` = any char (no escapes needed here).
        fn matches(p: &str, s: &str) -> bool {
            let (p, s) = (p.as_bytes(), s.as_bytes());
            let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
            while si < s.len() {
                if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
                    pi += 1;
                    si += 1;
                } else if pi < p.len() && p[pi] == b'*' {
                    star = Some(pi);
                    mark = si;
                    pi += 1;
                } else if let Some(st) = star {
                    pi = st + 1;
                    mark += 1;
                    si = mark;
                } else {
                    return false;
                }
            }
            while pi < p.len() && p[pi] == b'*' {
                pi += 1;
            }
            pi == p.len()
        }
        // Chrome hands Fetch the canonical URL (2130706433 → 127.0.0.1).
        for url in [
            "http://127.0.0.1:25809/api",
            "http://user:pw@127.0.0.1/",
            "http://localhost:8080/",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "https://y.talkseeds.com/",
            "file:///etc/passwd",
        ] {
            assert!(patterns.iter().any(|p| matches(p, url)), "no pattern pauses {url}");
        }
    }
}
