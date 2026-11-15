//! Text-level target evals (PLAN v15 §5.0 S1b): over a 40-name catalogue,
//! the deterministic resolver must never pick a wrong target, and must ground
//! ≥ 90 % of utterances per category that name a target. Utterances are what
//! the transcription model hands over (misheard names included, S0-A item 8).

use aionui_voice::resolver::{Candidate, Resolution, resolve};

fn catalogue() -> Vec<Candidate> {
    let rows: &[(&str, &str, &str)] = &[
        ("c01", "Kanban spec", "aionui"),
        ("c02", "Kanban build", "aionui"),
        ("c03", "AionCore voice slice", "aionui"),
        ("c04", "AionUi voice mode", "aionui"),
        ("c05", "clauterm-studio planning", "clauterm"),
        ("c06", "Fix login bug", "aionui"),
        ("c07", "Token Pool gateway", "clauterm"),
        ("c08", "Board Keeper", "aionui"),
        ("c09", "Release notes draft", "aionui"),
        ("c10", "MCP market", "aionui"),
        ("c11", "Sidebar ordering", "aionui"),
        ("c12", "Onboarding drift repair", "clauterm"),
        ("c13", "Deploy pipeline", "clauterm"),
        ("c14", "Dark theme polish", "aionui"),
        ("c15", "Wake word benchmark", "aionui"),
        ("c16", "Smart Turn evaluation", "aionui"),
        ("c17", "MiniMax cascade", "aionui"),
        ("c18", "Telegram channel", "aionui"),
        ("c19", "Cron scheduler", "aionui"),
        ("c20", "Office preview", "aionui"),
        ("c21", "Project explorer search", "aionui"),
        ("c22", "Team mailbox", "aionui"),
        ("c23", "Skill installer", "aionui"),
        ("c24", "Provider settings", "aionui"),
        ("c25", "Database migrations", "aionui"),
        ("c26", "Websocket reconnect", "aionui"),
        ("c27", "Budget meter", "aionui"),
        ("c28", "Codex review loop", "clauterm"),
        ("c29", "Gemini research", "clauterm"),
        ("c30", "Playwright checks", "clauterm"),
        ("c31", "Landing page copy", "talkseeds"),
        ("c32", "Pricing experiment", "talkseeds"),
        ("c33", "Newsletter sequence", "talkseeds"),
        ("c34", "Analytics tracking", "talkseeds"),
        ("c35", "Bug triage", "aionui"),
        ("c36", "Performance profiling", "aionui"),
        ("c37", "Android capture worklet", "aionui"),
        ("c38", "Jarvis link", "clauterm"),
        ("c39", "Voice plan review", "clauterm"),
        ("c40", "Voice plan reviews", "clauterm"),
    ];
    rows.iter()
        .map(|(id, name, project)| Candidate {
            id: (*id).into(),
            name: (*name).into(),
            project_name: Some((*project).into()),
        })
        .collect()
}

/// `Some(id)` = the target the user meant; `None` = no unique target exists
/// (the resolver must not pick one).
struct Case {
    category: &'static str,
    utterance: &'static str,
    expected: Option<&'static str>,
    focus: Option<&'static str>,
}

const fn case(category: &'static str, utterance: &'static str, expected: Option<&'static str>) -> Case {
    Case {
        category,
        utterance,
        expected,
        focus: None,
    }
}

