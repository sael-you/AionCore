//! The rules block of a session (C37 §3/§4). Pure.
//!
//! Order: global → project → assistant → conversation (the caller passes the
//! rules already sorted). The header states the precedence so it is explicit
//! instead of left to each model. Rules are added whole until the 24 KB cap;
//! what does not fit is NAMED at the end of the block (never cut silently).

use aionui_api_types::RULES_EFFECTIVE_MAX_BYTES;
use aionui_db::EffectiveRule;

pub const RULES_HEADER: &str = "[Owner Rules]\n\
These rules are written by the owner in AionUi and always apply. Precedence: platform policy and the hard rules \
of this repository's own AGENTS.md / CLAUDE.md come first; then these rules, where a more specific level wins \
(conversation > assistant > project > global); then skills; then CLI memory.";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposedRules {
    /// Empty when no rule applies.
    pub block: String,
    /// Rule ids in the block.
    pub delivered: Vec<String>,
    /// Rule ids left out because of the cap.
    pub omitted: Vec<String>,
}

pub fn compose_rules(rules: &[EffectiveRule]) -> ComposedRules {
    compose_rules_capped(rules, RULES_EFFECTIVE_MAX_BYTES)
}

pub(crate) fn compose_rules_capped(rules: &[EffectiveRule], cap: usize) -> ComposedRules {
    let mut out = ComposedRules::default();
    if rules.is_empty() {
        return out;
    }
    let mut body = String::new();
    let mut used = 0usize;
    let mut omitted_names: Vec<&str> = Vec::new();
    for r in rules {
        let text = r.rule.body.trim();
        if used + text.len() > cap {
            out.omitted.push(r.rule.id.clone());
            omitted_names.push(&r.rule.name);
            continue;
        }
        used += text.len();
        body.push_str(&format!("\n\n## {} ({})\n{}", r.rule.name, r.level, text));
        out.delivered.push(r.rule.id.clone());
    }
    out.block = format!("{RULES_HEADER}{body}");
    if !omitted_names.is_empty() {
        out.block.push_str(&format!(
            "\n\n(Not included, the {} KB rules limit was reached: {}. Tell the owner.)",
            cap / 1024,
            omitted_names.join(", ")
        ));
    }
    out
}

/// `rules block` + the session's own preset context (assistant rules).
pub fn prepend_rules(block: &str, preset_context: Option<String>) -> Option<String> {
    if block.is_empty() {
        return preset_context;
    }
    match preset_context.filter(|c| !c.trim().is_empty()) {
        Some(existing) => Some(format!("{block}\n\n{existing}")),
        None => Some(block.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::models::RuleRow;

    fn rule(id: &str, level: &'static str, body: &str) -> EffectiveRule {
        EffectiveRule {
            rule: RuleRow {
                id: id.into(),
                user_id: "u".into(),
                name: format!("Rule {id}"),
                description: String::new(),
                body: body.into(),
                enabled: true,
                origin: "owner".into(),
                origin_ref: String::new(),
                created_at: 0,
                updated_at: 0,
                deleted_at: None,
            },
            level,
        }
    }

    #[test]
    fn no_rules_no_block() {
        assert_eq!(compose_rules(&[]), ComposedRules::default());
        assert_eq!(prepend_rules("", Some("ctx".into())).as_deref(), Some("ctx"));
        assert_eq!(prepend_rules("", None), None);
    }

    #[test]
    fn block_keeps_order_levels_and_precedence_header() {
        let out = compose_rules(&[rule("a", "global", "Be brief.\n"), rule("b", "project", "Use pnpm.")]);
        assert!(out.block.starts_with("[Owner Rules]"));
        assert!(out.block.contains("conversation > assistant > project > global"));
        let a = out.block.find("## Rule a (global)\nBe brief.").unwrap();
        let b = out.block.find("## Rule b (project)\nUse pnpm.").unwrap();
        assert!(a < b);
        assert_eq!(out.delivered, vec!["a", "b"]);
        assert!(out.omitted.is_empty());
        assert_eq!(
            prepend_rules(&out.block, Some("assistant rules".into())).unwrap(),
            format!("{}\n\nassistant rules", out.block)
        );
    }

    #[test]
    fn over_the_cap_rules_are_named_not_cut() {
        let out = compose_rules_capped(
            &[
                rule("a", "global", &"x".repeat(60)),
                rule("b", "project", &"y".repeat(60)),
                rule("c", "conversation", "ok"),
            ],
            100,
        );
        assert_eq!(out.delivered, vec!["a", "c"]);
        assert_eq!(out.omitted, vec!["b"]);
        assert!(!out.block.contains("yyyy"));
        assert!(out.block.contains("Not included") && out.block.contains("Rule b"));
    }
}
