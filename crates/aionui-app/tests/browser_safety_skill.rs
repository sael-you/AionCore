//! Content guard for the `browser-safety` auto-inject skill (C30, TASK-111).
//!
//! The owner gave agents full control of the server browser; safety comes from
//! this skill. The test keeps the owner's never-without-OK list, the human-pace
//! rule, the skipped-step log and the bridge-only Playwright CLI use in the prose.

use std::path::PathBuf;

fn skill_prose() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/builtin-skills/auto-inject/browser-safety/SKILL.md");
    let body = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_skill_declares_its_name() {
    let prose = skill_prose();
    assert!(prose.starts_with("--- name: browser-safety description:"), "{prose}");
}

#[test]
fn every_owner_rule_needs_an_explicit_ok() {
    let prose = skill_prose();
    let ok_at = prose
        .find("Never without the owner's explicit OK in THIS conversation")
        .expect("the explicit-OK section");
    for rule in [
        "send, post, reply, comment, publish or share",
        "delete, archive",
        "payments, purchases, checkout",
        "account, security, privacy",
        "accept terms, invitations",
        "download a file and open or run it",
        "export or reveal tokens, cookies, localStorage",
    ] {
        let at = prose.find(rule).unwrap_or_else(|| panic!("missing rule: {rule}"));
        assert!(at > ok_at, "{rule} must sit under the explicit-OK section");
    }
    assert!(prose.contains("When unsure, stop and ask"), "{prose}");
    assert!(prose.contains("One OK covers one action"), "{prose}");
}

#[test]
fn the_skill_keeps_human_pace_and_logs_skipped_steps() {
    let prose = skill_prose();
    assert!(prose.contains("Human pace"), "{prose}");
    assert!(prose.contains("No bulk loops"), "{prose}");
    assert!(prose.contains("Log what you skipped"), "{prose}");
}

#[test]
fn playwright_cli_goes_through_the_bridge_and_never_shows_a_token() {
    let prose = skill_prose();
    assert!(
        prose.contains("playwright-cli -s=aionui attach --cdp \"http://127.0.0.1:$AIONUI_CDP_ACTIVE_PORT\""),
        "{prose}"
    );
    assert!(prose.contains("never put a token on a command line"), "{prose}");
    assert!(prose.contains("do not work around it"), "{prose}");
    assert!(!prose.contains("token="), "the skill must not show a tokened URL");
}

#[test]
fn instances_and_tabs_follow_the_owner_limits() {
    let prose = skill_prose();
    assert!(prose.contains("$AIONUI_CDP_PORTS"), "{prose}");
    assert!(prose.contains("BROWSER_INSTANCE_LIMIT"), "{prose}");
    assert!(prose.contains("BROWSER_TAB_LIMIT"), "{prose}");
    assert!(prose.contains("Do not close the owner's own tabs"), "{prose}");
    assert!(prose.contains("BROWSER_AGENTS_DISABLED"), "{prose}");
}

#[test]
fn agents_pick_a_profile_by_name_and_never_substitute_another() {
    let prose = skill_prose();
    assert!(prose.contains("$AIONUI_CDP_INSTANCES"), "{prose}");
    assert!(prose.contains("select((.name | ascii_downcase)"), "{prose}");
    assert!(prose.contains("never use another profile instead"), "{prose}");
    assert!(prose.contains("BROWSER_INSTANCE_NOT_FOUND"), "{prose}");
}

#[test]
fn the_owners_own_browser_is_named_and_its_pauses_are_respected() {
    let prose = skill_prose();
    assert!(prose.contains(r#""kind": "device", "name": "Mac · Chrome""#), "{prose}");
    assert!(prose.contains("BROWSER_APPROVAL_DENIED"), "{prose}");
    assert!(prose.contains("never retry"), "{prose}");
    assert!(prose.contains("instead of switching to a server profile"), "{prose}");
}
