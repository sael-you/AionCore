//! Kit resolution (card C27, D49/D50): the ONE place that turns a scope's
//! attachments into per-item decisions. Pure (no I/O): the repository loads a
//! [`KitLayers`] for a scope, every consumer (effective-set endpoint, skill
//! snapshot at conversation create, MCP market gate, `aioncore mcp exec`)
//! calls [`resolve`] on it.
//!
//! Order: global → project → assistant → conversation; a lower level wins.
//! Inside one level a `remove` beats an `add`. An item no level mentions keeps
//! its base state, so `effective = apply(base, resolve(layers))` equals
//! applying `set = (set ∪ adds) − removes` level by level on the base.

use std::collections::{BTreeMap, HashMap};

use crate::models::{KitAttachmentRow, KitItemRow, KitRow};

/// Attach levels in resolution order.
pub const KIT_LEVELS: [&str; 4] = ["global", "project", "assistant", "conversation"];

/// MCP market instances that may be attached globally (R2). The MCP market
/// rules re-export this list (single source).
pub const GLOBAL_MCP_IDS: &[&str] = &["context7", "studio-memory"];

/// Who the effective set is for. Missing ids make that level contribute nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KitScope {
    pub project_id: Option<String>,
    pub assistant_id: Option<String>,
    pub conversation_id: Option<String>,
}

impl KitScope {
    /// The attachment target of `level` for this scope (`Some("")` for global).
    pub fn target(&self, level: &str) -> Option<&str> {
        fn pick(v: &Option<String>) -> Option<&str> {
            v.as_deref().filter(|s| !s.is_empty())
        }
        match level {
            "global" => Some(""),
            "project" => pick(&self.project_id),
            "assistant" => pick(&self.assistant_id),
            "conversation" => pick(&self.conversation_id),
            _ => None,
        }
    }
}

/// Everything [`resolve`] needs for one scope: the scope's attachments and the
/// LIVE kits they reference, with their members.
#[derive(Debug, Clone, Default)]
pub struct KitLayers {
    pub attachments: Vec<KitAttachmentRow>,
    pub kits: HashMap<String, KitRow>,
    pub items: Vec<KitItemRow>,
}

/// The last decision about one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KitDecision {
    pub included: bool,
    pub level: &'static str,
    /// `add` | `remove` (the op that decided it).
    pub op: &'static str,
    /// The kit that carried it; `None` = attached as a single item.
    pub kit_id: Option<String>,
}

/// Decisions per item, keyed by skill name / MCP instance id (sorted).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KitResolution {
    pub skills: BTreeMap<String, KitDecision>,
    pub mcps: BTreeMap<String, KitDecision>,
    /// (template id, kit id) wanted by a kit attached with `add` and still
    /// contributing at the end (at least one level adds it).
    pub templates: Vec<(String, String)>,
}

impl KitResolution {
    /// `base ∪ included − removed`, base order kept, additions appended sorted.
    pub fn apply_skills(&self, base: &[String]) -> Vec<String> {
        apply(base, &self.skills)
    }

    pub fn mcp_granted(&self, id: &str) -> bool {
        self.mcps.get(id).is_some_and(|d| d.included)
    }

    pub fn mcp_removed(&self, id: &str) -> bool {
        self.mcps.get(id).is_some_and(|d| !d.included)
    }
}

fn apply(base: &[String], decisions: &BTreeMap<String, KitDecision>) -> Vec<String> {
    let mut out: Vec<String> = base
        .iter()
        .filter(|name| decisions.get(*name).is_none_or(|d| d.included))
        .cloned()
        .collect();
    for (name, d) in decisions {
        if d.included && !out.iter().any(|n| n == name) {
            out.push(name.clone());
        }
    }
    out
}

