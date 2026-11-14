//! Pure evaluation of gate `owner_if` (R3) and column exit rules (R4).
//!
//! Both read the card, its current reviews and its questions; they run inside
//! the move/gate transaction on rows read by that transaction.

use aionui_db::{BoardCardRow, BoardQuestionRow, BoardReviewRow};
use serde_json::Value;

/// Department name questions to the owner are addressed to.
pub const OWNER_DEPARTMENT: &str = "owner";

fn routing(card: &BoardCardRow) -> Vec<String> {
    serde_json::from_str(&card.routing).unwrap_or_default()
}

/// Current verdicts of the card's routed departments.
fn routed_verdicts<'a>(card: &BoardCardRow, reviews: &'a [BoardReviewRow]) -> Vec<&'a BoardReviewRow> {
    let routing = routing(card);
    reviews.iter().filter(|r| routing.contains(&r.department)).collect()
}

fn list_contains(rule: &Value, key: &str, value: Option<&str>) -> Option<bool> {
    let list = rule.get(key)?.as_array()?;
    // Unknown (NULL) size/tier counts as matching: unknown = ask.
    Some(match value {
        None => true,
        Some(value) => list.iter().any(|item| item.as_str() == Some(value)),
    })
}

/// `owner_if` of a gate: predicates combine with OR; `{}` (or a missing
/// `owner_if`) = always.
pub fn owner_must_decide(
    gate: &Value,
    card: &BoardCardRow,
    reviews: &[BoardReviewRow],
    questions: &[BoardQuestionRow],
) -> bool {
    let Some(rule) = gate.get("owner_if").and_then(Value::as_object) else {
        return true;
    };
    if rule.is_empty() {
        return true;
    }
    let rule = Value::Object(rule.clone());
    let verdicts = routed_verdicts(card, reviews);
    let size = list_contains(&rule, "size", card.size.as_deref()).unwrap_or(false);
    let tier = list_contains(&rule, "tier", card.tier.as_deref()).unwrap_or(false);
    let any_reject =
        rule.get("any_reject").and_then(Value::as_bool) == Some(true) && verdicts.iter().any(|r| r.verdict == "reject");
    let conflict = rule.get("conflict").and_then(Value::as_bool) == Some(true) && {
        let approve = verdicts.iter().any(|r| r.verdict == "approve");
        let against = verdicts.iter().any(|r| r.verdict == "change" || r.verdict == "reject");
        let owner_question = questions
            .iter()
            .any(|q| q.answered_by_event.is_none() && q.to_department == OWNER_DEPARTMENT);
        (approve && against) || owner_question
    };
    size || tier || any_reject || conflict
}

