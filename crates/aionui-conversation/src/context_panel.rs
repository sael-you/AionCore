//! The conversation Context panel (TASK-158, design C41): what fills the
//! agent's context window, the auto-compact setting and "Compact session".
//!
//! Two sources. `cli`: claude's own context report (`get_context_usage`, stored
//! by the agent pump as `runtime.context_report`; field names verified in the
//! claude 2.1.325 binary). `estimated`: every other backend reports totals
//! only, so the split is what AionCore knows it injected (the preset rules,
//! bytes / 4) and "messages" for the rest.

use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    AutoCompactMode, AutoCompactView, CompactConversationResponse, ContextCategory, ContextCost, ContextItems,
    ContextMcpToolItem, ContextMemoryFileItem, ContextNamedItem, ContextSource, ConversationContextResponse,
    EXTRA_AUTO_COMPACT, SendMessageRequest,
};
use aionui_common::now_ms;
use serde_json::Value;

use crate::error::ConversationError;
use crate::service::ConversationService;

/// Backends with a compact command: claude takes `/compact` as a slash
/// message; codex maps `/compact` to `thread/compact/start`
/// (codex_conn.rs `SlashRoute::Compact`).
const COMPACT_BACKENDS: [&str; 2] = ["claude", "codex"];
/// The only backend with an auto-compact threshold setting (see
/// `claude_auto_compact_env` in aionui-ai-agent).
const AUTO_COMPACT_BACKEND: &str = "claude";
/// Category ids the claude CLI uses (the `id` enum of its report schema).
const KNOWN_CATEGORY_IDS: [&str; 13] = [
    "system_prompt",
    "system_tools",
    "mcp_tools",
    "mcp_instructions",
    "mcp_tools_deferred",
    "system_tools_deferred",
    "custom_agents",
    "memory_files",
    "skills",
    "messages",
    "autocompact_buffer",
    "compact_buffer",
    "free_space",
];

/// A stored report older than this is refreshed when the panel asks for it.
const REPORT_REFRESH_AFTER_MS: i64 = 60_000;

/// No report yet, or one taken more than [`REPORT_REFRESH_AFTER_MS`] ago.
fn report_is_stale(stored_report: Option<&Value>, now: i64) -> bool {
    stored_report
        .and_then(|s| s.get("updated_at"))
        .and_then(Value::as_i64)
        .is_none_or(|at| now.saturating_sub(at) > REPORT_REFRESH_AFTER_MS)
}

fn category(id: &str, name: &str, tokens: u64, deferred: bool, estimated: bool) -> ContextCategory {
    ContextCategory {
        id: id.to_owned(),
        label_key: format!("conversation.context.category.{id}"),
        name: name.to_owned(),
        tokens,
        deferred,
        estimated,
    }
}

