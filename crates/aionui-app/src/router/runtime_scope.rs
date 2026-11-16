//! Route scope of the agent runtime token (card C13).
//!
//! Agent subprocesses receive `AIONUI_RUNTIME_TOKEN`. On the ordinary user
//! routers (`auth_middleware`) that token used to act with the owner's full
//! scope. It now reaches only the routes the agent-facing helper CLIs call;
//! everything else answers 403 `RUNTIME_SCOPE` (see
//! `aionui_auth::IRuntimeTokenVerifier::allows_route`).
//!
//! The list is derived from the HTTP calls of the CLI commands, not guessed:
//! - `aioncore config …`   → `commands/cmd_config.rs` (`request_json` callers)
//! - `aioncore diagnose …` → `commands/cmd_diagnose.rs` (named commands; the
//!   `diagnose http get` escape hatch is limited to the same GET rules)
//! - `aioncore board|session|conversation|voice|team|skills …` → the
//!   `/api/runtime/*` routers, which validate the token themselves and are not
//!   behind `auth_middleware`; they are listed so the scope stays complete if
//!   one of them is ever mounted behind it.
//!
//! Local identity (production `--local`) is unchanged: `auth_middleware` does
//! not look at runtime tokens there.
//!
//! A new CLI call to an `auth_middleware` route must add its rule here; the
//! `runtime_scope_e2e` test checks every rule against the real router.

use axum::http::Method;

/// One allowlist entry: `methods` on the path `pattern`.
///
/// `pattern` is split on `/`; a `*` segment matches exactly one non-empty
/// path segment (an id). `subtree: false` means the path must have exactly
/// the pattern's segments; `true` also admits deeper paths.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeRouteRule {
    pub methods: &'static [Method],
    pub pattern: &'static str,
    pub subtree: bool,
}

const GET: &[Method] = &[Method::GET];
const POST: &[Method] = &[Method::POST];
const PUT: &[Method] = &[Method::PUT];
const PATCH: &[Method] = &[Method::PATCH];
const DELETE: &[Method] = &[Method::DELETE];
const GET_POST: &[Method] = &[Method::GET, Method::POST];
const GET_PATCH: &[Method] = &[Method::GET, Method::PATCH];
const GET_PUT: &[Method] = &[Method::GET, Method::PUT];
const GET_PUT_DELETE: &[Method] = &[Method::GET, Method::PUT, Method::DELETE];
const GET_POST_DELETE: &[Method] = &[Method::GET, Method::POST, Method::DELETE];
const PUT_DELETE: &[Method] = &[Method::PUT, Method::DELETE];
const ANY: &[Method] = &[Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE];

const fn exact(methods: &'static [Method], pattern: &'static str) -> RuntimeRouteRule {
    RuntimeRouteRule {
        methods,
        pattern,
        subtree: false,
    }
}

const fn subtree(methods: &'static [Method], pattern: &'static str) -> RuntimeRouteRule {
    RuntimeRouteRule {
        methods,
        pattern,
        subtree: true,
    }
}

