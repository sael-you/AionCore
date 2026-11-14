//! Hierarchical conversations: limits, pure validation / planning helpers, and
//! the child-result report format.
//!
//! Everything here is pure (no repo access) so the rules can be unit-tested
//! without a database; `ConversationService` feeds it rows and applies the
//! outcome.

use std::collections::HashMap;

use aionui_db::models::ConversationRow;

/// Deepest allowed nesting: root = 0, the deepest child = `MAX_DEPTH`.
pub const MAX_DEPTH: u32 = 3;

/// Most direct children one parent may have.
pub const MAX_CHILDREN_PER_PARENT: usize = 12;

/// Most characters of the child's final assistant text carried in a report.
pub const CHILD_RESULT_MAX_CHARS: usize = 4000;

/// Opening / closing markers of the report block delivered to the parent.
pub const CHILD_RESULT_MARKER: &str = "[[AION_CHILD_RESULT";
pub const CHILD_RESULT_END_MARKER: &str = "[[/AION_CHILD_RESULT]]";

/// A rule of the hierarchy that a create / reparent request broke. The wire
/// values (`code`) are shared by the REST errors and the agent CLI error enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HierarchyViolation {
    ParentNotFound,
    ParentDepthExceeded,
    ParentFanoutExceeded,
    ParentCycle,
}

impl HierarchyViolation {
    pub fn code(self) -> &'static str {
        match self {
            Self::ParentNotFound => "parent_not_found",
            Self::ParentDepthExceeded => "parent_depth_exceeded",
            Self::ParentFanoutExceeded => "parent_fanout_exceeded",
            Self::ParentCycle => "parent_cycle",
        }
    }
}

/// `depth(parent) + 1 <= MAX_DEPTH` for a brand-new child.
pub fn check_new_child_depth(parent_depth: i64) -> Result<i64, HierarchyViolation> {
    let depth = parent_depth.max(0) + 1;
    if depth > i64::from(MAX_DEPTH) {
        Err(HierarchyViolation::ParentDepthExceeded)
    } else {
        Ok(depth)
    }
}

/// `children(parent) < MAX_CHILDREN_PER_PARENT` before adding one more.
pub fn check_fanout(current_children: usize) -> Result<(), HierarchyViolation> {
    if current_children >= MAX_CHILDREN_PER_PARENT {
        Err(HierarchyViolation::ParentFanoutExceeded)
    } else {
        Ok(())
    }
}

/// The depth rewrite for moving `moved` (and its whole subtree) under a parent
/// at `new_parent_depth` (`None` = detach to root).
///
/// `descendants` is the moved conversation's current subtree (excluding
/// itself). Returns `(conversation_id, new_depth)` for the moved conversation
/// and every descendant, or `ParentDepthExceeded` when the deepest descendant
/// would land below `MAX_DEPTH`.
pub fn plan_reparent_depths(
    moved: &ConversationRow,
    descendants: &[ConversationRow],
    new_parent_depth: Option<i64>,
) -> Result<Vec<(String, i64)>, HierarchyViolation> {
    let new_depth = new_parent_depth.map_or(0, |depth| depth.max(0) + 1);
    // A descendant's depth is its structural distance from the moved root, not
    // `stored depth + delta`: that stays correct even if a stored depth drifted.
    let by_id: HashMap<&str, &ConversationRow> = descendants.iter().map(|row| (row.id.as_str(), row)).collect();
    let hop_cap = descendants.len() + 1;
    let mut updates = Vec::with_capacity(descendants.len() + 1);
    updates.push((moved.id.clone(), new_depth));
    for row in descendants {
        let mut hops = 1usize;
        let mut cursor = row.parent_conversation_id.as_deref();
        while let Some(parent) = cursor {
            if parent == moved.id || hops > hop_cap {
                break;
            }
            hops += 1;
            cursor = by_id.get(parent).and_then(|r| r.parent_conversation_id.as_deref());
        }
        updates.push((row.id.clone(), new_depth + hops as i64));
    }
    if updates.iter().any(|(_, depth)| *depth > i64::from(MAX_DEPTH)) {
        return Err(HierarchyViolation::ParentDepthExceeded);
    }
    Ok(updates)
}

/// Terminal status of the child's turn, as carried in the report header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildResultStatus {
    Finished,
    Error,
    Cancelled,
}

impl ChildResultStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Truncate to at most `max` characters (not bytes), never splitting a char.
pub fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte_index, _)) => text[..byte_index].to_owned(),
        None => text.to_owned(),
    }
}

/// Build the report delivered to the parent:
///
/// ```text
/// [[AION_CHILD_RESULT child_id=<id> name=<name> status=<finished|error|cancelled>]]
/// <final assistant text, at most 4000 chars>
/// [[/AION_CHILD_RESULT]]
/// ```
///
/// The name is single-lined and stripped of `]` so it cannot terminate the
/// header early.
pub fn format_child_result(child_id: &str, name: &str, status: ChildResultStatus, final_text: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c.is_control() || c == ']' { ' ' } else { c })
        .collect();
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let body = truncate_chars(final_text.trim(), CHILD_RESULT_MAX_CHARS);
    format!(
        "{CHILD_RESULT_MARKER} child_id={child_id} name={name} status={}]]\n{body}\n{CHILD_RESULT_END_MARKER}",
        status.as_str()
    )
}

