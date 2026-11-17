//! TASK-116: per-board `agent_access` (agents across gates, with an owner
//! approval note under `approval`) and conversation assignees.

use crate::common;

use aionui_api_types::{
    AgentAccess, AssigneeKind, BoardSettingsDto, UpdateBoardCardRequest, UpdateBoardSettingsRequest,
};
use aionui_board::BoardActor;
use common::*;
use serde_json::json;

async fn set_access(env: &Env, version: i64, access: &str) -> Result<BoardSettingsDto, aionui_board::BoardError> {
    env.service
        .update_settings(
            &owner(),
            PROJECT,
            UpdateBoardSettingsRequest {
                version,
                enabled: None,
                template: None,
                settings: Some(json!({ "agent_access": access })),
            },
        )
        .await
}

async fn keeper(env: &Env) -> (BoardActor, BoardSettingsDto) {
    let settings = enable(&env.service, PROJECT).await;
    let caller = env
        .service
        .resolve_agent(USER, settings.keeper_conversation_id.as_deref().unwrap())
        .await
        .unwrap();
    assert!(caller.keeper);
    (BoardActor::Agent(caller), settings)
}

async fn last_event(env: &Env, card_id: &str) -> aionui_api_types::BoardEventDto {
    env.service
        .card_events(&owner(), card_id, None, Some(100))
        .await
        .unwrap()
        .items
        .pop()
        .unwrap()
}

#[tokio::test]
async fn the_access_defaults_by_project_and_only_full_or_approval_is_stored() {
    let env = env_with_host(true).await;
    let settings = enable(&env.service, PROJECT).await;
    assert_eq!(
        settings.agent_access,
        AgentAccess::Approval,
        "any other project: approval"
    );

    // The owner's TB-test and clauterm-studio projects default to full.
    for name in ["TB-test", "clauterm-studio"] {
        sqlx::query("UPDATE projects SET name = ? WHERE project_id = ?")
            .bind(name)
            .bind(PROJECT)
            .execute(env.pool())
            .await
            .unwrap();
        let view = env.service.board_view(&owner(), PROJECT, false).await.unwrap();
        assert_eq!(view.settings.agent_access, AgentAccess::Full, "{name}");
    }

    let error = set_access(&env, settings.version, "owner_only").await.unwrap_err();
    assert_eq!(code(error.clone()), "board_invalid");
    assert_eq!(error.details().unwrap()["field"], "settings.agent_access");
    let stored = set_access(&env, settings.version, "approval").await.unwrap();
    assert_eq!(
        stored.agent_access,
        AgentAccess::Approval,
        "a stored value beats the default"
    );
    assert_eq!(stored.settings["agent_access"], "approval");
}

#[tokio::test]
async fn with_full_access_the_keeper_moves_through_the_decision_gate_and_the_history_says_so() {
    let env = env_with_host(true).await;
    let (keeper, settings) = keeper(&env).await;
    set_access(&env, settings.version, "full").await.unwrap();
    let card = env
        .service
        .create_card(&owner(), PROJECT, create_in("c", "triage"))
        .await
        .unwrap();

    // Into the Decision gate, then out of it (no owner decision needed).
    let card = env
        .service
        .move_card(&keeper, &card.id, mv("decision", "triage"))
        .await
        .unwrap();
    assert_eq!(card.column_key, "decision");
    let card = env
        .service
        .move_card(&keeper, &card.id, mv("planned", "decision"))
        .await
        .unwrap();
    assert_eq!(card.column_key, "planned");
    let event = last_event(&env, &card.id).await;
    assert_eq!(event.kind, "gate_crossed");
    assert_eq!(event.actor_ref, keeper.actor_ref());
    assert_eq!(event.payload["gate"], "decision");
    assert_eq!(event.payload["agent_access"], "full");
    assert_eq!(event.payload["from"], "decision");
    assert_eq!(event.payload["to"], "planned");
    assert!(event.payload["approval"].is_null());

    // Straight over the gate too.
    let other = env
        .service
        .create_card(&owner(), PROJECT, create_in("o", "triage"))
        .await
        .unwrap();
    let other = env
        .service
        .move_card(&keeper, &other.id, mv("planned", "triage"))
        .await
        .unwrap();
    assert_eq!(other.column_key, "planned");

    // `deploying` is never entered by a move, whatever the access.
    let error = env
        .service
        .move_card(&keeper, &other.id, mv("deploying", "planned"))
        .await
        .unwrap_err();
    assert_eq!(
        code(error),
        "board_agent_move_denied",
        "deploying stays closed to agents"
    );
}