fn cases() -> Vec<Case> {
    vec![
        // Names said as written.
        case("exact", "tell the kanban build to rerun the failing tests", Some("c02")),
        case("exact", "in kanban spec add the edge cases", Some("c01")),
        case("exact", "ask the board keeper to move card twelve", Some("c08")),
        case("exact", "tell the token pool gateway to log upgrades", Some("c07")),
        case("exact", "the deploy pipeline should wait for the backup", Some("c13")),
        case("exact", "tell the cron scheduler to skip weekends", Some("c19")),
        case("exact", "in the budget meter add a daily cap test", Some("c27")),
        case("exact", "tell the telegram channel to retry the webhook", Some("c18")),
        case("exact", "send the pricing experiment a new variant idea", Some("c32")),
        case(
            "exact",
            "tell playwright checks to cover the mobile layout",
            Some("c30"),
        ),
        case("exact", "ask the minimax cascade to measure first audio", Some("c17")),
        case("exact", "tell the jarvis link to pair again", Some("c38")),
        // Misheard (S0-A transcripts: "IonCore", "Clawterm", "Aeon").
        case(
            "misheard",
            "in the ion core voice slice add logging to the attach",
            Some("c03"),
        ),
        case("misheard", "tell the eon core voice slice to retry", Some("c03")),
        case(
            "misheard",
            "clawterm studio planning needs the new numbers",
            Some("c05"),
        ),
        case(
            "misheard",
            "in the clauterm studio planning add the review",
            Some("c05"),
        ),
        case("misheard", "the ion ui voice mode needs a mute button", Some("c04")),
        case("misheard", "tell the code ex review loop to stop", Some("c28")),
        case("misheard", "tell the gemeni research to summarize", Some("c29")),
        case("misheard", "the web socket reconnect should back off", Some("c26")),
        case("misheard", "ask the play right checks to run headed", Some("c30")),
        case("misheard", "tell the mini max cascade to switch voices", Some("c17")),
        // Homophones and near-duplicates.
        case("homophone", "tell the kanban build to bump the version", Some("c02")),
        case("homophone", "add acceptance criteria to the kanban spec", Some("c01")),
        case("homophone", "tell kanban to hurry up", None),
        case("homophone", "voice plan review", None),
        // A partial name is not enough: Jarvis asks instead of guessing.
        case("homophone", "tell the voice slice about the budget", None),
        case("homophone", "the voice mode needs captions", None),
        // No name: focus or ask.
        Case {
            category: "focus",
            utterance: "add a retry around the upload",
            expected: Some("c06"),
            focus: Some("c06"),
        },
        Case {
            category: "focus",
            utterance: "and make it shorter",
            expected: Some("c09"),
            focus: Some("c09"),
        },
        case("focus", "add a retry around the upload", None),
        // Chit-chat and questions never produce a target.
        case("chitchat", "thanks that's all for now", None),
        case("chitchat", "what time is it", None),
        case("chitchat", "never mind", None),
        // Injected text names a different target: the resolver only reads
        // the user's transcript, so a quoted instruction cannot redirect.
        case(
            "injected",
            "tell the bug triage that the release notes said to deploy",
            Some("c35"),
        ),
    ]
}

#[test]
fn resolver_evals_never_pick_a_wrong_target_and_ground_names() {
    let catalogue = catalogue();
    let mut wrong = Vec::new();
    let mut per_category: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
    for case in cases() {
        let resolution = resolve(case.utterance, &catalogue, case.focus);
        let picked = match &resolution {
            Resolution::Unique { id, .. } | Resolution::Focus { id } => Some(id.as_str()),
            Resolution::Ambiguous { .. } | Resolution::Unresolved => None,
        };
        match (case.expected, picked) {
            (Some(expected), Some(picked)) if expected != picked => wrong.push(format!(
                "{}: {:?} → {picked} (expected {expected})",
                case.category, case.utterance
            )),
            (None, Some(picked)) => wrong.push(format!(
                "{}: {:?} → {picked} (expected none)",
                case.category, case.utterance
            )),
            _ => {}
        }
        if let Some(expected) = case.expected {
            let entry = per_category.entry(case.category).or_default();
            entry.1 += 1;
            if picked == Some(expected) {
                entry.0 += 1;
            }
        }
    }
    assert!(wrong.is_empty(), "wrong targets: {wrong:#?}");
    for (category, (grounded, total)) in per_category {
        let rate = grounded as f64 / total as f64;
        assert!(rate >= 0.9, "{category}: grounding {grounded}/{total} below 90 %");
    }
}