/// Whether `extra.report_to_parent` allows reporting. Absent or non-bool means
/// "yes" (the default); only an explicit `false` opts out.
pub fn reports_to_parent(extra: &serde_json::Value) -> bool {
    extra
        .get("report_to_parent")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, parent: Option<&str>, depth: i64) -> ConversationRow {
        ConversationRow {
            id: id.to_owned(),
            user_id: "u".to_owned(),
            name: id.to_owned(),
            r#type: "acp".to_owned(),
            extra: "{}".to_owned(),
            model: None,
            status: None,
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 0,
            updated_at: 0,
            project_id: None,
            folder_id: None,
            name_source: None,
            parent_conversation_id: parent.map(str::to_owned),
            depth,
        }
    }

    #[test]
    fn limits_are_the_contract_values() {
        assert_eq!(MAX_DEPTH, 3);
        assert_eq!(MAX_CHILDREN_PER_PARENT, 12);
    }

    #[test]
    fn violation_codes_are_distinct_snake_case() {
        let codes = [
            HierarchyViolation::ParentNotFound,
            HierarchyViolation::ParentDepthExceeded,
            HierarchyViolation::ParentFanoutExceeded,
            HierarchyViolation::ParentCycle,
        ]
        .map(HierarchyViolation::code);
        assert_eq!(
            codes,
            [
                "parent_not_found",
                "parent_depth_exceeded",
                "parent_fanout_exceeded",
                "parent_cycle"
            ]
        );
    }

    #[test]
    fn new_child_depth_is_parent_plus_one_up_to_the_cap() {
        assert_eq!(check_new_child_depth(0), Ok(1));
        assert_eq!(check_new_child_depth(2), Ok(3));
        assert_eq!(check_new_child_depth(3), Err(HierarchyViolation::ParentDepthExceeded));
    }

    #[test]
    fn fanout_allows_up_to_twelve_children() {
        assert_eq!(check_fanout(11), Ok(()));
        assert_eq!(check_fanout(12), Err(HierarchyViolation::ParentFanoutExceeded));
    }

    #[test]
    fn reparent_rewrites_every_descendant_depth() {
        // a(1) -> b(2) -> c(3); move a under a depth-0 root: a=1 stays, b=2, c=3.
        let a = row("a", Some("old"), 1);
        let b = row("b", Some("a"), 2);
        let c = row("c", Some("b"), 3);
        let plan = plan_reparent_depths(&a, &[b.clone(), c.clone()], Some(0)).unwrap();
        assert_eq!(
            plan,
            vec![("a".to_owned(), 1), ("b".to_owned(), 2), ("c".to_owned(), 3)]
        );
    }

    #[test]
    fn reparent_to_root_pulls_the_subtree_up() {
        let a = row("a", Some("old"), 2);
        let b = row("b", Some("a"), 3);
        let plan = plan_reparent_depths(&a, &[b], None).unwrap();
        assert_eq!(plan, vec![("a".to_owned(), 0), ("b".to_owned(), 1)]);
    }

    #[test]
    fn reparent_rejects_a_subtree_that_would_overflow_max_depth() {
        let a = row("a", None, 0);
        let b = row("b", Some("a"), 1);
        let c = row("c", Some("b"), 2);
        // a under a depth-1 parent => a=2, b=3, c=4 > MAX_DEPTH.
        assert_eq!(
            plan_reparent_depths(&a, &[b.clone(), c.clone()], Some(1)),
            Err(HierarchyViolation::ParentDepthExceeded)
        );
        // under a depth-0 parent => a=1, b=2, c=3 fits exactly.
        assert!(plan_reparent_depths(&a, &[b, c], Some(0)).is_ok());
    }

    #[test]
    fn reparent_uses_structural_depth_not_stale_stored_depth() {
        let a = row("a", None, 0);
        let b = row("b", Some("a"), 9); // corrupted stored depth
        let plan = plan_reparent_depths(&a, &[b], Some(0)).unwrap();
        assert_eq!(plan, vec![("a".to_owned(), 1), ("b".to_owned(), 2)]);
    }

    #[test]
    fn child_result_has_header_body_and_footer() {
        let text = format_child_result("c1", "Research", ChildResultStatus::Finished, "  done  ");
        assert_eq!(
            text,
            "[[AION_CHILD_RESULT child_id=c1 name=Research status=finished]]\ndone\n[[/AION_CHILD_RESULT]]"
        );
    }

    #[test]
    fn child_result_truncates_to_4000_chars_on_a_char_boundary() {
        let long = "é".repeat(5000);
        let text = format_child_result("c1", "n", ChildResultStatus::Error, &long);
        let body = text.lines().nth(1).unwrap();
        assert_eq!(body.chars().count(), CHILD_RESULT_MAX_CHARS);
        assert!(text.contains("status=error]]"));
        assert!(text.ends_with(CHILD_RESULT_END_MARKER));
    }

    #[test]
    fn child_result_name_cannot_break_the_header() {
        let text = format_child_result("c1", "a]]\nb", ChildResultStatus::Cancelled, "x");
        let header = text.lines().next().unwrap();
        assert_eq!(header, "[[AION_CHILD_RESULT child_id=c1 name=a b status=cancelled]]");
    }

    #[test]
    fn report_to_parent_defaults_true_and_only_explicit_false_opts_out() {
        assert!(reports_to_parent(&serde_json::json!({})));
        assert!(reports_to_parent(&serde_json::json!({"report_to_parent": true})));
        assert!(!reports_to_parent(&serde_json::json!({"report_to_parent": false})));
    }
}
