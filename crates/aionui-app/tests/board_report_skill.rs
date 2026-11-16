//! Content guard for the `cs-board-report` auto-inject skill (C25).
//!
//! Every conversation receives this skill, so its Butler lookup must match the
//! wired board CLI: `board status` → `keeper_conversation_id`, with
//! `session list` as the fallback and the platform Butler kept to platform
//! paths only.

use std::path::PathBuf;

fn skill_prose() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/builtin-skills/auto-inject/cs-board-report/SKILL.md");
    let body = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_skill_declares_its_name() {
    let prose = skill_prose();
    assert!(prose.starts_with("--- name: cs-board-report description:"), "{prose}");
}

#[test]
fn the_butler_comes_from_board_status_with_a_session_list_fallback() {
    let prose = skill_prose();
    assert!(
        aionui_api_types::board_tool_descriptors()
            .iter()
            .any(|tool| tool.cli_command == ["status"]),
        "the board CLI must still offer `status`"
    );
    let status_at = prose
        .find("\"$AIONUI_HELPER_BIN\" board status")
        .expect("the skill must resolve the Butler via board status");
    let keeper_at = prose.find("keeper_conversation_id").expect("names the status field");
    let fallback_at = prose
        .find("\"$AIONUI_HELPER_BIN\" session list")
        .expect("session list fallback when the id is null");
    assert!(status_at < keeper_at && keeper_at < fallback_at, "{prose}");
    assert!(prose.contains("· Board Butler"), "{prose}");
}

#[test]
fn the_platform_butler_is_limited_to_platform_paths() {
    let prose = skill_prose();
    assert!(
        prose.contains(
            "Use the platform Butler `c7fe7b70` ONLY for work under `/root/projects/aionui/**` (incl. `.wt/` worktrees) or `/root/projects/clauterm-studio`."
        ),
        "{prose}"
    );
}

#[test]
fn the_skill_keeps_the_no_burst_and_one_way_rules() {
    let prose = skill_prose();
    assert!(prose.contains("one line per card per milestone"), "{prose}");
    assert!(prose.contains("ONE message"), "{prose}");
    assert!(prose.contains("never answer a Butler acknowledgement"), "{prose}");
    assert!(prose.contains("\"$AIONUI_HELPER_BIN\" session send-message"), "{prose}");
}
