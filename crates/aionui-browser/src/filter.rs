//! What an agent may send through the CDP bridge (C30, owner change
//! 2026-11-16): full control of pages and tabs. Refused are only the cookie /
//! storage read-write APIs (they expose or change account sessions without
//! limiting interaction), browser lifecycle and context management, and the
//! few methods that would open a channel around this filter. `createTarget`
//! needs a free tab slot (`max_tabs`).
//!
//! Train-7 review: the filter judges method AND params (a non-flat session
//! or `sendMessageToTarget` would tunnel any inner method past it), all of
//! `Fetch.*` belongs to AionCore's navigation guard, and [`sanitize`] strips
//! cookies and cookie headers from everything Chrome sends back to an agent
//! (`Network.enable` events expose them, HttpOnly included).

use aionui_api_types::BrowserErrorCode;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterDecision {
    Forward,
    /// `Target.createTarget`: forward only while the instance has fewer than
    /// `max_tabs` pages, else `BROWSER_TAB_LIMIT`.
    NeedsTabSlot,
    Refuse(BrowserErrorCode),
}

/// Cookie and storage APIs: never through the bridge.
const STORAGE_METHODS: &[&str] = &[
    "Network.getCookies",
    "Network.getAllCookies",
    "Network.setCookie",
    "Network.setCookies",
    "Network.deleteCookies",
    "Network.clearBrowserCookies",
    "Storage.getCookies",
    "Storage.setCookies",
    "Storage.clearCookies",
    "Storage.clearDataForOrigin",
    "Storage.clearDataForStorageKey",
    "Storage.getSharedStorageEntries",
    "Storage.getInterestGroupDetails",
    "DOMStorage.getDOMStorageItems",
    "DOMStorage.setDOMStorageItem",
    "DOMStorage.removeDOMStorageItem",
    "DOMStorage.clear",
    "IndexedDB.requestData",
    "IndexedDB.requestDatabase",
    "IndexedDB.getMetadata",
    "IndexedDB.clearObjectStore",
    "IndexedDB.deleteDatabase",
    "CacheStorage.requestEntries",
    "CacheStorage.requestCachedResponse",
    "Browser.setDownloadBehavior",
    "Page.setDownloadBehavior",
];

/// Browser lifecycle, profile contexts and side channels around the filter.
const LIFECYCLE_METHODS: &[&str] = &[
    "Browser.close",
    "Browser.crash",
    "Browser.crashGpuProcess",
    "Target.createBrowserContext",
    "Target.disposeBrowserContext",
    // Page JS would get a raw CDP binding that bypasses this filter.
    "Target.exposeDevToolsProtocol",
    // Port forwarding from the browser to this host.
    "Tethering.bind",
    // Carries an arbitrary inner CDP message past this filter.
    "Target.sendMessageToTarget",
];

/// Methods that open a session; only flat sessions (`flatten: true`) pass,
/// so every later message of that session goes through this filter too.
const SESSION_METHODS: &[&str] = &["Target.attachToTarget", "Target.setAutoAttach"];

pub fn decide(method: &str, params: &Value) -> FilterDecision {
    if method == "Target.createTarget" {
        return FilterDecision::NeedsTabSlot;
    }
    let blocked = STORAGE_METHODS.contains(&method)
        || LIFECYCLE_METHODS.contains(&method)
        // The navigation guard owns request interception.
        || method.starts_with("Fetch.")
        || (SESSION_METHODS.contains(&method) && params.get("flatten").and_then(Value::as_bool) != Some(true));
    if blocked {
        return FilterDecision::Refuse(BrowserErrorCode::BrowserMethodBlocked);
    }
    FilterDecision::Forward
}

/// Keys removed wherever they appear in a frame to an agent.
const COOKIE_KEYS: &[&str] = &[
    "associatedCookies",
    "blockedCookies",
    "exemptedCookies",
    "cookies",
    "headersText",
    "requestHeadersText",
];

fn is_cookie_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("cookie") || name.eq_ignore_ascii_case("set-cookie")
}

