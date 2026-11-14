//! Column templates (SPEC §3). Every board uses the department (`studio`)
//! flow (D39): all columns closed to agents (the supervisor and the Board
//! Keeper move), two gates, exit rules and WIP defaults.

/// Column keys the rules refer to by name.
pub const DEPLOYING: &str = "deploying";
pub const READY_TO_DEPLOY: &str = "ready_to_deploy";

use aionui_api_types::{AgentMove, BoardTemplate, ColumnCategory, ColumnKind, ColumnResolution};
use serde_json::{Value, json};

/// One column a template creates.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnTemplate {
    pub key: &'static str,
    pub category: ColumnCategory,
    pub kind: ColumnKind,
    pub resolution: Option<ColumnResolution>,
    pub agent_move: AgentMove,
    pub wip_limit: Option<i64>,
    pub exit_rule: Option<Value>,
    pub gate: Option<Value>,
}

fn column(key: &'static str, category: ColumnCategory, agent_move: AgentMove) -> ColumnTemplate {
    ColumnTemplate {
        key,
        category,
        kind: ColumnKind::Normal,
        resolution: None,
        agent_move,
        wip_limit: None,
        exit_rule: None,
        gate: None,
    }
}

fn terminal(
    key: &'static str,
    category: ColumnCategory,
    resolution: ColumnResolution,
    agent_move: AgentMove,
) -> ColumnTemplate {
    ColumnTemplate {
        kind: ColumnKind::Terminal,
        resolution: Some(resolution),
        ..column(key, category, agent_move)
    }
}

/// The columns a template creates, in board order.
pub fn template_columns(template: BoardTemplate) -> Vec<ColumnTemplate> {
    match template {
        BoardTemplate::Studio => studio(),
    }
}

fn studio() -> Vec<ColumnTemplate> {
    use ColumnCategory::*;
    let deny = AgentMove::Deny;
    let gate = |key: &'static str, category: ColumnCategory, rule: Value| ColumnTemplate {
        kind: ColumnKind::Gate,
        gate: Some(rule),
        ..column(key, category, deny)
    };
    let with = |mut template: ColumnTemplate, wip: Option<i64>, exit_rule: Option<Value>| {
        template.wip_limit = wip;
        template.exit_rule = exit_rule;
        template
    };
    vec![
        column("ideas", Backlog, deny),
        column("inbox", Backlog, deny),
        column("triage", Todo, deny),
        with(
            column("review", Todo, deny),
            Some(4),
            Some(json!({ "reviews_required": "routing" })),
        ),
        with(
            column("accuracy", Todo, deny),
            None,
            Some(json!({ "no_open_questions": true })),
        ),
        with(
            gate(
                "decision",
                Todo,
                json!({
                    "owner_if": { "size": ["m", "l"], "tier": ["t2", "t3"], "any_reject": true, "conflict": true },
                    "on_approve": "planned",
                    "on_reject": "parked"
                }),
            ),
            None,
            Some(json!({ "reviews_required": "routing", "no_open_questions": true })),
        ),
        column("planned", InProgress, deny),
        with(column("building", InProgress, deny), Some(1), None),
        with(column("local_test", InProgress, deny), Some(1), None),
        gate(
            READY_TO_DEPLOY,
            InProgress,
            json!({ "owner_if": {}, "on_approve": "deploying", "on_reject": "local_test" }),
        ),
        column(DEPLOYING, InProgress, deny),
        terminal("deployed", Done, ColumnResolution::Completed, deny),
        terminal("done", Done, ColumnResolution::Completed, deny),
        terminal("parked", Done, ColumnResolution::Dropped, deny),
        terminal("rejected", Done, ColumnResolution::Dropped, deny),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_denies_agents_and_marks_both_gates() {
        let columns = template_columns(BoardTemplate::Studio);
        assert_eq!(columns.len(), 15);
        assert!(columns.iter().all(|c| c.agent_move == AgentMove::Deny));
        let gates: Vec<_> = columns
            .iter()
            .filter(|c| c.kind == ColumnKind::Gate)
            .map(|c| c.key)
            .collect();
        assert_eq!(gates, ["decision", "ready_to_deploy"]);
        let wip: Vec<_> = columns.iter().filter_map(|c| c.wip_limit.map(|w| (c.key, w))).collect();
        assert_eq!(wip, [("review", 4), ("building", 1), ("local_test", 1)]);
        let dropped: Vec<_> = columns
            .iter()
            .filter(|c| c.resolution == Some(ColumnResolution::Dropped))
            .map(|c| c.key)
            .collect();
        assert_eq!(dropped, ["parked", "rejected"]);
    }
}
