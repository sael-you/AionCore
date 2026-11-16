//! What an agent may send through the CDP bridge (C30, owner change
//! 2026-11-16): full control of pages and tabs. Refused are only the cookie /
//! storage read-write APIs (they expose or change account sessions without
//! limiting interaction), browser lifecycle and context management, and the
//! few methods that would open a channel around this filter. `createTarget`
//! needs a free tab slot (`max_tabs`).

use aionui_api_types::BrowserErrorCode;

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
];

pub fn decide(method: &str) -> FilterDecision {
    if method == "Target.createTarget" {
        return FilterDecision::NeedsTabSlot;
    }
    if STORAGE_METHODS.contains(&method) || LIFECYCLE_METHODS.contains(&method) {
        return FilterDecision::Refuse(BrowserErrorCode::BrowserMethodBlocked);
    }
    FilterDecision::Forward
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
            let got = match decide(method) {
                FilterDecision::Forward => "forward".to_owned(),
                FilterDecision::NeedsTabSlot => "tab_slot".to_owned(),
                FilterDecision::Refuse(code) => serde_json::to_value(code).unwrap().as_str().unwrap().to_owned(),
            };
            assert_eq!(got, case["expect"].as_str().unwrap(), "{method}");
        }
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