/// Header containers come as a map (`{"Cookie": "…"}`) or as a list of
/// `{name, value}` (`Fetch.requestPaused.responseHeaders`).
fn strip_cookie_headers(value: &mut Value) {
    match value {
        Value::Object(map) => map.retain(|k, _| !is_cookie_header(k)),
        Value::Array(list) => {
            list.retain(|entry| !entry.get("name").and_then(Value::as_str).is_some_and(is_cookie_header))
        }
        _ => {}
    }
}

fn sanitize_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|k, _| !COOKIE_KEYS.contains(&k.as_str()));
            for (key, child) in map.iter_mut() {
                if key.ends_with("eaders") {
                    // headers, requestHeaders, responseHeaders, extraHTTPHeaders, …
                    strip_cookie_headers(child);
                }
                sanitize_value(child);
            }
        }
        Value::Array(list) => list.iter_mut().for_each(sanitize_value),
        _ => {}
    }
}

/// A Chrome → agent text frame with every cookie and cookie header removed.
/// Frames that cannot carry one are returned untouched (cheap substring test).
pub fn sanitize(text: &str) -> std::borrow::Cow<'_, str> {
    let lower_hit = text.contains("ookie") || text.contains("OOKIE") || text.contains("eadersText");
    if !lower_hit {
        return std::borrow::Cow::Borrowed(text);
    }
    match serde_json::from_str::<Value>(text) {
        Ok(mut frame) => {
            sanitize_value(&mut frame);
            std::borrow::Cow::Owned(frame.to_string())
        }
        // Not JSON: nothing an agent could parse anyway; drop it.
        Err(_) => std::borrow::Cow::Owned(String::new()),
    }
}

/// Constant-time token comparison (no early exit on the first difference).
/// Shared rule with the desktop bridge (`tokensMatch` in cdpTargetProtocol.ts).
pub fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0 && !a.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn bridge_filter_fixture_cases() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/bridge_filter.cases.json")).expect("fixture parses");
        let cases = fixture["cases"].as_array().expect("cases");
        assert!(cases.len() >= 10);
        for case in cases {
            let method = case["method"].as_str().unwrap();
            let got = match decide(method, &case["params"]) {
                FilterDecision::Forward => "forward".to_owned(),
                FilterDecision::NeedsTabSlot => "tab_slot".to_owned(),
                FilterDecision::Refuse(code) => serde_json::to_value(code).unwrap().as_str().unwrap().to_owned(),
            };
            assert_eq!(got, case["expect"].as_str().unwrap(), "{method}");
        }
    }

    #[test]
    fn bridge_sanitize_fixture_cases() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/bridge_filter.cases.json")).expect("fixture parses");
        let cases = fixture["sanitize"].as_array().expect("sanitize");
        assert!(cases.len() >= 5);
        for case in cases {
            let input = case["frame"].to_string();
            let out: Value = serde_json::from_str(&sanitize(&input)).unwrap();
            assert_eq!(out, case["expect"], "{}", case["name"]);
            let text = out.to_string().to_ascii_lowercase();
            assert!(
                !text.contains("secret-session"),
                "{}: no cookie value survives",
                case["name"]
            );
        }
        let plain = r#"{"id":1,"result":{"frameId":"F"}}"#;
        assert!(
            matches!(sanitize(plain), std::borrow::Cow::Borrowed(_)),
            "untouched fast path"
        );
    }

    /// The `tokens` cases of the shared desktop/server fixture (byte-identical
    /// copy of ui `tests/fixtures/cdpTargetProtocol.cases.json`).
    #[test]
    fn shared_fixture_token_cases() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/cdpTargetProtocol.cases.json"))
            .expect("shared fixture parses");
        let cases = fixture["tokens"].as_array().expect("tokens");
        assert!(!cases.is_empty());
        for case in cases {
            let (a, b) = (case["a"].as_str().unwrap(), case["b"].as_str().unwrap());
            assert_eq!(tokens_match(a, b), case["match"].as_bool().unwrap(), "{a} vs {b}");
        }
        assert!(!tokens_match("", ""), "an empty token never matches");
    }
}