/// The routes a conversation runtime token may call through `auth_middleware`.
pub(crate) const RUNTIME_TOKEN_ROUTE_ALLOWLIST: &[RuntimeRouteRule] = &[
    // Agent routers (board, session-messages, conversations/create, voice,
    // team-tools, skills): self-authenticating, outside auth_middleware.
    subtree(ANY, "/api/runtime"),
    // config context / conversation rename; diagnose conversations
    exact(GET, "/api/conversations"),
    exact(GET_PATCH, "/api/conversations/*"),
    exact(GET, "/api/conversations/*/messages"),
    // diagnose teams
    exact(GET, "/api/teams"),
    // config assistants
    exact(GET_POST, "/api/assistants"),
    exact(POST, "/api/assistants/import"),
    exact(GET_PUT_DELETE, "/api/assistants/*"),
    exact(PATCH, "/api/assistants/*/state"),
    // config assistants rule|skill
    exact(POST, "/api/skills/assistant-rule/read"),
    exact(POST, "/api/skills/assistant-rule/write"),
    exact(DELETE, "/api/skills/assistant-rule/*"),
    exact(POST, "/api/skills/assistant-skill/read"),
    exact(POST, "/api/skills/assistant-skill/write"),
    exact(DELETE, "/api/skills/assistant-skill/*"),
    // config skills
    exact(GET, "/api/skills"),
    exact(GET, "/api/skills/paths"),
    exact(POST, "/api/skills/info"),
    exact(POST, "/api/skills/import"),
    exact(POST, "/api/skills/scan"),
    exact(GET_POST_DELETE, "/api/skills/external-paths"),
    exact(POST, "/api/skills/market/enable"),
    exact(POST, "/api/skills/market/disable"),
    exact(DELETE, "/api/skills/*"),
    // config mcp; diagnose mcp
    exact(GET_POST, "/api/mcp/servers"),
    exact(POST, "/api/mcp/servers/import"),
    exact(GET_PUT_DELETE, "/api/mcp/servers/*"),
    exact(POST, "/api/mcp/servers/*/toggle"),
    exact(POST, "/api/mcp/test-connection"),
    exact(GET, "/api/mcp/agent-configs"),
    exact(POST, "/api/mcp/oauth/check-status"),
    exact(POST, "/api/mcp/oauth/login"),
    exact(POST, "/api/mcp/oauth/logout"),
    exact(GET, "/api/mcp/oauth/authenticated"),
    // config providers; diagnose providers
    exact(GET_POST, "/api/providers"),
    exact(POST, "/api/providers/detect-protocol"),
    exact(POST, "/api/providers/fetch-models"),
    exact(PUT_DELETE, "/api/providers/*"),
    exact(POST, "/api/providers/*/models"),
    // config providers health-check; config agents
    exact(POST, "/api/agents/provider-health-check"),
    exact(GET, "/api/agents/management"),
    exact(PATCH, "/api/agents/*/enabled"),
    exact(GET_PUT, "/api/agents/*/overrides"),
    exact(POST, "/api/agents/custom"),
    exact(POST, "/api/agents/custom/try-connect"),
    exact(PUT_DELETE, "/api/agents/custom/*"),
    // config settings
    exact(GET_PATCH, "/api/settings"),
    exact(GET_PUT, "/api/settings/client"),
    // config cron jobs; diagnose cron
    exact(GET_POST, "/api/cron/jobs"),
    exact(GET_PUT_DELETE, "/api/cron/jobs/*"),
    exact(POST, "/api/cron/jobs/*/run"),
    exact(GET_POST_DELETE, "/api/cron/jobs/*/skill"),
    // config cron current
    exact(GET, "/api/internal/conversation-cron/list"),
    exact(POST, "/api/internal/conversation-cron/create"),
    exact(PUT, "/api/internal/conversation-cron/jobs/*"),
];

impl RuntimeRouteRule {
    fn matches(&self, method: &Method, path: &str) -> bool {
        if !self.methods.contains(method) {
            return false;
        }
        let mut path_segments = path.split('/');
        for expected in self.pattern.split('/') {
            match path_segments.next() {
                Some(actual) if expected == "*" && !actual.is_empty() => {}
                Some(actual) if actual == expected => {}
                _ => return false,
            }
        }
        match path_segments.next() {
            None => true,
            Some(_) => self.subtree,
        }
    }
}

