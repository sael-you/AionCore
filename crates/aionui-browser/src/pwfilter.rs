//! Playwright-protocol filter for Juggler engines (Camoufox, C35 v2 option A,
//! TASK-136 K6).
//!
//! Camoufox is driven by Playwright over its Juggler patch, so agents attach
//! with `playwright-cli attach --endpoint` to a Playwright server (the
//! sidecar), not to CDP. That gives the whole Playwright API, so the bridge
//! filters the Playwright wire protocol to keep Chromium's guarantees:
//!
//! - no cookie or storage access: cookies/addCookies/clearCookies,
//!   storageState, Page.webStorage*, HAR, tracing; and `Cookie`/`Set-Cookie`
//!   headers are stripped from every frame in both directions
//!   ([`strip_cookie_headers`]);
//! - no new contexts or browsers outside the instance's profile:
//!   `Browser.newContext*`, every `BrowserType` call, `Browser.startServer`;
//! - no requests made by the sidecar's own process (they would bypass the
//!   egress proxy): `Playwright.newRequest`, every `APIRequestContext` call;
//! - no server-side files: `Artifact.saveAs/pathAfterFinished`,
//!   `setInputFiles` with `localPaths`/`localDirectory`, temp files, HAR;
//! - every URL an agent call names (`Frame.goto`, `Route.continue`,
//!   `Route.redirectNavigationRequest`, `addScriptTag/addStyleTag`) is
//!   `http(s)` and allowed by the [`NavigationGuard`] (`goto` may also open
//!   `about:blank`): no `file:`, `about:config`, `view-source:`. The egress
//!   proxy refuses internal addresses for everything the page itself loads;
//! - at most `max_tabs` pages; `Browser.close` detaches instead of closing.
//!
//! Default deny: an allow-list of object TYPES (learned from the server's
//! `__create__` messages) and, for Page, Frame, Route, BrowserContext,
//! Request, Response and Artifact, an allow-list of METHODS; anything else
//! is refused. Refusals are answered in
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

/// Types whose calls may be forwarded. Page, Frame, Route, BrowserContext,
/// Request, Response and Artifact are further limited to the METHOD
/// allow-lists below (default deny: a method a future Playwright adds is
/// refused until it is listed here).
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
    "Disposable",
];

// Method allow-lists, from playwright-core 1.62's protocol validators
// (`scheme.<Type><Method>Params` in coreBundle.js), minus what reads or
// writes cookies/storage, records to server files or leaves the profile.

/// Not listed: webStorage* (localStorage/sessionStorage dumps), screencast*
/// (server-side recording), setDockTile, requestGC.
const PAGE_ALLOWED: &[&str] = &[
    "addInitScript",
    "bringToFront",
    "cancelPickLocator",
    "clearConsoleMessages",
    "clearPageErrors",
    "close",
    "consoleMessages",
    "emulateMedia",
    "expectScreenshot",
    "exposeBinding",
    "goBack",
    "goForward",
    "hideHighlight",
    "keyboardDown",
    "keyboardInsertText",
    "keyboardPress",
    "keyboardType",
    "keyboardUp",
    "mouseClick",
    "mouseDown",
    "mouseMove",
    "mouseUp",
    "mouseWheel",
    "pageErrors",
    "pdf",
    "pickLocator",
    "registerLocatorHandler",
    "reload",
    "requests",
    "resolveLocatorHandlerNoReply",
    "runBeforeUnload",
    "screenshot",
    "setExtraHTTPHeaders",
    "setNetworkInterceptionPatterns",
    "setViewportSize",
    "setWebSocketInterceptionPatterns",
    "startCSSCoverage",
    "startJSCoverage",
    "stopCSSCoverage",
    "stopJSCoverage",
    "touchscreenTap",
    "unregisterLocatorHandler",
    "updateSubscription",
];

const FRAME_ALLOWED: &[&str] = &[
    "addScriptTag",
    "addStyleTag",
    "ariaSnapshot",
    "blur",
    "check",
    "click",
    "content",
    "dblclick",
    "dispatchEvent",
    "dragAndDrop",
    "drop",
    "evalOnSelector",
    "evalOnSelectorAll",
    "evaluateExpression",
    "evaluateExpressionHandle",
    "expect",
    "fill",
    "focus",
    "frameElement",
    "getAttribute",
    "goto",
    "hideHighlight",
    "highlight",
    "hover",
    "innerHTML",
    "innerText",
    "inputValue",
    "isChecked",
    "isDisabled",
    "isEditable",
    "isEnabled",
    "isHidden",
    "isVisible",
    "press",
    "queryCount",
    "querySelector",
    "querySelectorAll",
    "resolveSelector",
    "selectOption",
    "setContent",
    "setInputFiles",
    "tap",
    "textContent",
    "title",
    "type",
    "uncheck",
    "waitForFunction",
    "waitForSelector",
    "waitForTimeout",
];

const ROUTE_ALLOWED: &[&str] = &["abort", "continue", "fulfill", "redirectNavigationRequest"];