#[tokio::test]
async fn with_approval_access_an_agent_crosses_a_gate_only_with_an_owner_note() {
    let env = env_with_host(true).await;
    let (keeper, _) = keeper(&env).await; // proj-1 defaults to approval
    let card = env
        .service
        .create_card(&owner(), PROJECT, create_in("c", "triage"))
        .await
        .unwrap();

    let error = env
        .service
        .move_card(&keeper, &card.id, mv("decision", "triage"))
        .await
        .unwrap_err();
    assert_eq!(code(error.clone()), "board_agent_move_denied");
    let details = error.details().unwrap();
    assert_eq!(details["approval_required"], true);
    assert_eq!(details["gate"], "decision");

    let blank = aionui_board::MoveCard {
        approval_note: Some("   ".into()),
        ..mv("decision", "triage")
    };
    assert_eq!(
        code(env.service.move_card(&keeper, &card.id, blank).await.unwrap_err()),
        "board_agent_move_denied",
        "a blank note is no approval"
    );

    let approved = aionui_board::MoveCard {
        approval_note: Some("owner in chat 2026-11-16 14:02: ship it".into()),
        ..mv("decision", "triage")
    };
    let moved = env.service.move_card(&keeper, &card.id, approved).await.unwrap();
    assert_eq!(moved.column_key, "decision");
    let event = last_event(&env, &card.id).await;
    assert_eq!(event.kind, "gate_crossed");
    assert_eq!(event.payload["agent_access"], "approval");
    assert_eq!(event.payload["approval"]["by"], "owner");
    assert_eq!(
        event.payload["approval"]["via"],
        "owner in chat 2026-11-16 14:02: ship it"
    );
    assert!(event.payload["approval"]["at"].as_i64().unwrap() > 0);

    // Non-gate moves need no note.
    let plain = env
        .service
        .create_card(&owner(), PROJECT, create_in("p", "ideas"))
        .await
        .unwrap();
    env.service
        .move_card(&keeper, &plain.id, mv("triage", "ideas"))
        .await
        .unwrap();
    assert_eq!(last_event(&env, &plain.id).await.kind, "card_moved");
}

#[tokio::test]
async fn a_card_is_assigned_to_one_of_the_users_conversations() {
    let env = env_with_host(true).await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    let update = |version: i64, conversation: &str| UpdateBoardCardRequest {
        version,
        assignee_conversation: Some(conversation.into()),
        ..Default::default()
    };

    let error = env
        .service
        .update_card(&owner(), &card.id, update(card.version, "no-such-conversation"))
        .await
        .unwrap_err();
    assert_eq!(code(error.clone()), "board_invalid");
    assert_eq!(error.details().unwrap()["field"], "assignee_conversation");
    // Another user's conversation is not yours.
    conversation(env.pool(), OTHER, "conv-other", None, "{}").await;
    let error = env
        .service
        .update_card(&owner(), &card.id, update(card.version, "conv-other"))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_invalid");

    let assigned = env
        .service
        .update_card(&owner(), &card.id, update(card.version, CONV))
        .await
        .unwrap();
    assert_eq!(assigned.assignee_kind, Some(AssigneeKind::Conversation));
    assert_eq!(assigned.assignee_ref.as_deref(), Some(CONV));
    assert_eq!(last_event(&env, &card.id).await.payload["changed"], json!(["assignee"]));

    // Agents set it too (the CLI's `board update --assignee-conversation`).
    let agent = BoardActor::Agent(env.service.resolve_agent(USER, CONV).await.unwrap());
    let again = env
        .service
        .update_card(&agent, &card.id, update(assigned.version, CONV))
        .await
        .unwrap();
    assert_eq!(again.assignee_ref.as_deref(), Some(CONV), "unchanged, still assigned");
}

const CLOSING_COLUMNS: [&str; 4] = ["deployed", "done", "parked", "rejected"];