/// Whether a valid runtime token may call `method path` (`path` without query).
pub(crate) fn runtime_token_allows_route(method: &Method, path: &str) -> bool {
    RUNTIME_TOKEN_ROUTE_ALLOWLIST
        .iter()
        .any(|rule| rule.matches(method, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every HTTP call the helper CLIs make through `auth_middleware`, copied
    /// from the call sites (ids replaced by samples). Keep in sync with
    /// `commands/cmd_config.rs` and `commands/cmd_diagnose.rs`.
    const CLI_CALLS: &[(&str, &str, &str)] = &[
        // cmd_config.rs
        ("config context|conversation rename", "GET", "/api/conversations/conv-1"),
        ("config conversation rename", "PATCH", "/api/conversations/conv-1"),
        ("config assistants list|create|import|delete", "GET", "/api/assistants"),
        (
            "config assistants get|update|state|delete",
            "GET",
            "/api/assistants/a-1",
        ),
        ("config assistants create", "POST", "/api/assistants"),
        ("config assistants import", "POST", "/api/assistants/import"),
        ("config assistants update", "PUT", "/api/assistants/a-1"),
        ("config assistants state", "PATCH", "/api/assistants/a-1/state"),
        ("config assistants delete", "DELETE", "/api/assistants/a-1"),
        (
            "config assistants rule read|write|delete",
            "POST",
            "/api/skills/assistant-rule/read",
        ),
        (
            "config assistants rule write",
            "POST",
            "/api/skills/assistant-rule/write",
        ),
        (
            "config assistants rule delete",
            "DELETE",
            "/api/skills/assistant-rule/a-1",
        ),
        (
            "config assistants skill read|write|delete",
            "POST",
            "/api/skills/assistant-skill/read",
        ),
        (
            "config assistants skill write",
            "POST",
            "/api/skills/assistant-skill/write",
        ),
        (
            "config assistants skill delete",
            "DELETE",
            "/api/skills/assistant-skill/a-1",
        ),
        ("config skills list|import|delete", "GET", "/api/skills"),
        ("config skills info", "POST", "/api/skills/info"),
        ("config skills paths|market", "GET", "/api/skills/paths"),
        ("config skills import", "POST", "/api/skills/import"),
        ("config skills delete", "DELETE", "/api/skills/my-skill"),
        ("config skills scan", "POST", "/api/skills/scan"),
        (
            "config skills external-paths list|add|remove",
            "GET",
            "/api/skills/external-paths",
        ),
        ("config skills external-paths add", "POST", "/api/skills/external-paths"),
        (
            "config skills external-paths remove",
            "DELETE",
            "/api/skills/external-paths",
        ),
        ("config skills market enable", "POST", "/api/skills/market/enable"),
        ("config skills market disable", "POST", "/api/skills/market/disable"),
        (
            "config mcp servers list|create|delete|import",
            "GET",
            "/api/mcp/servers",
        ),
        ("config mcp servers get|update|toggle", "GET", "/api/mcp/servers/s-1"),
        ("config mcp servers create", "POST", "/api/mcp/servers"),
        ("config mcp servers update", "PUT", "/api/mcp/servers/s-1"),
        ("config mcp servers delete", "DELETE", "/api/mcp/servers/s-1"),
        ("config mcp servers toggle", "POST", "/api/mcp/servers/s-1/toggle"),
        ("config mcp servers import", "POST", "/api/mcp/servers/import"),
        ("config mcp test-connection", "POST", "/api/mcp/test-connection"),
        ("config mcp agent-configs", "GET", "/api/mcp/agent-configs"),
        (
            "config mcp oauth check-status|login|logout",
            "POST",
            "/api/mcp/oauth/check-status",
        ),
        ("config mcp oauth login", "POST", "/api/mcp/oauth/login"),
        ("config mcp oauth logout", "POST", "/api/mcp/oauth/logout"),
        ("config mcp oauth authenticated", "GET", "/api/mcp/oauth/authenticated"),
        (
            "config providers list|create|update|delete|models",
            "GET",
            "/api/providers",
        ),
        ("config providers create", "POST", "/api/providers"),
        ("config providers update", "PUT", "/api/providers/p-1"),
        ("config providers delete", "DELETE", "/api/providers/p-1"),
        (
            "config providers detect-protocol",
            "POST",
            "/api/providers/detect-protocol",
        ),
        ("config providers fetch-models", "POST", "/api/providers/fetch-models"),
        ("config providers models fetch", "POST", "/api/providers/p-1/models"),
        (
            "config providers health-check",
            "POST",
            "/api/agents/provider-health-check",
        ),
        ("config settings get|patch", "GET", "/api/settings"),
        ("config settings patch", "PATCH", "/api/settings"),
        ("config settings client get|put", "GET", "/api/settings/client"),
        ("config settings client put", "PUT", "/api/settings/client"),
        ("config agents list|enable|custom", "GET", "/api/agents/management"),
        ("config agents enable", "PATCH", "/api/agents/ag-1/enabled"),
        ("config agents overrides get|set", "GET", "/api/agents/ag-1/overrides"),
        ("config agents overrides set", "PUT", "/api/agents/ag-1/overrides"),
        ("config agents custom create", "POST", "/api/agents/custom"),
        ("config agents custom update", "PUT", "/api/agents/custom/ag-1"),
        ("config agents custom delete", "DELETE", "/api/agents/custom/ag-1"),
        (
            "config agents custom try-connect",
            "POST",
            "/api/agents/custom/try-connect",
        ),
        ("config cron jobs list|create|delete", "GET", "/api/cron/jobs"),
        ("config cron jobs get|update", "GET", "/api/cron/jobs/j-1"),
        ("config cron jobs create", "POST", "/api/cron/jobs"),
        ("config cron jobs update", "PUT", "/api/cron/jobs/j-1"),
        ("config cron jobs delete", "DELETE", "/api/cron/jobs/j-1"),
        ("config cron jobs run", "POST", "/api/cron/jobs/j-1/run"),
        (
            "config cron jobs skill get|save|delete",
            "GET",
            "/api/cron/jobs/j-1/skill",
        ),
        ("config cron jobs skill save", "POST", "/api/cron/jobs/j-1/skill"),
        ("config cron jobs skill delete", "DELETE", "/api/cron/jobs/j-1/skill"),
        (
            "config cron current list|create|update",
            "GET",
            "/api/internal/conversation-cron/list",
        ),
        (
            "config cron current create",
            "POST",
            "/api/internal/conversation-cron/create",
        ),
        (
            "config cron current update",
            "PUT",
            "/api/internal/conversation-cron/jobs/j-1",
        ),
        // cmd_diagnose.rs
        ("diagnose summary|providers", "GET", "/api/providers"),
        ("diagnose summary|mcp", "GET", "/api/mcp/servers"),
        ("diagnose summary|cron", "GET", "/api/cron/jobs"),
        ("diagnose summary|conversations list", "GET", "/api/conversations"),
        ("diagnose conversation get", "GET", "/api/conversations/conv-1"),
        (
            "diagnose conversation messages",
            "GET",
            "/api/conversations/conv-1/messages",
        ),
        ("diagnose teams", "GET", "/api/teams"),
    ];

    #[test]
    fn every_helper_cli_call_is_inside_the_scope() {
        for (command, method, path) in CLI_CALLS {
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            assert!(
                runtime_token_allows_route(&method, path),
                "{command}: {method} {path} must be allowed"
            );
        }
    }

    /// Each rule names a real route behind `auth_middleware`: with a bogus
    /// runtime token the real router answers 401 "Invalid runtime token"
    /// (route matched, auth ran), never 404/405 (no such route or method).
    #[tokio::test]
    async fn every_rule_names_a_real_authenticated_route() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let db = aionui_db::init_database_memory().await.unwrap();
        let services = crate::AppServices::from_config(db, &crate::AppConfig::default())
            .await
            .unwrap();
        let router = crate::create_router(&services).await.expect("build router");

        for rule in RUNTIME_TOKEN_ROUTE_ALLOWLIST {
            if rule.pattern == "/api/runtime" {
                // Self-authenticating agent routers, not behind auth_middleware.
                continue;
            }
            let path = rule.pattern.replace('*', "c13-probe");
            for method in rule.methods {
                let request = Request::builder()
                    .method(method.clone())
                    .uri(&path)
                    .header("content-type", "application/json")
                    .header("x-aionui-user-id", "system_default_user")
                    .header("x-aionui-conversation-id", "conv-probe")
                    .header("x-aionui-runtime-token", "not-a-valid-token")
                    .body(Body::from("{}"))
                    .unwrap();
                let response = router.clone().oneshot(request).await.unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::UNAUTHORIZED,
                    "{method} {path}: expected auth_middleware's 401, got {}",
                    response.status()
                );
            }
        }
    }

    #[test]
    fn exact_rules_match_ids_but_not_deeper_paths() {
        assert!(runtime_token_allows_route(&Method::PATCH, "/api/conversations/conv-1"));
        assert!(!runtime_token_allows_route(
            &Method::PATCH,
            "/api/conversations/conv-1/artifacts/a1"
        ));
        assert!(!runtime_token_allows_route(
            &Method::DELETE,
            "/api/conversations/conv-1"
        ));
        assert!(!runtime_token_allows_route(
            &Method::POST,
            "/api/conversations/conv-1/messages"
        ));
        assert!(!runtime_token_allows_route(
            &Method::GET,
            "/api/conversations/conv-1/workspace"
        ));
    }

    #[test]
    fn wildcard_needs_a_non_empty_segment_and_prefixes_are_segment_aware() {
        assert!(!runtime_token_allows_route(&Method::GET, "/api/conversations/"));
        assert!(!runtime_token_allows_route(
            &Method::GET,
            "/api/conversations//messages"
        ));
        assert!(!runtime_token_allows_route(&Method::GET, "/api/settingsx"));
        assert!(!runtime_token_allows_route(&Method::GET, "/api/runtimex/board"));
        assert!(runtime_token_allows_route(&Method::POST, "/api/runtime/board/cards"));
    }

    #[test]
    fn owner_only_routes_are_outside_the_scope() {
        for (method, path) in [
            (Method::POST, "/api/auth/change-password"),
            (Method::POST, "/api/auth/logout"),
            (Method::GET, "/api/files/read"),
            (Method::POST, "/api/conversations"),
            (Method::GET, "/api/projects"),
            (Method::GET, "/api/sidebar"),
            (Method::POST, "/api/teams"),
            (Method::POST, "/api/mcp/market/install"),
            (Method::HEAD, "/api/settings"),
        ] {
            assert!(!runtime_token_allows_route(&method, path), "{method} {path}");
        }
    }
}