/// Not listed: cookies/addCookies/clearCookies, storageState/setStorageState,
/// close, newCDPSession, pause, recorder, exposeConsoleApi, HAR replay,
/// credentials*, createTempFiles (server-side upload files).
const CONTEXT_ALLOWED: &[&str] = &[
    "addInitScript",
    "clearPermissions",
    "clockFastForward",
    "clockInstall",
    "clockPauseAt",
    "clockResume",
    "clockRunFor",
    "clockSetFixedTime",
    "clockSetSystemTime",
    "exposeBinding",
    "grantPermissions",
    "newPage",
    "registerSelectorEngine",
    "setExtraHTTPHeaders",
    "setGeolocation",
    "setHTTPCredentials",
    "setNetworkInterceptionPatterns",
    "setOffline",
    "setTestIdAttributeName",
    "setWebSocketInterceptionPatterns",
    "updateSubscription",
];

/// Raw header reads are allowed: the bridge strips cookie headers from every
/// frame ([`strip_cookie_headers`]).
const REQUEST_ALLOWED: &[&str] = &["rawRequestHeaders", "response"];
const RESPONSE_ALLOWED: &[&str] = &[
    "body",
    "httpVersion",
    "rawResponseHeaders",
    "securityDetails",
    "serverAddr",
    "sizes",
];
/// Not listed: saveAs, pathAfterFinished (server paths).
const ARTIFACT_ALLOWED: &[&str] = &["cancel", "delete", "failure", "saveAsStream", "stream"];

/// `Browser`: only `close` (answered as a detach).
const BROWSER_ALLOWED: &[&str] = &["close"];

/// The method allow-list of a type, when it has one.
fn method_allow_list(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "Page" => PAGE_ALLOWED,
        "Frame" => FRAME_ALLOWED,
        "Route" => ROUTE_ALLOWED,
        "BrowserContext" => CONTEXT_ALLOWED,
        "Request" => REQUEST_ALLOWED,
        "Response" => RESPONSE_ALLOWED,
        "Artifact" => ARTIFACT_ALLOWED,
        _ => return None,
    })
}

/// Header names never shown to or taken from agents.
const COOKIE_HEADERS: &[&str] = &["cookie", "set-cookie", "cookie2", "set-cookie2"];

fn is_cookie_header(name: &str) -> bool {
    COOKIE_HEADERS.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// Remove cookie headers from every headers list or map anywhere in a frame
/// (either direction): arrays of `{name, value}` pairs (request/response
/// initializers, `rawRequestHeaders`/`rawResponseHeaders` results,
/// `Route.fulfill`/`continue` and `setExtraHTTPHeaders` params) and objects
/// under a `headers` key. Returns how many were removed.
pub fn strip_cookie_headers(value: &mut Value) -> usize {
    match value {
        Value::Array(items) => {
            let before = items.len();
            items.retain(|item| {
                !(item.get("value").is_some() && item.get("name").and_then(Value::as_str).is_some_and(is_cookie_header))
            });
            let mut removed = before - items.len();
            for item in items {
                removed += strip_cookie_headers(item);
            }
            removed
        }
        Value::Object(map) => {
            let mut removed = 0;
            if let Some(Value::Object(headers)) = map.get_mut("headers") {
                let names: Vec<String> = headers.keys().filter(|k| is_cookie_header(k)).cloned().collect();
                for name in names {
                    headers.remove(&name);
                    removed += 1;
                }
            }
            for child in map.values_mut() {
                removed += strip_cookie_headers(child);
            }
            removed
        }
        _ => 0,
    }
}

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
        if let Some(allowed) = method_allow_list(kind)
            && !allowed.contains(&method)
        {
            return Verdict::Refuse(format!("{kind}.{method} is not available on AionUi's server browser"));
        }
        // Every URL-carrying call (Frame.goto, Frame.addScriptTag/addStyleTag,
        // Route.continue, Route.redirectNavigationRequest, and whatever a
        // listed method carries as `url`) passes the same guard: http(s)
        // only, never AionUi, loopback or a private network. Only `goto`
        // may also open about:blank.
        if let Some(url) = params.get("url").and_then(Value::as_str) {
            let verdict = url_verdict(url, guard, kind == "Frame" && method == "goto");
            if verdict != Verdict::Allow {
                return verdict;
            }
        }
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
            "BrowserContext" if method == "newPage" && self.pages >= self.max_tabs => {
                Verdict::Refuse(format!("tab limit reached ({} tabs)", self.max_tabs))
            }
            "Frame" | "ElementHandle" if method == "setInputFiles" => {
                if !params["localPaths"].is_null() || !params["localDirectory"].is_null() {
                    Verdict::Refuse("setInputFiles with server paths is not available: send the files".into())
                } else {
                    Verdict::Allow
                }
            }
            _ => Verdict::Allow,
        }
    }
}

