//! Hierarchical conversations in the sidebar read model.
//!
//! Conversations that have a parent in the SAME archive slice are not items of
//! any group: they attach to their ROOT item. This module folds the thin
//! independent-conversation list into roots + a root → descendants index, and
//! lifts each root's activity key to `MAX(updated_at)` over its subtree so a
//! root rises when a child is active. Orphans (parent missing from the slice —
//! deleted, or in the other archive scope) are roots. Classification
//! (pinned / project / dir / chats) is done on the root only.

use std::collections::{HashMap, HashSet};

use aionui_db::SidebarConversationThin;

/// One descendant of a root, in the order the client receives them
/// (`created_at` ascending, then `id`).
#[derive(Debug, Clone)]
pub(crate) struct DescendantRef {
    pub id: String,
}

/// Root id → its flat descendant list (all levels).
#[derive(Debug, Default, Clone)]
pub(crate) struct Hierarchy {
    by_root: HashMap<String, Vec<DescendantRef>>,
    descendant_ids: HashSet<String>,
}

impl Hierarchy {
    pub(crate) fn descendants_of(&self, root_id: &str) -> &[DescendantRef] {
        self.by_root.get(root_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether `id` hangs under some root (so it is never an item itself).
    pub(crate) fn is_descendant(&self, id: &str) -> bool {
        self.descendant_ids.contains(id)
    }
}

/// Fold independents into `(roots, hierarchy)`. Each returned root carries
/// `updated_at = MAX(updated_at)` over its whole subtree; root order is the
/// input order (callers sort groups themselves).
pub(crate) fn fold_hierarchy(convs: Vec<SidebarConversationThin>) -> (Vec<SidebarConversationThin>, Hierarchy) {
    if convs.iter().all(|c| c.parent_conversation_id.is_none()) {
        return (convs, Hierarchy::default());
    }
    let index: HashMap<&str, &SidebarConversationThin> = convs.iter().map(|c| (c.id.as_str(), c)).collect();

    // root_of[id] = Some(root id) for descendants, None for roots.
    let mut root_of: HashMap<String, Option<String>> = HashMap::with_capacity(convs.len());
    for conv in &convs {
        root_of.insert(conv.id.clone(), find_root(conv, &index));
    }

    let mut subtree_updated: HashMap<String, i64> = HashMap::new();
    let mut by_root: HashMap<String, Vec<&SidebarConversationThin>> = HashMap::new();
    for conv in &convs {
        if let Some(Some(root)) = root_of.get(&conv.id) {
            by_root.entry(root.clone()).or_default().push(conv);
            let entry = subtree_updated.entry(root.clone()).or_insert(i64::MIN);
            *entry = (*entry).max(conv.updated_at);
        }
    }

    let mut hierarchy = Hierarchy::default();
    for (root, mut members) in by_root {
        members.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        for member in &members {
            hierarchy.descendant_ids.insert(member.id.clone());
        }
        hierarchy.by_root.insert(
            root,
            members
                .into_iter()
                .map(|m| DescendantRef { id: m.id.clone() })
                .collect(),
        );
    }

    let roots = convs
        .into_iter()
        .filter(|c| matches!(root_of.get(&c.id), Some(None)))
        .map(|mut c| {
            if let Some(latest) = subtree_updated.get(&c.id) {
                c.updated_at = c.updated_at.max(*latest);
            }
            c
        })
        .collect();
    (roots, hierarchy)
}

/// Walk parent pointers inside the slice. `None` = `conv` is a root (no parent,
/// parent not in the slice, or a corrupted cycle).
fn find_root(conv: &SidebarConversationThin, index: &HashMap<&str, &SidebarConversationThin>) -> Option<String> {
    let mut current = conv;
    let mut hops = 0usize;
    let mut moved = false;
    while let Some(parent_id) = current.parent_conversation_id.as_deref() {
        let Some(parent) = index.get(parent_id) else { break };
        if parent.id == conv.id {
            // Cycle back to the start: refuse to nest.
            return None;
        }
        moved = true;
        current = parent;
        hops += 1;
        if hops > index.len() {
            return None;
        }
    }
    moved.then(|| current.id.clone())
}

/// All descendants of `id` (excluding `id`) from a raw thin listing, following
/// parent pointers; used by archive / unarchive to cascade to the subtree.
pub(crate) fn subtree_ids(convs: &[SidebarConversationThin], id: &str) -> Vec<String> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for conv in convs {
        if let Some(parent) = conv.parent_conversation_id.as_deref() {
            children.entry(parent).or_default().push(conv.id.as_str());
        }
    }
    let mut seen: HashSet<&str> = HashSet::new();
    seen.insert(id);
    let mut out = Vec::new();
    let mut frontier = vec![id];
    while let Some(current) = frontier.pop() {
        for child in children.get(current).into_iter().flatten() {
            if seen.insert(child) {
                out.push((*child).to_owned());
                frontier.push(child);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thin(id: &str, parent: Option<&str>, created: i64, updated: i64) -> SidebarConversationThin {
        SidebarConversationThin {
            id: id.to_owned(),
            project_id: None,
            workspace: None,
            team_id: None,
            parent_conversation_id: parent.map(str::to_owned),
            updated_at: updated,
            created_at: created,
        }
    }

    #[test]
    fn without_parents_the_listing_is_returned_untouched() {
        let (roots, hierarchy) = fold_hierarchy(vec![thin("a", None, 1, 10), thin("b", None, 2, 20)]);
        assert_eq!(roots.len(), 2);
        assert!(hierarchy.descendants_of("a").is_empty());
    }

    #[test]
    fn children_attach_to_the_root_at_every_level_and_lift_its_activity() {
        let (roots, hierarchy) = fold_hierarchy(vec![
            thin("root", None, 1, 10),
            thin("grand", Some("child"), 3, 99),
            thin("child", Some("root"), 2, 20),
            thin("other", None, 4, 50),
        ]);
        let mut ids: Vec<_> = roots.iter().map(|r| (r.id.as_str(), r.updated_at)).collect();
        ids.sort();
        assert_eq!(ids, vec![("other", 50), ("root", 99)]);
        let desc: Vec<_> = hierarchy.descendants_of("root").iter().map(|d| d.id.as_str()).collect();
        assert_eq!(desc, vec!["child", "grand"], "flat, created_at ascending");
        assert!(hierarchy.is_descendant("grand") && !hierarchy.is_descendant("root"));
    }

    #[test]
    fn an_orphan_whose_parent_is_outside_the_slice_is_a_root() {
        let (roots, hierarchy) = fold_hierarchy(vec![thin("kid", Some("gone"), 1, 5), thin("x", None, 2, 6)]);
        assert_eq!(roots.len(), 2);
        assert!(hierarchy.descendants_of("kid").is_empty());
    }

    #[test]
    fn a_corrupted_cycle_does_not_hang_and_yields_roots() {
        let (roots, _) = fold_hierarchy(vec![thin("a", Some("b"), 1, 1), thin("b", Some("a"), 2, 2)]);
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn subtree_ids_walks_all_levels_excluding_the_start() {
        let convs = vec![
            thin("r", None, 1, 1),
            thin("c1", Some("r"), 2, 2),
            thin("c2", Some("r"), 3, 3),
            thin("g", Some("c1"), 4, 4),
            thin("z", None, 5, 5),
        ];
        let mut ids = subtree_ids(&convs, "r");
        ids.sort();
        assert_eq!(ids, vec!["c1", "c2", "g"]);
        assert!(subtree_ids(&convs, "z").is_empty());
    }
}