fn percent(used: u64, size: Option<u64>) -> Option<f64> {
    let size = size.filter(|s| *s > 0)?;
    Some(((used as f64 / size as f64) * 1000.0).round() / 10.0)
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn tokens_of(v: &Value) -> u64 {
    v.get("tokens").and_then(Value::as_u64).unwrap_or(0)
}

fn list<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

/// The conversation's backend label (`claude`, `codex`, …) from its `extra`.
fn backend_of(extra: &Value) -> &str {
    extra.get("backend").and_then(Value::as_str).unwrap_or_default()
}

fn mode_of(extra: &Value) -> AutoCompactMode {
    extra
        .get(EXTRA_AUTO_COMPACT)
        .and_then(Value::as_str)
        .and_then(AutoCompactMode::parse)
        .unwrap_or_default()
}

fn cost_of(usage: Option<&Value>) -> Option<ContextCost> {
    let cost = usage?.get("cost")?;
    Some(ContextCost {
        amount: cost.get("amount")?.as_f64()?,
        currency: cost.get("currency").and_then(Value::as_str).unwrap_or("USD").to_owned(),
    })
}

/// Build the panel from what is stored. `usage` = `runtime.context_usage`,
/// `stored_report` = `runtime.context_report` (`{report, updated_at}`), `extra`
/// = the conversation's extra. `None` before the first usage.
pub(crate) fn build_context(
    usage: Option<&Value>,
    stored_report: Option<&Value>,
    extra: &Value,
    now: i64,
) -> Option<ConversationContextResponse> {
    let backend = backend_of(extra);
    let supported = backend == AUTO_COMPACT_BACKEND;
    let mode = mode_of(extra);
    let report = stored_report
        .and_then(|s| s.get("report"))
        .filter(|r| r.get("totalTokens").and_then(Value::as_u64).is_some());

    if let Some(report) = report {
        let used = report.get("totalTokens").and_then(Value::as_u64).unwrap_or(0);
        let size = report.get("maxTokens").and_then(Value::as_u64).filter(|s| *s > 0);
        let categories = list(report, "categories")
            .map(|c| {
                let name = str_of(c, "name");
                let id = c
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| KNOWN_CATEGORY_IDS.contains(id))
                    .unwrap_or("other");
                let deferred = c.get("isDeferred").and_then(Value::as_bool).unwrap_or(false)
                    || c.get("kind").and_then(Value::as_str) == Some("deferred");
                category(id, &name, tokens_of(c), deferred, false)
            })
            .collect();
        let items = ContextItems {
            mcp_tools: list(report, "mcpTools")
                .map(|t| ContextMcpToolItem {
                    name: str_of(t, "name"),
                    server: str_of(t, "serverName"),
                    tokens: tokens_of(t),
                    loaded: t.get("isLoaded").and_then(Value::as_bool),
                })
                .collect(),
            memory_files: list(report, "memoryFiles")
                .map(|f| ContextMemoryFileItem {
                    path: str_of(f, "path"),
                    kind: str_of(f, "type"),
                    tokens: tokens_of(f),
                })
                .collect(),
            agents: list(report, "agents")
                .map(|a| ContextNamedItem {
                    name: str_of(a, "agentType"),
                    source: str_of(a, "source"),
                    tokens: tokens_of(a),
                })
                .collect(),
            skills: report
                .get("skills")
                .map(|s| {
                    list(s, "skillFrontmatter")
                        .map(|k| ContextNamedItem {
                            name: str_of(k, "name"),
                            source: str_of(k, "source"),
                            tokens: tokens_of(k),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        let enabled = report
            .get("isAutoCompactEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let threshold_tokens = report
            .get("autoCompactThreshold")
            .and_then(Value::as_u64)
            .filter(|_| enabled);
        return Some(ConversationContextResponse {
            used,
            size,
            percent: percent(used, size),
            model: report
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_owned),
            auto_compact: AutoCompactView {
                supported,
                mode,
                threshold_tokens,
                until_tokens: threshold_tokens.map(|t| t.saturating_sub(used)),
            },
            categories,
            items,
            cost: cost_of(usage),
            source: ContextSource::Cli,
            updated_at: stored_report
                .and_then(|s| s.get("updated_at"))
                .and_then(Value::as_i64)
                .unwrap_or(now),
        });
    }

    // Estimated: totals from the agent, the split from what AionCore injected.
    let used = usage?.get("used").and_then(Value::as_u64)?;
    let size = usage
        .and_then(|u| u.get("size"))
        .and_then(Value::as_u64)
        .filter(|s| *s > 0);
    let rules_bytes = ["preset_context", "preset_rules"]
        .iter()
        .filter_map(|k| extra.get(*k).and_then(Value::as_str))
        .map(str::len)
        .sum::<usize>() as u64;
    let rules = (rules_bytes / 4).min(used);
    let mut categories = Vec::new();
    if rules > 0 {
        categories.push(category("system_prompt", "System prompt", rules, false, true));
    }
    categories.push(category("messages", "Messages", used - rules, false, true));
    if let Some(size) = size {
        categories.push(category(
            "free_space",
            "Free space",
            size.saturating_sub(used),
            false,
            true,
        ));
    }
    Some(ConversationContextResponse {
        used,
        size,
        percent: percent(used, size),
        model: None,
        auto_compact: AutoCompactView {
            supported,
            mode,
            threshold_tokens: None,
            until_tokens: None,
        },
        categories,
        items: ContextItems::default(),
        cost: cost_of(usage),
        source: ContextSource::Estimated,
        updated_at: now,
    })
}

impl ConversationService {
    /// `GET /api/conversations/{id}/context`. Owner-scoped like `/usage`;
    /// reads only stored state, so it never waits on the agent. When a live
    /// claude session has no report, or one older than
    /// [`REPORT_REFRESH_AFTER_MS`] (the tab was opened on an idle
    /// conversation), it asks for one — the answer arrives as an
    /// `acp_context_report` frame. The age gate also bounds the asks: the GET
    /// that frame triggers finds a fresh report.
    pub async fn get_context(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ConversationContextResponse>, ConversationError> {
        let row = self.owned_row(user_id, conversation_id).await?;
        let extra: Value = serde_json::from_str(&row.extra).unwrap_or_default();
        let state = self
            .acp_session_repo()
            .load_runtime_state_for_user(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load context state: {e}")))?;
        let parse = |raw: Option<&String>| raw.and_then(|r| serde_json::from_str::<Value>(r).ok());
        let usage = parse(state.as_ref().and_then(|s| s.context_usage_json.as_ref()));
        let report = parse(state.as_ref().and_then(|s| s.context_report_json.as_ref()));
        let now = now_ms();
        if report_is_stale(report.as_ref(), now)
            && backend_of(&extra) == AUTO_COMPACT_BACKEND
            && let Ok(task) = self.task(conversation_id)
        {
            let asked = task.refresh_context_report().await;
            tracing::debug!(conversation_id, asked, "context report missing or old; asked the agent");
        }
        Ok(build_context(usage.as_ref(), report.as_ref(), &extra, now))
    }

    /// `PUT /api/conversations/{id}/auto-compact`. Stored in the
    /// conversation's `extra`; the agent reads it at its next start.
    pub async fn set_auto_compact(
        &self,
        user_id: &str,
        conversation_id: &str,
        mode: AutoCompactMode,
    ) -> Result<AutoCompactView, ConversationError> {
        let row = self.owned_row(user_id, conversation_id).await?;
        let extra: Value = serde_json::from_str(&row.extra).unwrap_or_default();
        if backend_of(&extra) != AUTO_COMPACT_BACKEND {
            return Err(ConversationError::AutoCompactUnsupported {
                conversation_id: conversation_id.to_owned(),
            });
        }
        let matched = self
            .conversation_repo()
            .set_extra_keys(
                user_id,
                conversation_id,
                &[(EXTRA_AUTO_COMPACT, Value::String(mode.as_str().to_owned()))],
                None,
                None,
            )
            .await?;
        if !matched {
            return Err(ConversationError::NotFound {
                id: conversation_id.to_owned(),
            });
        }
        tracing::info!(conversation_id, mode = mode.as_str(), "auto-compact setting changed");
        // The threshold in force is the running agent's; the panel re-reads it.
        let current = self.get_context(user_id, conversation_id).await?;
        let (threshold_tokens, until_tokens) = current
            .map(|c| (c.auto_compact.threshold_tokens, c.auto_compact.until_tokens))
            .unwrap_or((None, None));
        Ok(AutoCompactView {
            supported: true,
            mode,
            threshold_tokens,
            until_tokens,
        })
    }

    /// `POST /api/conversations/{id}/compact`: start the backend's compact as
    /// a normal turn (so it is serialized, cancellable and visible like one).
    pub async fn compact(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<CompactConversationResponse, ConversationError> {
        let row = self.owned_row(user_id, conversation_id).await?;
        let extra: Value = serde_json::from_str(&row.extra).unwrap_or_default();
        if !COMPACT_BACKENDS.contains(&backend_of(&extra)) {
            return Err(ConversationError::CompactUnsupported {
                conversation_id: conversation_id.to_owned(),
            });
        }
        if row.status.as_deref() == Some("running") || self.runtime_state().is_claimed(conversation_id) {
            return Err(ConversationError::TurnRunning {
                conversation_id: conversation_id.to_owned(),
            });
        }
        let request = SendMessageRequest {
            content: "/compact".to_owned(),
            files: Vec::new(),
            sessions: Vec::new(),
            inject_skills: Vec::new(),
            hidden: false,
            idempotency_key: None,
        };
        let sent = self
            .send_message(user_id, conversation_id, request, task_manager)
            .await?;
        tracing::info!(conversation_id, "compact session started");
        Ok(CompactConversationResponse { msg_id: sent.msg_id })
    }

    async fn owned_row(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<aionui_db::models::ConversationRow, ConversationError> {
        self.conversation_repo()
            .get(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load conversation: {e}")))?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude_report() -> Value {
        // Field names: the get_context_usage response schema of claude 2.1.325.
        json!({
            "report": {
                "totalTokens": 603_800, "maxTokens": 1_000_000, "rawMaxTokens": 1_000_000,
                "percentage": 60, "model": "claude-opus-5-5",
                "isAutoCompactEnabled": true, "autoCompactThreshold": 963_800,
                "categories": [
                    {"id": "system_prompt", "name": "System prompt", "tokens": 3_800, "color": "x"},
                    {"id": "messages", "name": "Messages", "tokens": 600_000, "color": "x"},
                    {"id": "mcp_tools_deferred", "name": "MCP tools (deferred)", "tokens": 42_000, "color": "inactive", "isDeferred": true},
                    {"id": "free_space", "name": "Free space", "tokens": 396_200, "color": "x", "kind": "free"},
                    {"name": "Something new", "tokens": 5, "color": "x"}
                ],
                "mcpTools": [{"name": "search", "serverName": "docs", "tokens": 900, "isLoaded": false}],
                "memoryFiles": [{"path": "/w/CLAUDE.md", "type": "Project", "tokens": 1_200}],
                "agents": [{"agentType": "reviewer", "source": "projectSettings", "tokens": 300}],
                "skills": {"totalSkills": 1, "includedSkills": 1, "tokens": 80,
                    "skillFrontmatter": [{"name": "cs-build", "source": "plugin", "tokens": 80}]}
            },
            "updated_at": 1_795_000_000_000_i64
        })
    }

    #[test]
    fn a_report_is_refreshed_when_missing_or_older_than_a_minute() {
        let at = |ms: i64| json!({"report": {}, "updated_at": ms});
        assert!(report_is_stale(None, 100_000));
        assert!(report_is_stale(Some(&json!({"report": {}})), 100_000));
        assert!(report_is_stale(Some(&at(30_000)), 100_000));
        // Fresh: the GET triggered by the report's own frame does not ask again.
        assert!(!report_is_stale(Some(&at(99_000)), 100_000));
        assert!(!report_is_stale(Some(&at(40_000)), 100_000));
    }

    #[test]
    fn nothing_before_the_first_usage() {
        assert_eq!(build_context(None, None, &json!({"backend": "claude"}), 1), None);
    }

    #[test]
    fn a_claude_report_is_passed_through_category_by_category() {
        let usage = json!({"used": 603_800, "size": 1_000_000, "cost": {"amount": 1.5, "currency": "USD"}});
        let extra = json!({"backend": "claude", "auto_compact": "60"});
        let ctx = build_context(Some(&usage), Some(&claude_report()), &extra, 5).expect("context");

        assert_eq!(ctx.source, ContextSource::Cli);
        assert_eq!(
            (ctx.used, ctx.size, ctx.percent),
            (603_800, Some(1_000_000), Some(60.4))
        );
        assert_eq!(ctx.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(ctx.updated_at, 1_795_000_000_000);
        assert_eq!(
            ctx.cost,
            Some(ContextCost {
                amount: 1.5,
                currency: "USD".into()
            })
        );
        let ids: Vec<(&str, bool)> = ctx.categories.iter().map(|c| (c.id.as_str(), c.deferred)).collect();
        assert_eq!(
            ids,
            vec![
                ("system_prompt", false),
                ("messages", false),
                ("mcp_tools_deferred", true),
                ("free_space", false),
                ("other", false)
            ]
        );
        assert_eq!(
            ctx.categories[0].label_key,
            "conversation.context.category.system_prompt"
        );
        assert_eq!(ctx.categories[4].name, "Something new");
        assert!(ctx.categories.iter().all(|c| !c.estimated));
        // Acceptance: the loaded categories (not deferred, not free space) sum to `used` within 1 %.
        let loaded: u64 = ctx
            .categories
            .iter()
            .filter(|c| !c.deferred && c.id != "free_space")
            .map(|c| c.tokens)
            .sum();
        assert!(loaded.abs_diff(ctx.used) * 100 <= ctx.used, "{loaded} vs {}", ctx.used);

        assert_eq!(
            ctx.auto_compact,
            AutoCompactView {
                supported: true,
                mode: AutoCompactMode::P60,
                threshold_tokens: Some(963_800),
                until_tokens: Some(360_000),
            }
        );
        assert_eq!(ctx.items.mcp_tools[0].server, "docs");
        assert_eq!(ctx.items.mcp_tools[0].loaded, Some(false));
        assert_eq!(ctx.items.memory_files[0].kind, "Project");
        assert_eq!(ctx.items.agents[0].name, "reviewer");
        assert_eq!(ctx.items.skills[0].name, "cs-build");
    }

    #[test]
    fn a_disabled_auto_compact_has_no_threshold() {
        let mut stored = claude_report();
        stored["report"]["isAutoCompactEnabled"] = json!(false);
        let ctx = build_context(
            None,
            Some(&stored),
            &json!({"backend": "claude", "auto_compact": "off"}),
            5,
        )
        .unwrap();
        assert_eq!(ctx.auto_compact.mode, AutoCompactMode::Off);
        assert_eq!(ctx.auto_compact.threshold_tokens, None);
        assert_eq!(ctx.auto_compact.until_tokens, None);
    }

    #[test]
    fn other_backends_get_totals_and_an_estimated_split() {
        let usage = json!({"used": 50_000, "size": 258_400});
        let extra = json!({"backend": "codex", "preset_context": "x".repeat(4_000)});
        let ctx = build_context(Some(&usage), None, &extra, 9).expect("context");

        assert_eq!(ctx.source, ContextSource::Estimated);
        assert_eq!(ctx.updated_at, 9);
        assert_eq!((ctx.used, ctx.size, ctx.percent), (50_000, Some(258_400), Some(19.3)));
        let rows: Vec<(&str, u64, bool)> = ctx
            .categories
            .iter()
            .map(|c| (c.id.as_str(), c.tokens, c.estimated))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("system_prompt", 1_000, true),
                ("messages", 49_000, true),
                ("free_space", 208_400, true)
            ]
        );
        assert!(!ctx.auto_compact.supported, "the select is hidden");
        assert_eq!(ctx.auto_compact.mode, AutoCompactMode::Auto);
        assert_eq!(ctx.items, ContextItems::default());
    }

    #[test]
    fn an_unknown_window_size_gives_no_percent_and_no_free_space() {
        let ctx = build_context(Some(&json!({"used": 1_200})), None, &json!({"backend": "codex"}), 1).unwrap();
        assert_eq!((ctx.size, ctx.percent), (None, None));
        assert_eq!(ctx.categories.len(), 1);
        assert_eq!(ctx.categories[0].id, "messages");
    }
}
