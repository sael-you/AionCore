//! Playwright-protocol filter for Juggler engines (Camoufox, C35 v2 option A,
//! TASK-136 K6).
//!
//! Camoufox is driven by Playwright over its Juggler patch, so agents attach
//! with `playwright-cli attach --endpoint` to a Playwright server (the
//! sidecar), not to CDP. That gives the whole Playwright API, so the bridge
//! filters the Playwright wire protocol to keep Chromium's guarantees:
//!
//! - no cookie or storage access: `BrowserContext.cookies/addCookies/
//!   clearCookies/storageState/setStorageState`, HAR, tracing;
//! - no new contexts or browsers outside the instance's profile:
//!   `Browser.newContext*`, every `BrowserType` call, `Browser.startServer`;
//! - no requests made by the sidecar's own process (they would bypass the
//!   egress proxy): `Playwright.newRequest`, every `APIRequestContext` call;
//! - no server-side files: `Artifact.saveAs/pathAfterFinished`,
//!   `setInputFiles` with `localPaths`/`localDirectory`, HAR replay;
//! - `goto` only to `http(s)` (and `about:blank`) URLs the
//!   [`NavigationGuard`] allows — the egress proxy refuses internal
//!   addresses for everything else the page loads;
//! - at most `max_tabs` pages; `Browser.close` detaches instead of closing.
//!
//! The filter is an allow-list of object TYPES (learned from the server's
//! `__create__` messages) with method deny-lists inside the allowed types;
//! a call on an unknown or denied type is refused. Refusals are answered in
//! Playwright's own error format, so the client sees a normal error.
//!
//! Pinned client/server: `@playwright/cli` 0.1.17 (playwright-core
//! `1.62.0-alpha-1783623505000`); Camoufox pins playwright `<1.63` because
//! every Playwright minor may change Juggler.

use std::collections::HashMap;

use serde_json::{Value, json};
use url::Url;

use crate::guard::{GuardDecision, NavigationGuard};

/// What to do with one client → server message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Forward it unchanged.
    Allow,
    /// Do not forward; answer the caller with this error message.
    Refuse(String),
    /// `Browser.close`: answer success and drop the agent's connection (the
    /// instance keeps running for the owner).
    Detach,
}

/// Types whose calls may be forwarded (methods still checked below).
const ALLOWED_TYPES: &[&str] = &[
    "Root",
    "Browser",
    "BrowserContext",
    "Page",
    "Frame",
    "ElementHandle",
    "JSHandle",
    "Worker",
    "Request",
    "Response",
    "Route",
    "WebSocket",
    "WebSocketRoute",
    "Dialog",
    "BindingCall",
    "Artifact",
    "Stream",
    "WritableStream",
    "Disposable",
];

/// `Browser`: only `close` (answered as a detach).
const BROWSER_ALLOWED: &[&str] = &["close"];

const CONTEXT_DENIED: &[&str] = &[
    "cookies",
    "addCookies",
    "clearCookies",
    "storageState",
    "setStorageState",
    "close",
    "newCDPSession",
    "pause",
    "enableRecorder",
    "disableRecorder",
    "exposeConsoleApi",
    "routeAPIRequestsFromHar",
    "unrouteAPIRequestsFromHar",
    "credentialsCreate",
    "credentialsDelete",
    "credentialsGet",
    "credentialsInstall",
    "harStart",
    "harExport",
];

const ARTIFACT_DENIED: &[&str] = &["saveAs", "pathAfterFinished"];

/// Per-connection filter state.
#[derive(Debug, Default)]
pub struct PwFilter {
    /// guid → protocol type, from the server's `__create__` messages.
    types: HashMap<String, String>,
    /// Open pages (for `max_tabs`).
    pages: usize,
    max_tabs: usize,
}

fn method_name(msg: &Value) -> &str {
    msg.get("method").and_then(Value::as_str).unwrap_or("")
}

impl PwFilter {
    pub fn new(max_tabs: u8) -> Self {
        Self {
            max_tabs: usize::from(max_tabs.max(1)),
            ..Default::default()
        }
    }

    /// Learn object types from a server → client message.
    pub fn on_server_message(&mut self, msg: &Value) {
        match method_name(msg) {
            "__create__" => {
                let params = &msg["params"];
                if let (Some(guid), Some(kind)) = (params["guid"].as_str(), params["type"].as_str()) {
                    if kind == "Page" {
                        self.pages += 1;
                    }
                    self.types.insert(guid.to_owned(), kind.to_owned());
                }
            }
            "__dispose__" => {
                if let Some(guid) = msg["guid"].as_str()
                    && let Some(kind) = self.types.remove(guid)
                    && kind == "Page"
                {
                    self.pages = self.pages.saturating_sub(1);
                }
            }
            _ => {}
        }
    }