/// Missing items of a column's exit rule (`[]` = satisfied):
/// `review:<dept>` (routed department without a non-pending verdict),
/// `open_questions`, `min_confidence` (a routed verdict below it).
pub fn exit_rule_missing(
    exit_rule: &Value,
    card: &BoardCardRow,
    reviews: &[BoardReviewRow],
    questions: &[BoardQuestionRow],
) -> Vec<String> {
    let mut missing = Vec::new();
    let routing = routing(card);
    if exit_rule.get("reviews_required").and_then(Value::as_str) == Some("routing") {
        for department in &routing {
            let done = reviews
                .iter()
                .any(|r| &r.department == department && r.verdict != "pending");
            if !done {
                missing.push(format!("review:{department}"));
            }
        }
    }
    if exit_rule.get("no_open_questions").and_then(Value::as_bool) == Some(true)
        && questions.iter().any(|q| q.answered_by_event.is_none())
    {
        missing.push("open_questions".to_owned());
    }
    if let Some(min) = exit_rule.get("min_confidence").and_then(Value::as_f64)
        && routed_verdicts(card, reviews)
            .iter()
            .any(|review| review.confidence.is_none_or(|c| c < min))
    {
        missing.push("min_confidence".to_owned());
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card(size: Option<&str>, tier: Option<&str>, routing: &[&str]) -> BoardCardRow {
        BoardCardRow {
            id: "c".into(),
            project_id: "p".into(),
            user_id: "u".into(),
            number: 1,
            title: "t".into(),
            description: String::new(),
            card_type: "feature".into(),
            priority: "medium".into(),
            size: size.map(str::to_owned),
            tier: tier.map(str::to_owned),
            column_key: "decision".into(),
            position: 0,
            labels: "[]".into(),
            skills: "[]".into(),
            routing: serde_json::to_string(routing).unwrap(),
            creator_kind: "user".into(),
            creator_ref: "u".into(),
            assignee_kind: None,
            assignee_ref: None,
            blocked: false,
            blocked_reason: None,
            change_id: None,
            metadata: "{}".into(),
            version: 1,
            archived_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn review(department: &str, verdict: &str, confidence: Option<f64>) -> BoardReviewRow {
        BoardReviewRow {
            id: department.into(),
            card_id: "c".into(),
            project_id: "p".into(),
            user_id: "u".into(),
            department: department.into(),
            reviewer_ref: "x".into(),
            conversation_id: None,
            verdict: verdict.into(),
            summary: String::new(),
            findings: "[]".into(),
            recommendation: String::new(),
            assumptions: "[]".into(),
            confidence,
            tokens_used: 0,
            version: 1,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn question(to: &str, open: bool) -> BoardQuestionRow {
        BoardQuestionRow {
            id: "q".into(),
            card_id: "c".into(),
            project_id: "p".into(),
            user_id: "u".into(),
            from_department: "qa".into(),
            to_department: to.into(),
            text: "?".into(),
            answered_by_event: (!open).then(|| "e".into()),
            version: 1,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn decision() -> Value {
        json!({ "owner_if": { "size": ["m", "l"], "tier": ["t2", "t3"], "any_reject": true, "conflict": true } })
    }

    #[test]
    fn owner_if_predicates_combine_with_or_and_unknown_means_ask() {
        assert!(!owner_must_decide(
            &decision(),
            &card(Some("s"), Some("t0"), &[]),
            &[],
            &[]
        ));
        assert!(owner_must_decide(
            &decision(),
            &card(Some("m"), Some("t0"), &[]),
            &[],
            &[]
        ));
        assert!(owner_must_decide(
            &decision(),
            &card(Some("s"), Some("t3"), &[]),
            &[],
            &[]
        ));
        assert!(
            owner_must_decide(&decision(), &card(None, Some("t0"), &[]), &[], &[]),
            "NULL size = ask"
        );
        assert!(owner_must_decide(
            &json!({ "owner_if": {} }),
            &card(Some("s"), Some("t0"), &[]),
            &[],
            &[]
        ));
        assert!(owner_must_decide(
            &json!({}),
            &card(Some("s"), Some("t0"), &[]),
            &[],
            &[]
        ));
    }

    #[test]
    fn rejects_and_conflicts_among_routed_verdicts_need_the_owner() {
        let small = card(Some("s"), Some("t0"), &["qa", "arch"]);
        assert!(owner_must_decide(
            &decision(),
            &small,
            &[review("qa", "reject", None)],
            &[]
        ));
        assert!(owner_must_decide(
            &decision(),
            &small,
            &[review("qa", "approve", None), review("arch", "change", None)],
            &[]
        ));
        assert!(
            !owner_must_decide(
                &decision(),
                &small,
                &[review("qa", "approve", None), review("other", "reject", None)],
                &[]
            ),
            "unrouted departments do not count"
        );
        assert!(owner_must_decide(&decision(), &small, &[], &[question("owner", true)]));
        assert!(!owner_must_decide(
            &decision(),
            &small,
            &[],
            &[question("owner", false)]
        ));
    }

    #[test]
    fn exit_rules_list_what_is_missing() {
        let card = card(None, None, &["qa", "arch"]);
        let rule = json!({ "reviews_required": "routing", "no_open_questions": true, "min_confidence": 0.7 });
        let reviews = [review("qa", "approve", Some(0.9)), review("arch", "pending", Some(0.2))];
        assert_eq!(
            exit_rule_missing(&rule, &card, &reviews, &[question("qa", true)]),
            ["review:arch", "open_questions", "min_confidence"]
        );
        let done = [review("qa", "approve", Some(0.9)), review("arch", "change", Some(0.8))];
        assert!(exit_rule_missing(&rule, &card, &done, &[question("qa", false)]).is_empty());
    }
}