/// Resolve the decisions of a scope. Attachments whose target is not the
/// scope's (or whose level/kind/op is unknown) are ignored; a kit that is not
/// in `layers.kits` (deleted) expands to nothing.
pub fn resolve(scope: &KitScope, layers: &KitLayers) -> KitResolution {
    let mut members: HashMap<&str, Vec<&KitItemRow>> = HashMap::new();
    for item in &layers.items {
        members.entry(item.kit_id.as_str()).or_default().push(item);
    }
    let mut out = KitResolution::default();
    let mut templates: BTreeMap<(String, String), ()> = BTreeMap::new();

    for level in KIT_LEVELS {
        let Some(target) = scope.target(level) else {
            continue;
        };
        // item key -> (op, kit) for this level; a single item beats a kit as
        // provenance, kits in id order.
        let mut adds: BTreeMap<(&str, String), Option<String>> = BTreeMap::new();
        let mut removes: BTreeMap<(&str, String), Option<String>> = BTreeMap::new();
        let mut rows: Vec<&KitAttachmentRow> = layers
            .attachments
            .iter()
            .filter(|a| a.level == level && a.target_id == target)
            .collect();
        rows.sort_by(|a, b| {
            (a.item_kind.as_str(), a.item_ref.as_str()).cmp(&(b.item_kind.as_str(), b.item_ref.as_str()))
        });
        for a in rows {
            let bucket = match a.op.as_str() {
                "add" => &mut adds,
                "remove" => &mut removes,
                _ => continue,
            };
            let mut put = |kind: &'static str, item: &str, kit: Option<&str>| {
                let slot = bucket.entry((kind, item.to_owned())).or_insert(kit.map(str::to_owned));
                if kit.is_none() {
                    *slot = None;
                }
            };
            match a.item_kind.as_str() {
                "skill" => put("skill", &a.item_ref, None),
                "mcp" => put("mcp", &a.item_ref, None),
                "kit" => {
                    if !layers.kits.contains_key(&a.item_ref) {
                        continue;
                    }
                    for m in members.get(a.item_ref.as_str()).into_iter().flatten() {
                        match m.kind.as_str() {
                            "skill" => put("skill", &m.item_ref, Some(&a.item_ref)),
                            "mcp" => put("mcp", &m.item_ref, Some(&a.item_ref)),
                            "template" if a.op == "add" => {
                                templates.insert((m.item_ref.clone(), a.item_ref.clone()), ());
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        for ((kind, item), kit) in adds {
            if removes.contains_key(&(kind, item.clone())) {
                continue;
            }
            target_map(&mut out, kind).insert(
                item,
                KitDecision {
                    included: true,
                    level,
                    op: "add",
                    kit_id: kit,
                },
            );
        }
        for ((kind, item), kit) in removes {
            target_map(&mut out, kind).insert(
                item,
                KitDecision {
                    included: false,
                    level,
                    op: "remove",
                    kit_id: kit,
                },
            );
        }
    }
    out.templates = templates.into_keys().collect();
    out
}

fn target_map<'a>(out: &'a mut KitResolution, kind: &str) -> &'a mut BTreeMap<String, KitDecision> {
    if kind == "skill" {
        &mut out.skills
    } else {
        &mut out.mcps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(level: &str, target: &str, kind: &str, item: &str, op: &str) -> KitAttachmentRow {
        KitAttachmentRow {
            id: format!("{level}-{target}-{kind}-{item}"),
            user_id: "u".into(),
            level: level.into(),
            target_id: target.into(),
            item_kind: kind.into(),
            item_ref: item.into(),
            op: op.into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn kit(id: &str, members: &[(&str, &str)]) -> (KitRow, Vec<KitItemRow>) {
        (
            KitRow {
                id: id.into(),
                user_id: "u".into(),
                name: id.into(),
                description: String::new(),
                builtin_key: None,
                created_at: 0,
                updated_at: 0,
                deleted_at: None,
            },
            members
                .iter()
                .map(|(kind, item)| KitItemRow {
                    kit_id: id.into(),
                    user_id: "u".into(),
                    kind: (*kind).into(),
                    item_ref: (*item).into(),
                })
                .collect(),
        )
    }

    fn layers(kits: Vec<(KitRow, Vec<KitItemRow>)>, attachments: Vec<KitAttachmentRow>) -> KitLayers {
        let mut l = KitLayers {
            attachments,
            ..Default::default()
        };
        for (k, items) in kits {
            l.items.extend(items);
            l.kits.insert(k.id.clone(), k);
        }
        l
    }

    fn scope() -> KitScope {
        KitScope {
            project_id: Some("p1".into()),
            assistant_id: Some("a1".into()),
            conversation_id: Some("c1".into()),
        }
    }

    #[test]
    fn levels_apply_top_down_and_lower_level_wins() {
        let l = layers(
            vec![kit(
                "k6",
                &[("skill", "seo-audit"), ("skill", "ai-seo"), ("mcp", "gsc-1")],
            )],
            vec![
                att("project", "p1", "kit", "k6", "add"),
                att("assistant", "a1", "skill", "seo-audit", "remove"),
                att("conversation", "c1", "skill", "seo-audit", "add"),
                att("conversation", "c1", "mcp", "gsc-1", "remove"),
            ],
        );
        let r = resolve(&scope(), &l);
        let seo = &r.skills["seo-audit"];
        assert!(seo.included);
        assert_eq!(
            (seo.level, seo.op, seo.kit_id.as_deref()),
            ("conversation", "add", None)
        );
        let ai = &r.skills["ai-seo"];
        assert_eq!(
            (ai.included, ai.level, ai.kit_id.as_deref()),
            (true, "project", Some("k6"))
        );
        assert!(r.mcp_removed("gsc-1"));
        assert!(!r.mcp_granted("gsc-1"));
    }

    #[test]
    fn higher_level_remove_is_undone_by_a_lower_add_but_not_the_reverse() {
        let l = layers(
            vec![],
            vec![
                att("global", "", "skill", "pdf", "remove"),
                att("project", "p1", "skill", "pdf", "add"),
                att("project", "p1", "skill", "cron", "add"),
                att("assistant", "a1", "skill", "cron", "remove"),
            ],
        );
        let r = resolve(&scope(), &l);
        assert!(r.skills["pdf"].included);
        assert!(!r.skills["cron"].included);
        assert_eq!(r.skills["cron"].level, "assistant");
    }

    #[test]
    fn same_level_remove_beats_add_from_a_kit() {
        let l = layers(
            vec![kit("k1", &[("skill", "pf-core"), ("skill", "cs-build")])],
            vec![
                att("project", "p1", "kit", "k1", "add"),
                att("project", "p1", "skill", "cs-build", "remove"),
            ],
        );
        let r = resolve(&scope(), &l);
        assert!(r.skills["pf-core"].included);
        assert!(!r.skills["cs-build"].included);
        assert_eq!(r.skills["cs-build"].op, "remove");
    }

    #[test]
    fn apply_removes_base_items_and_appends_additions() {
        let l = layers(
            vec![kit("k2", &[("skill", "cs-verify")])],
            vec![
                att("project", "p1", "kit", "k2", "add"),
                att("conversation", "c1", "skill", "cron", "remove"),
            ],
        );
        let r = resolve(&scope(), &l);
        let base = vec!["cron".to_owned(), "pdf".to_owned()];
        assert_eq!(r.apply_skills(&base), vec!["pdf", "cs-verify"]);
        // No attachments: base unchanged.
        assert_eq!(resolve(&scope(), &KitLayers::default()).apply_skills(&base), base);
    }

    #[test]
    fn kit_remove_removes_its_items_from_above_and_base() {
        let l = layers(
            vec![kit("k5", &[("skill", "analytics-tracking"), ("mcp", "ph-1")])],
            vec![
                att("project", "p1", "kit", "k5", "add"),
                att("conversation", "c1", "kit", "k5", "remove"),
            ],
        );
        let r = resolve(&scope(), &l);
        assert!(!r.skills["analytics-tracking"].included);
        assert_eq!(r.skills["analytics-tracking"].kit_id.as_deref(), Some("k5"));
        assert!(r.mcp_removed("ph-1"));
    }

    #[test]
    fn deleted_kit_and_foreign_targets_contribute_nothing() {
        let (_, items) = kit("gone", &[("skill", "x")]);
        let mut l = layers(
            vec![kit("k1", &[("skill", "pf-core")])],
            vec![
                att("project", "p1", "kit", "gone", "add"),
                att("project", "other", "kit", "k1", "add"),
                att("assistant", "a2", "skill", "pdf", "add"),
            ],
        );
        l.items.extend(items);
        let r = resolve(&scope(), &l);
        assert!(r.skills.is_empty(), "{:?}", r.skills);
        // A scope without project/assistant ignores those levels entirely.
        let only_global = KitScope::default();
        let l = layers(vec![], vec![att("project", "", "skill", "pdf", "add")]);
        assert!(resolve(&only_global, &l).skills.is_empty());
    }

    #[test]
    fn templates_of_added_kits_are_reported() {
        let l = layers(
            vec![kit("k2", &[("template", "sentry"), ("skill", "pf-staging")])],
            vec![att("project", "p1", "kit", "k2", "add")],
        );
        let r = resolve(&scope(), &l);
        assert_eq!(r.templates, vec![("sentry".to_owned(), "k2".to_owned())]);
    }
}