    /// Open pages the filter knows of.
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// Decide a client → server message.
    pub fn on_client_message(&self, msg: &Value, guard: &NavigationGuard) -> Verdict {
        let guid = msg["guid"].as_str().unwrap_or("");
        let method = method_name(msg);
        if guid.is_empty() {
            // The connection's root: only the handshake.
            return if method == "initialize" {
                Verdict::Allow
            } else {
                Verdict::Refuse(format!("{method} is not available"))
            };
        }
        // Waits and aborts carry no action of their own.
        if method == "__waitInfo__" || method == "__abort__" {
            return Verdict::Allow;
        }
        let Some(kind) = self.types.get(guid) else {
            return Verdict::Refuse("unknown object".into());
        };
        if !ALLOWED_TYPES.contains(&kind.as_str()) {
            return Verdict::Refuse(format!("{kind} is not available on AionUi's server browser"));
        }
        let params = &msg["params"];
        match kind.as_str() {
            "Browser" => {
                if method == "close" {
                    Verdict::Detach
                } else if BROWSER_ALLOWED.contains(&method) {
                    Verdict::Allow
                } else {
                    Verdict::Refuse(format!(
                        "Browser.{method} is not available: use the browser's existing context (contexts()[0])"
                    ))
                }
            }
            "BrowserContext" => {
                if CONTEXT_DENIED.contains(&method) {
                    return Verdict::Refuse(format!(
                        "BrowserContext.{method} is not available (no cookie, storage, recorder or HAR access)"
                    ));
                }
                if method == "newPage" && self.pages >= self.max_tabs {
                    return Verdict::Refuse(format!("tab limit reached ({} tabs)", self.max_tabs));
                }
                Verdict::Allow
            }
            "Artifact" if ARTIFACT_DENIED.contains(&method) => Verdict::Refuse(format!(
                "Artifact.{method} is not available: stream the download instead"
            )),
            "Frame" | "ElementHandle" if method == "setInputFiles" => {
                if !params["localPaths"].is_null() || !params["localDirectory"].is_null() {
                    Verdict::Refuse("setInputFiles with server paths is not available: send the files".into())
                } else {
                    Verdict::Allow
                }
            }
            "Frame" if method == "goto" => goto_verdict(params["url"].as_str().unwrap_or(""), guard),
            _ => Verdict::Allow,
        }
    }
}

/// `goto` may open `http(s)` URLs the guard allows, and `about:blank`.
pub fn goto_verdict(raw: &str, guard: &NavigationGuard) -> Verdict {
    let Ok(url) = Url::parse(raw.trim()) else {
        return Verdict::Refuse("not a URL".into());
    };
    match url.scheme() {
        "about" if url.as_str() == "about:blank" => Verdict::Allow,
        "http" | "https" => match guard.decide(url.as_str()) {
            GuardDecision::Allow => Verdict::Allow,
            GuardDecision::Block => {
                Verdict::Refuse("this address is blocked (AionUi itself, loopback or a private network)".into())
            }
        },
        other => Verdict::Refuse(format!("{other}: URLs are not available")),
    }
}

/// The error reply Playwright's client expects for a refused call.
pub fn refusal(id: &Value, message: &str) -> Value {
    json!({
        "id": id,
        "error": { "error": {
            "message": format!("AionUi refused: {message}"),
            "name": "Error",
            "stack": "",
        }},
    })
}