/// A URL an agent call may name: `http(s)` allowed by the guard;
/// `about:blank` only where `allow_blank` (a navigation).
pub fn url_verdict(raw: &str, guard: &NavigationGuard, allow_blank: bool) -> Verdict {
    let Ok(url) = Url::parse(raw.trim()) else {
        return Verdict::Refuse("not a URL".into());
    };
    match url.scheme() {
        "about" if allow_blank && url.as_str() == "about:blank" => Verdict::Allow,
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
            ("route@1", "Route"),
            ("request@1", "Request"),
            ("response@1", "Response"),
            ("ws-stream@1", "WritableStream"),
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
            ("context@1", "setNetworkInterceptionPatterns"),
            ("artifact@1", "saveAsStream"),
            ("route@1", "fulfill"),
            ("response@1", "rawResponseHeaders"),
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

    // Review part 2, blocker 1: each of these opened a local or internal URL
    // (or dumped storage) while only Frame.goto was checked.
    #[test]
    fn every_url_carrying_call_passes_the_guard() {
        let f = setup();
        let g = guard();
        let bad_urls = [
            "file:///root/.clauterm/aionui-browser/u/1/cookies.sqlite",
            "about:config",
            "view-source:https://example.com",
            "chrome://browser/content/browser.xhtml",
            "resource://gre/",
            "http://127.0.0.1:25809/api/conversations",
            "http://[::1]:8080/",
            "http://192.168.1.1/",
            "https://aionui.example/",
            "about:blank",
        ];
        for (guid, method) in [
            ("route@1", "redirectNavigationRequest"),
            ("route@1", "continue"),
            ("frame@1", "addScriptTag"),
            ("frame@1", "addStyleTag"),
        ] {
            for url in bad_urls {
                assert!(
                    refused(f.on_client_message(&call(guid, method, json!({"url": url})), &g)),
                    "{guid}.{method} {url}"
                );
            }
            assert_eq!(
                f.on_client_message(&call(guid, method, json!({"url": "https://example.com/a.js"})), &g),
                Verdict::Allow,
                "{guid}.{method} public"
            );
        }
        // No url = continue unchanged.
        assert_eq!(
            f.on_client_message(&call("route@1", "continue", json!({})), &g),
            Verdict::Allow
        );
    }

    #[test]
    fn methods_are_default_deny_per_type() {
        let f = setup();
        let g = guard();
        for (guid, method) in [
            ("page@1", "webStorageItems"),
            ("page@1", "webStorageGetItem"),
            ("page@1", "webStorageSetItem"),
            ("page@1", "webStorageClear"),
            ("page@1", "screencastStart"),
            ("page@1", "someMethodAFuturePlaywrightAdds"),
            ("frame@1", "someNewFrameMethod"),
            ("route@1", "someNewRouteMethod"),
            ("context@1", "createTempFiles"),
            ("context@1", "someNewContextMethod"),
            ("request@1", "someNewRequestMethod"),
            ("response@1", "someNewResponseMethod"),
            ("artifact@1", "pathAfterFinished"),
            ("ws-stream@1", "write"),
        ] {
            assert!(
                refused(f.on_client_message(&call(guid, method, json!({})), &g)),
                "{guid}.{method}"
            );
        }
    }

    // Review part 2, blocker 2: Set-Cookie (HttpOnly too) reached the agent
    // in response headers; Cookie could be set through headers params.
    #[test]
    fn cookie_headers_are_stripped_from_every_headers_list_and_map() {
        let mut created = json!({"guid": "page@1", "method": "__create__", "params": {"type": "Response", "guid": "response@9",
            "initializer": {"url": "https://example.com/", "status": 200, "headers": [
                {"name": "content-type", "value": "text/html"},
                {"name": "Set-Cookie", "value": "sid=secret; HttpOnly"},
                {"name": "set-cookie", "value": "b=2"}]}}});
        assert_eq!(strip_cookie_headers(&mut created), 2);
        assert_eq!(
            created["params"]["initializer"]["headers"],
            json!([{"name": "content-type", "value": "text/html"}])
        );

        let mut raw = json!({"id": 4, "result": {"headers": [
            {"name": "COOKIE", "value": "sid=secret"}, {"name": "accept", "value": "*/*"}]}});
        assert_eq!(strip_cookie_headers(&mut raw), 1);
        assert_eq!(raw["result"]["headers"], json!([{"name": "accept", "value": "*/*"}]));

        let mut fulfill = call(
            "route@1",
            "fulfill",
            json!({"status": 200, "headers": [
            {"name": "set-cookie", "value": "planted=1"}, {"name": "x-a", "value": "b"}]}),
        );
        assert_eq!(strip_cookie_headers(&mut fulfill), 1);
        let mut map = json!({"params": {"headers": {"Cookie": "a=b", "X-Ok": "1"}, "nested": [{"headers": {"set-cookie": "x"}}]}});
        assert_eq!(strip_cookie_headers(&mut map), 2);
        assert_eq!(map["params"]["headers"], json!({"X-Ok": "1"}));

        let mut clean =
            json!({"id": 1, "result": {"value": "a cookie recipe", "headers": [{"name": "etag", "value": "x"}]}});
        assert_eq!(strip_cookie_headers(&mut clean), 0);
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