#[tokio::test]
async fn with_full_access_the_keeper_records_shipped_and_closed_cards_in_every_terminal_column() {
    let env = env_with_host(true).await;
    let (keeper, settings) = keeper(&env).await;
    set_access(&env, settings.version, "full").await.unwrap();
    for column in CLOSING_COLUMNS {
        // From work in progress (past Ready to deploy and deploying) …
        let card = env
            .service
            .create_card(&owner(), PROJECT, create_in(column, "local_test"))
            .await
            .unwrap();
        let moved = env
            .service
            .move_card(&keeper, &card.id, mv(column, "local_test"))
            .await
            .unwrap_or_else(|error| panic!("local_test → {column}: {error:?}"));
        assert_eq!(moved.column_key, column);
        let event = last_event(&env, &card.id).await;
        assert_eq!(event.kind, "gate_crossed", "{column}");
        assert_eq!(event.payload["gate"], "ready_to_deploy", "{column}");
        assert_eq!(event.payload["to"], column);
    }
    // … and between the closing columns (deployed → done passes nothing but
    // still sits after deploying).
    let card = env
        .service
        .create_card(&owner(), PROJECT, create_in("shipped", "deployed"))
        .await
        .unwrap();
    let moved = env
        .service
        .move_card(&keeper, &card.id, mv("done", "deployed"))
        .await
        .unwrap();
    assert_eq!(moved.column_key, "done");
    let event = last_event(&env, &card.id).await;
    assert_eq!(
        (event.kind.as_str(), event.payload["gate"].as_str()),
        ("gate_crossed", Some("deploying"))
    );

    // Never INTO deploying itself: only the ship flow enters it.
    let card = env
        .service
        .create_card(&owner(), PROJECT, create_in("x", "local_test"))
        .await
        .unwrap();
    let error = env
        .service
        .move_card(&keeper, &card.id, mv("deploying", "local_test"))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_agent_move_denied");
}

#[tokio::test]
async fn with_approval_access_closing_moves_need_the_owners_note() {
    let env = env_with_host(true).await;
    let (keeper, _) = keeper(&env).await; // proj-1 defaults to approval
    for column in CLOSING_COLUMNS {
        let card = env
            .service
            .create_card(&owner(), PROJECT, create_in(column, "local_test"))
            .await
            .unwrap();
        let error = env
            .service
            .move_card(&keeper, &card.id, mv(column, "local_test"))
            .await
            .unwrap_err();
        assert_eq!(code(error.clone()), "board_agent_move_denied", "{column}");
        assert_eq!(error.details().unwrap()["approval_required"], true, "{column}");
        let approved = aionui_board::MoveCard {
            approval_note: Some(format!("owner: {column} it")),
            ..mv(column, "local_test")
        };
        let moved = env.service.move_card(&keeper, &card.id, approved).await.unwrap();
        assert_eq!(moved.column_key, column);
        let event = last_event(&env, &card.id).await;
        assert_eq!(event.kind, "gate_crossed");
        assert_eq!(event.payload["approval"]["via"], format!("owner: {column} it"));
    }
}

#[tokio::test]
async fn the_keeper_sets_wip_limits_only_under_full_access_within_bounds() {
    let env = env_with_host(true).await;
    let (keeper, settings) = keeper(&env).await;
    let limits = |pairs: &[(&str, i64)]| {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), *v))
            .collect::<std::collections::BTreeMap<String, i64>>()
    };
    let wanted = limits(&[("building", 3), ("local_test", 3)]);

    // proj-1 defaults to approval: refused.
    let error = env.service.set_wip_limits(&keeper, PROJECT, &wanted).await.unwrap_err();
    assert_eq!(code(error), "board_agent_move_denied");

    let settings = set_access(&env, settings.version, "full").await.unwrap();
    let changed = env.service.set_wip_limits(&keeper, PROJECT, &wanted).await.unwrap();
    assert_eq!(changed.version, settings.version + 1);
    let wip = |key: &str| changed.columns.iter().find(|c| c.key == key).unwrap().wip_limit;
    assert_eq!(
        (wip("building"), wip("local_test"), wip("review")),
        (Some(3), Some(3), Some(4))
    );
    let page = env
        .service
        .project_events(&owner(), PROJECT, None, Some(100))
        .await
        .unwrap();
    let event = page.items.last().unwrap();
    assert_eq!(event.kind, "columns_changed");
    assert_eq!(event.actor_ref, keeper.actor_ref());
    assert_eq!(event.payload["wip"], json!({ "building": 3, "local_test": 3 }));

    for bad in [
        limits(&[("building", 0)]),
        limits(&[("building", 21)]),
        limits(&[("nowhere", 2)]),
        limits(&[]),
    ] {
        let error = env.service.set_wip_limits(&keeper, PROJECT, &bad).await.unwrap_err();
        assert_eq!(code(error), "board_invalid", "{bad:?}");
    }
    // A department agent is not the Keeper.
    let department = BoardActor::Agent(env.service.resolve_agent(USER, CONV).await.unwrap());
    let error = env
        .service
        .set_wip_limits(&department, PROJECT, &wanted)
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_agent_move_denied");
}