/// The success reply for a call answered locally (`Browser.close` detach).
pub fn empty_result(id: &Value) -> Value {
    json!({ "id": id, "result": {} })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> NavigationGuard {
        NavigationGuard::new(["aionui.example".to_owned()], Vec::<String>::new())
    }

    fn created(f: &mut PwFilter, guid: &str, kind: &str) {
        f.on_server_message(&json!({"guid": "parent", "method": "__create__",
            "params": {"type": kind, "guid": guid, "initializer": {}}}));
    }

    fn call(guid: &str, method: &str, params: Value) -> Value {
        json!({"id": 7, "guid": guid, "method": method, "params": params, "metadata": {}})
    }

    fn setup() -> PwFilter {
        let mut f = PwFilter::new(2);
        for (guid, kind) in [
            ("browser@1", "Browser"),
            ("context@1", "BrowserContext"),
            ("page@1", "Page"),
            ("frame@1", "Frame"),
            ("handle@1", "ElementHandle"),
            ("request-context@1", "APIRequestContext"),
            ("browser-type@1", "BrowserType"),
            ("artifact@1", "Artifact"),
            ("tracing@1", "Tracing"),
            ("playwright", "Playwright"),
            ("local-utils@1", "LocalUtils"),
        ] {
            created(&mut f, guid, kind);
        }
        f
    }

    fn refused(v: Verdict) -> bool {
        matches!(v, Verdict::Refuse(_))
    }

    #[test]
    fn cookies_storage_and_context_creation_are_refused() {
        let f = setup();
        let g = guard();
        for m in [
            "cookies",
            "addCookies",
            "clearCookies",
            "storageState",
            "setStorageState",
            "harStart",
            "close",
        ] {
            assert!(
                refused(f.on_client_message(&call("context@1", m, json!({})), &g)),
                "{m}"
            );
        }
        for m in [
            "newContext",
            "newContextForReuse",
            "startTracing",
            "startServer",
            "newBrowserCDPSession",
        ] {
            assert!(
                refused(f.on_client_message(&call("browser@1", m, json!({})), &g)),
                "{m}"
            );
        }
        assert_eq!(
            f.on_client_message(&call("browser@1", "close", json!({})), &g),
            Verdict::Detach
        );
    }

    #[test]
    fn whole_types_that_bypass_the_browser_are_refused() {
        let f = setup();
        let g = guard();
        for (guid, m) in [
            ("request-context@1", "fetch"),
            ("browser-type@1", "launch"),
            ("browser-type@1", "launchPersistentContext"),
            ("tracing@1", "tracingStart"),
            ("playwright", "newRequest"),
            ("local-utils@1", "harOpen"),
            ("unknown@9", "anything"),
        ] {
            assert!(
                refused(f.on_client_message(&call(guid, m, json!({})), &g)),
                "{guid}.{m}"
            );
        }
        assert!(refused(f.on_client_message(&call("", "something", json!({})), &g)));
        assert_eq!(
            f.on_client_message(&call("", "initialize", json!({"sdkLanguage": "javascript"})), &g),
            Verdict::Allow
        );
    }

    #[test]
    fn page_work_is_allowed() {
        let f = setup();
        let g = guard();
        for (guid, m) in [
            ("frame@1", "click"),
            ("frame@1", "evaluateExpression"),
            ("frame@1", "ariaSnapshot"),
            ("page@1", "screenshot"),
            ("handle@1", "fill"),
            ("context@1", "newPage"),
            ("context@1", "route"),
            ("artifact@1", "saveAsStream"),
        ] {
            assert_eq!(
                f.on_client_message(&call(guid, m, json!({})), &g),
                Verdict::Allow,
                "{guid}.{m}"
            );
        }
        assert!(refused(f.on_client_message(
            &call("artifact@1", "saveAs", json!({"path": "/x"})),
            &g
        )));
    }

    #[test]
    fn goto_only_to_public_web_urls() {
        let f = setup();
        let g = guard();
        let goto = |url: &str| f.on_client_message(&call("frame@1", "goto", json!({"url": url})), &g);
        assert_eq!(goto("https://example.com/"), Verdict::Allow);
        assert_eq!(goto("about:blank"), Verdict::Allow);
        for bad in [
            "http://127.0.0.1:25809/",
            "http://localhost:8080/api",
            "http://10.0.0.5/",
            "http://169.254.169.254/latest/meta-data",
            "https://aionui.example/settings",
            "file:///etc/passwd",
            "about:config",
            "view-source:https://example.com",
            "javascript:alert(1)",
            "data:text/html,hi",
            "not a url",
        ] {
            assert!(refused(goto(bad)), "{bad}");
        }
    }

    #[test]
    fn server_paths_in_uploads_are_refused() {
        let f = setup();
        let g = guard();
        let up = |p: Value| f.on_client_message(&call("handle@1", "setInputFiles", p), &g);
        assert!(refused(up(json!({"localPaths": ["/root/.ssh/id_rsa"]}))));
        assert!(refused(up(json!({"localDirectory": "/etc"}))));
        assert_eq!(
            up(json!({"payloads": [{"name": "a.txt", "buffer": "aGk="}]})),
            Verdict::Allow
        );
    }

    #[test]
    fn tab_limit_follows_created_and_disposed_pages() {
        let mut f = setup();
        let g = guard();
        assert_eq!(f.pages(), 1);
        created(&mut f, "page@2", "Page");
        assert!(refused(
            f.on_client_message(&call("context@1", "newPage", json!({})), &g)
        ));
        f.on_server_message(&json!({"guid": "page@2", "method": "__dispose__", "params": {}}));
        assert_eq!(f.pages(), 1);
        assert_eq!(
            f.on_client_message(&call("context@1", "newPage", json!({})), &g),
            Verdict::Allow
        );
    }

    #[test]
    fn refusal_uses_playwrights_error_shape() {
        let r = refusal(&json!(7), "nope");
        assert_eq!(r["id"], 7);
        assert_eq!(r["error"]["error"]["message"], "AionUi refused: nope");
        assert_eq!(empty_result(&json!(3)), json!({"id": 3, "result": {}}));
    }
}
