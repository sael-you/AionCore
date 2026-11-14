//! S4 rules over the studio template: R3 gates (owner_if, decisions,
//! idempotency, force), R4 exit rules, R6 reviews and questions, owner
//! links, assign/brainstorm (§6: reservation, find-or-create, fencing,
//! resume, packet once, team path, parent fallback) and the Board Keeper
//! (D40).

mod common;

use aionui_api_types::{
    BoardAssignRequest, BoardBrainstormRequest, BoardGateRequest, BoardLinkRequest, BoardReviewRequest,
    BoardVerdictQuestion, CardSize, CardTier, GateDecision, LinkRole, ParentFallback, ReviewVerdict,
    UpdateBoardCardRequest, UpdateBoardSettingsRequest,
};
use aionui_board::{BoardActor, HostTeam};
use common::*;
use serde_json::json;

fn verdict(verdict: ReviewVerdict) -> BoardReviewRequest {
    BoardReviewRequest {
        verdict,
        summary: "looked at it".into(),
        findings: Vec::new(),
        recommendation: String::new(),
        assumptions: Vec::new(),
        questions: Vec::new(),
        confidence: Some(0.9),
        tokens_used: Some(1200),
    }
}

fn gate(decision: GateDecision, key: &str) -> BoardGateRequest {
    BoardGateRequest {
        decision,
        reason: Some("ok".into()),
        idempotency_key: key.into(),
    }
}

/// A card routed to `qa`, sized/tiered so the Decision gate needs the owner
/// (size m), sitting in `decision`.
async fn card_at_decision(env: &Env, size: Option<CardSize>) -> aionui_api_types::BoardCardDto {
    let card = env
        .service
        .create_card(&owner(), PROJECT, create("feature"))
        .await
        .unwrap();
    let card = env
        .service
        .update_card(
            &owner(),
            &card.id,
            UpdateBoardCardRequest {
                version: card.version,
                size: Some(size),
                tier: Some(Some(CardTier::T0)),
                routing: Some(vec!["qa".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    env.service
        .submit_review(&owner(), &card.id, "qa", verdict(ReviewVerdict::Approve))
        .await
        .unwrap();
    env.service
        .move_card(&owner(), &card.id, mv("decision", "ideas"))
        .await
        .unwrap()
}

#[tokio::test]
async fn r3_a_matching_card_leaves_the_decision_gate_only_through_the_owner() {
    let mut env = env().await;
    enable(&env.service, PROJECT).await;
    let card = card_at_decision(&env, Some(CardSize::M)).await;
    let error = env
        .service
        .move_card(&owner(), &card.id, mv("planned", "decision"))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_gate_use_decision");
    let error = env
        .service
        .move_card(&BoardActor::system(USER), &card.id, mv("planned", "decision"))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_gate_use_decision");
    assert_eq!(
        code(
            env.service
                .decide_gate(&BoardActor::system(USER), &card.id, gate(GateDecision::Approve, "k1"))
                .await
                .unwrap_err()
        ),
        "board_system_forbidden"
    );
    env.drain();
    let approved = env
        .service
        .decide_gate(&owner(), &card.id, gate(GateDecision::Approve, "k1"))
        .await
        .unwrap();
    assert_eq!(approved.column_key, "planned");
    assert_eq!(env.kinds(), ["gate_approved"]);
    let events = env.service.card_events(&owner(), &card.id, None, None).await.unwrap();
    let last = events.items.last().unwrap();
    assert_eq!(
        (last.payload["from"].as_str(), last.payload["to"].as_str()),
        (Some("decision"), Some("planned"))
    );

    // Same key, same request → stored response, no second event.
    let replay = env
        .service
        .decide_gate(&owner(), &card.id, gate(GateDecision::Approve, "k1"))
        .await
        .unwrap();
    assert_eq!(replay, approved);
    assert!(env.kinds().is_empty());
    // Same key, different request → conflict.
    let error = env
        .service
        .decide_gate(&owner(), &card.id, gate(GateDecision::Reject, "k1"))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_idempotency_conflict");
}

#[tokio::test]
async fn r3_owner_if_is_or_and_a_non_matching_card_passes_like_a_normal_column() {
    let env = env().await;
    enable(&env.service, PROJECT).await;
    let small = card_at_decision(&env, Some(CardSize::S)).await;
    let moved = env
        .service
        .move_card(&owner(), &small.id, mv("planned", "decision"))
        .await
        .unwrap();
    assert_eq!(
        moved.column_key, "planned",
        "size s / tier t0 / approve → no owner decision"
    );
    let unknown = card_at_decision(&env, None).await;
    let error = env
        .service
        .move_card(&owner(), &unknown.id, mv("planned", "decision"))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_gate_use_decision", "unknown size = ask");
}

#[tokio::test]
async fn r3_gate_force_needs_a_reason_and_rejects_go_to_on_reject() {
    let env = env().await;
    enable(&env.service, PROJECT).await;
    let card = card_at_decision(&env, Some(CardSize::L)).await;
    let error = env
        .service
        .move_card(&owner(), &card.id, forced("building", "decision", None))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_invalid", "reason required");
    let forced_card = env
        .service
        .move_card(
            &owner(),
            &card.id,
            forced("building", "decision", Some("urgent hotfix")),
        )
        .await
        .unwrap();
    assert_eq!(forced_card.column_key, "building");
    let events = env.service.card_events(&owner(), &card.id, None, None).await.unwrap();
    let last = events.items.last().unwrap();
    assert_eq!(last.kind, "gate_forced");
    assert_eq!(last.payload["reason"], "urgent hotfix");

    let other = card_at_decision(&env, Some(CardSize::M)).await;
    let rejected = env
        .service
        .decide_gate(&owner(), &other.id, gate(GateDecision::Reject, "k2"))
        .await
        .unwrap();
    assert_eq!(rejected.column_key, "parked");
    let error = env
        .service
        .decide_gate(&owner(), &rejected.id, gate(GateDecision::Approve, "k3"))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_invalid", "not in a gate column");
}

#[tokio::test]
async fn r3_ready_to_deploy_is_not_enabled_and_deploying_is_system_only() {
    let env = env().await;
    enable(&env.service, PROJECT).await;
    let card = env
        .service
        .create_card(&owner(), PROJECT, create("ship it"))
        .await
        .unwrap();
    let card = env
        .service
        .move_card(&owner(), &card.id, mv("ready_to_deploy", "ideas"))
        .await
        .unwrap();
    let error = env
        .service
        .decide_gate(&owner(), &card.id, gate(GateDecision::Approve, "d1"))
        .await
        .unwrap_err();
    assert_eq!(
        (code(error.clone()), error.status()),
        ("board_feature_not_enabled", 403)
    );
    assert_eq!(
        code(
            env.service
                .move_card(&owner(), &card.id, mv("deployed", "ready_to_deploy"))
                .await
                .unwrap_err()
        ),
        "board_gate_use_decision",
        "{{}} owner_if = always"
    );
    let manual = env
        .service
        .move_card(
            &owner(),
            &card.id,
            forced("deployed", "ready_to_deploy", Some("deployed by hand")),
        )
        .await
        .unwrap();
    assert_eq!(manual.column_key, "deployed");
    let other = env.service.create_card(&owner(), PROJECT, create("x")).await.unwrap();
    assert_eq!(
        code(
            env.service
                .move_card(&owner(), &other.id, forced("deploying", "ideas", Some("r")))
                .await
                .unwrap_err()
        ),
        "board_gate_use_decision",
        "cards enter deploying only through the Deploy approval"
    );
    // The supervisor runs deploys: it may move into / out of deploying.
    let system = BoardActor::system(USER);
    env.service
        .move_card(&system, &other.id, mv("deploying", "ideas"))
        .await
        .unwrap();
    assert_eq!(
        code(
            env.service
                .move_card(&owner(), &other.id, mv("local_test", "deploying"))
                .await
                .unwrap_err()
        ),
        "board_gate_use_decision"
    );
    let back = env
        .service
        .move_card(
            &owner(),
            &other.id,
            forced("local_test", "deploying", Some("supervisor stuck")),
        )
        .await
        .unwrap();
    assert_eq!(back.column_key, "local_test");
    let events = env.service.card_events(&owner(), &other.id, None, None).await.unwrap();
    assert_eq!(events.items.last().unwrap().kind, "deploy_forced");
}

#[tokio::test]
async fn r4_exit_rules_list_what_is_missing_and_the_owner_may_force() {
    let env = env().await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    let card = env
        .service
        .update_card(
            &owner(),
            &card.id,
            UpdateBoardCardRequest {
                version: card.version,
                routing: Some(vec!["qa".into(), "arch".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    env.service
        .move_card(&owner(), &card.id, mv("review", "ideas"))
        .await
        .unwrap();
    let error = env
        .service
        .move_card(&owner(), &card.id, mv("accuracy", "review"))
        .await
        .unwrap_err();
    assert_eq!(code(error.clone()), "board_exit_rule");
    assert_eq!(error.details().unwrap()["missing"], json!(["review:qa", "review:arch"]));
    env.service
        .submit_review(&owner(), &card.id, "qa", verdict(ReviewVerdict::Approve))
        .await
        .unwrap();
    let mut with_question = verdict(ReviewVerdict::Change);
    with_question.questions = vec![BoardVerdictQuestion {
        to: "owner".into(),
        text: "Which API?".into(),
    }];
    let result = env
        .service
        .submit_review(&owner(), &card.id, "arch", with_question)
        .await
        .unwrap();
    assert_eq!(result.questions.len(), 1);
    env.service
        .move_card(&owner(), &card.id, mv("accuracy", "review"))
        .await
        .unwrap();
    // accuracy: no_open_questions.
    let error = env
        .service
        .move_card(&owner(), &card.id, mv("decision", "accuracy"))
        .await
        .unwrap_err();
    assert_eq!(error.details().unwrap()["missing"], json!(["open_questions"]));
    let moved = env
        .service
        .move_card(&owner(), &card.id, forced("decision", "accuracy", None))
        .await
        .unwrap();
    let events = env.service.card_events(&owner(), &moved.id, None, None).await.unwrap();
    assert_eq!(events.items.last().unwrap().payload["overrides"], json!(["exit_rule"]));
    // Backward moves skip exit rules.
    let error = env
        .service
        .move_card(&owner(), &card.id, mv("triage", "decision"))
        .await
        .unwrap_err();
    assert_eq!(
        code(error),
        "board_gate_use_decision",
        "conflict (approve + change) keeps it at Decision"
    );
}

#[tokio::test]
async fn r6_reviews_keep_one_versioned_verdict_per_department() {
    let mut env = env().await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    env.drain();
    let first = env
        .service
        .submit_review(&owner(), &card.id, "qa", verdict(ReviewVerdict::Change))
        .await
        .unwrap();
    assert_eq!(first.review.version, 1);
    let second = env
        .service
        .submit_review(&owner(), &card.id, "qa", verdict(ReviewVerdict::Approve))
        .await
        .unwrap();
    assert_eq!((second.review.version, second.review.id.clone()), (2, first.review.id));
    let messages = env.drain();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].data["event"]["kind"], "review_updated");
    assert_eq!(messages[1].data["event"]["payload"]["department"], "qa");
    assert_eq!(
        messages[1].data["review"]["verdict"], "approve",
        "the WS hint carries the review"
    );
    let detail = env.service.card_detail(&owner(), &card.id).await.unwrap();
    assert_eq!(detail.reviews.len(), 1);
    assert_eq!(detail.card.version, card.version, "reviews never bump the card");
    let mut bad = verdict(ReviewVerdict::Approve);
    bad.confidence = Some(1.5);
    assert_eq!(
        code(
            env.service
                .submit_review(&owner(), &card.id, "qa", bad)
                .await
                .unwrap_err()
        ),
        "board_invalid"
    );
    assert_eq!(
        code(
            env.service
                .submit_review(
                    &BoardActor::system(USER),
                    &card.id,
                    "qa",
                    verdict(ReviewVerdict::Approve)
                )
                .await
                .unwrap_err()
        ),
        "board_system_forbidden"
    );
}

#[tokio::test]
async fn r6_agents_review_only_for_their_linked_department() {
    let env = env().await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    let agent = BoardActor::Agent(env.service.resolve_agent(USER, CONV).await.unwrap());
    let error = env
        .service
        .submit_review(&agent, &card.id, "qa", verdict(ReviewVerdict::Approve))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_department_mismatch");
    env.service
        .add_link(
            &owner(),
            &card.id,
            BoardLinkRequest {
                conversation_id: CONV.into(),
                role: LinkRole::Review,
                department: Some("qa".into()),
            },
        )
        .await
        .unwrap();
    let error = env
        .service
        .submit_review(&agent, &card.id, "arch", verdict(ReviewVerdict::Approve))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_department_mismatch");
    let result = env
        .service
        .submit_review(&agent, &card.id, "qa", verdict(ReviewVerdict::Approve))
        .await
        .unwrap();
    assert_eq!(result.review.conversation_id.as_deref(), Some(CONV));
    assert!(result.review.reviewer_ref.starts_with("assistant:"));
}

#[tokio::test]
async fn owner_links_are_scoped_unique_and_removed_as_tombstones() {
    let mut env = env().await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    conversation(env.pool(), USER, "elsewhere", Some(PROJECT_B), "{}").await;
    let link = |conversation: &str, role| BoardLinkRequest {
        conversation_id: conversation.into(),
        role,
        department: None,
    };
    assert_eq!(
        code(
            env.service
                .add_link(&owner(), &card.id, link("elsewhere", LinkRole::Other))
                .await
                .unwrap_err()
        ),
        "board_not_found",
        "links point to a conversation of the same project"
    );
    env.service
        .add_link(&owner(), &card.id, link(CONV, LinkRole::Lead))
        .await
        .unwrap();
    conversation(env.pool(), USER, "second", Some(PROJECT), "{}").await;
    assert_eq!(
        code(
            env.service
                .add_link(&owner(), &card.id, link("second", LinkRole::Lead))
                .await
                .unwrap_err()
        ),
        "board_already_linked"
    );
    env.drain();
    env.service.remove_link(&owner(), &card.id, CONV).await.unwrap();
    assert_eq!(env.kinds(), ["link_removed"]);
    assert_eq!(
        code(env.service.remove_link(&owner(), &card.id, CONV).await.unwrap_err()),
        "board_not_found"
    );
}

fn assign_request(key: &str) -> BoardAssignRequest {
    BoardAssignRequest {
        assistant_id: Some(ASSISTANT.into()),
        team_id: None,
        department: Some("qa".into()),
        role: None,
        idempotency_key: key.into(),
    }
}

#[tokio::test]
async fn assign_creates_links_and_delivers_the_packet_once() {
    let mut env = env_with_host(true).await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    let card = env
        .service
        .update_card(
            &owner(),
            &card.id,
            UpdateBoardCardRequest {
                version: card.version,
                skills: Some(vec!["mermaid".into(), "not-installed".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    env.drain();
    let response = env
        .service
        .assign(&owner(), &card.id, assign_request("a1"))
        .await
        .unwrap();
    assert!(response.packet_sent);
    assert_eq!(
        (response.link.role, response.link.department.as_deref()),
        (LinkRole::Review, Some("qa"))
    );
    assert_eq!(response.card.assignee_ref.as_deref(), Some(ASSISTANT));
    assert_eq!(env.kinds(), ["card_assigned"]);
    let detail = env.service.card_detail(&owner(), &card.id).await.unwrap();
    assert_eq!(detail.assignee_skills, ["assistant:asst-1", "mermaid"]);
    let sent = env.host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.contains("<card-data>"));
    assert!(sent[0].1.contains("skills not installed (not enabled): not-installed"));
    let extra: String = sqlx::query_scalar("SELECT extra FROM conversations WHERE id = ?")
        .bind(&response.conversation_id)
        .fetch_one(env.pool())
        .await
        .unwrap();
    let extra: serde_json::Value = serde_json::from_str(&extra).unwrap();
    assert_eq!(extra["board_assignment_id"], response.assignment_id.as_str());
    assert_eq!(extra["preset_enabled_skills"], json!(["mermaid"]));

    // Replay: the stored response, nothing new.
    let replay = env
        .service
        .assign(&owner(), &card.id, assign_request("a1"))
        .await
        .unwrap();
    assert_eq!(replay.conversation_id, response.conversation_id);
    assert_eq!(env.host.sent.lock().unwrap().len(), 1);
    assert!(env.kinds().is_empty());
    let mut different = assign_request("a1");
    different.department = Some("arch".into());
    assert_eq!(
        code(env.service.assign(&owner(), &card.id, different).await.unwrap_err()),
        "board_idempotency_conflict"
    );
}

#[tokio::test]
async fn a_failed_packet_send_is_recorded_and_the_retry_resumes_without_a_second_conversation() {
    let mut env = env_with_host(true).await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    *env.host.fail_send.lock().unwrap() = true;
    env.drain();
    let first = env
        .service
        .assign(&owner(), &card.id, assign_request("a2"))
        .await
        .unwrap();
    assert!(!first.packet_sent);
    let messages = env.drain();
    let kinds: Vec<_> = messages
        .iter()
        .map(|m| m.data["event"]["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["card_assigned", "packet_send_failed"]);
    let failed = &messages[1].data["event"]["payload"];
    assert_eq!(failed["idempotency_key"], "a2");
    assert_eq!(failed["request"]["assistant_id"], ASSISTANT);
    assert_eq!(failed["request"]["department"], "qa");
    assert_eq!(failed["conversation_id"], first.conversation_id.as_str());

    *env.host.fail_send.lock().unwrap() = false;
    let second = env
        .service
        .assign(&owner(), &card.id, assign_request("a2"))
        .await
        .unwrap();
    assert!(second.packet_sent);
    assert_eq!(second.conversation_id, first.conversation_id, "resumed, not recreated");
    assert_eq!(
        env.kinds(),
        ["packet_sent"],
        "the link step is not repeated; the warning clears"
    );
    let conversations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE json_extract(extra, '$.board_assignment_id') = ?")
            .bind(&first.assignment_id)
            .fetch_one(env.pool())
            .await
            .unwrap();
    assert_eq!(conversations, 1);
    assert_eq!(env.host.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_live_reservation_blocks_a_concurrent_retry_and_a_stale_one_is_taken_over() {
    let env = env_with_host(true).await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    let request = assign_request("a3");
    // Simulate a claimant that reserved and is still working.
    let response = env.service.assign(&owner(), &card.id, request.clone()).await.unwrap();
    exec(
        env.pool(),
        "UPDATE board_idempotency SET state = 'pending', packet_message_id = NULL, \
         updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000",
    )
    .await;
    assert_eq!(
        code(
            env.service
                .assign(&owner(), &card.id, request.clone())
                .await
                .unwrap_err()
        ),
        "board_assignment_in_progress"
    );
    exec(env.pool(), "UPDATE board_idempotency SET updated_at = 1").await;
    let resumed = env.service.assign(&owner(), &card.id, request).await.unwrap();
    assert_eq!(resumed.conversation_id, response.conversation_id);
    assert_eq!(
        env.host.sent.lock().unwrap().len(),
        1,
        "the first user message exists: not sent twice"
    );
    let token: Option<String> = sqlx::query_scalar("SELECT claim_token FROM board_idempotency")
        .fetch_one(env.pool())
        .await
        .unwrap();
    assert!(token.is_some(), "the takeover issued a fencing token");
}

#[tokio::test]
async fn assign_nests_under_the_lead_or_reports_the_fallback() {
    let env = env_with_host(true).await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    env.service
        .add_link(
            &owner(),
            &card.id,
            BoardLinkRequest {
                conversation_id: CONV.into(),
                role: LinkRole::Lead,
                department: None,
            },
        )
        .await
        .unwrap();
    let nested = env
        .service
        .assign(&owner(), &card.id, assign_request("n1"))
        .await
        .unwrap();
    assert_eq!(nested.parent_fallback, None);
    let parent: Option<String> = sqlx::query_scalar("SELECT parent_conversation_id FROM conversations WHERE id = ?")
        .bind(&nested.conversation_id)
        .fetch_one(env.pool())
        .await
        .unwrap();
    assert_eq!(parent.as_deref(), Some(CONV));
    exec(env.pool(), "UPDATE conversations SET depth = 3 WHERE id = 'conv-1'").await;
    let mut deep = assign_request("n2");
    deep.department = Some("arch".into());
    let root = env.service.assign(&owner(), &card.id, deep).await.unwrap();
    assert_eq!(root.parent_fallback, Some(ParentFallback::Depth));
    let events = env.service.card_events(&owner(), &card.id, None, None).await.unwrap();
    let fallback = events.items.last().unwrap();
    assert_eq!(fallback.kind, "link_parent_fallback");
    assert_eq!(fallback.payload["reason"], "depth");
    assert_eq!(fallback.payload["conversation_id"], root.conversation_id.as_str());
}

#[tokio::test]
async fn team_assign_links_the_lead_and_checks_the_project() {
    let env = env_with_host(true).await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    exec(
        env.pool(),
        "INSERT INTO teams (id, user_id, name, project_id, created_at, updated_at) VALUES \
         ('team-1', 'user-1', 'T', 'proj-1', 1, 1), ('team-2', 'user-1', 'U', 'proj-2', 1, 1)",
    )
    .await;
    conversation(env.pool(), USER, "lead-1", Some(PROJECT), r#"{"teamId":"team-1"}"#).await;
    env.host.teams.lock().unwrap().extend([
        HostTeam {
            team_id: "team-1".into(),
            name: "T".into(),
            lead_conversation_id: Some("lead-1".into()),
        },
        HostTeam {
            team_id: "team-2".into(),
            name: "U".into(),
            lead_conversation_id: Some("lead-2".into()),
        },
    ]);
    let team = |team: &str, key: &str| BoardAssignRequest {
        assistant_id: None,
        team_id: Some(team.into()),
        department: Some("build".into()),
        role: None,
        idempotency_key: key.into(),
    };
    let error = env
        .service
        .assign(&owner(), &card.id, team("team-2", "t0"))
        .await
        .unwrap_err();
    assert_eq!((code(error.clone()), error.status()), ("board_wrong_project", 409));
    let response = env
        .service
        .assign(&owner(), &card.id, team("team-1", "t1"))
        .await
        .unwrap();
    assert_eq!(
        (response.link.role, response.conversation_id.as_str()),
        (LinkRole::Lead, "lead-1")
    );
    assert_eq!(response.card.assignee_kind, Some(aionui_api_types::AssigneeKind::Team));
    assert_eq!(env.host.team_messages.lock().unwrap().len(), 1);
    let error = env
        .service
        .assign(&owner(), &card.id, team("team-1", "t2"))
        .await
        .unwrap_err();
    assert_eq!(code(error), "board_already_linked", "the same team twice");
}

#[tokio::test]
async fn brainstorm_defaults_to_the_keeper_assistant() {
    let env = env_with_host(true).await;
    enable(&env.service, PROJECT).await;
    let card = env
        .service
        .create_card(&owner(), PROJECT, create("idea"))
        .await
        .unwrap();
    let response = env
        .service
        .brainstorm(&owner(), &card.id, BoardBrainstormRequest::default())
        .await
        .unwrap();
    assert_eq!(response.link.role, LinkRole::Brainstorm);
    assert_eq!(response.card.assignee_ref.as_deref(), Some(KEEPER_ASSISTANT));
}

#[tokio::test]
async fn enabling_creates_the_keeper_and_the_keeper_moves_between_non_gate_columns() {
    let env = env_with_host(true).await;
    let settings = enable(&env.service, PROJECT).await;
    let keeper = settings.keeper_conversation_id.expect("keeper created on enable");
    let name: String = sqlx::query_scalar("SELECT name FROM conversations WHERE id = ?")
        .bind(&keeper)
        .fetch_one(env.pool())
        .await
        .unwrap();
    assert_eq!(name, "Project proj-1 · Board");
    let caller = env.service.resolve_agent(USER, &keeper).await.unwrap();
    assert!(caller.keeper);
    assert!(env.service.agent_status(USER, &keeper).await.unwrap().keeper);
    let actor = BoardActor::Agent(caller);
    let card = env
        .service
        .create_card(&actor, PROJECT, create("from the owner's chat"))
        .await
        .unwrap();
    let card = env
        .service
        .move_card(&actor, &card.id, mv("triage", "ideas"))
        .await
        .unwrap();
    assert_eq!(card.column_key, "triage");
    assert_eq!(
        code(
            env.service
                .move_card(&actor, &card.id, mv("decision", "triage"))
                .await
                .unwrap_err()
        ),
        "board_agent_move_denied",
        "never into a gate"
    );
    assert_eq!(
        code(
            env.service
                .move_card(&actor, &card.id, forced("planned", "triage", Some("x")))
                .await
                .unwrap_err()
        ),
        "board_invalid",
        "never force"
    );
    // A department agent still cannot move.
    let department = BoardActor::Agent(env.service.resolve_agent(USER, CONV).await.unwrap());
    assert_eq!(
        code(
            env.service
                .move_card(&department, &card.id, mv("inbox", "triage"))
                .await
                .unwrap_err()
        ),
        "board_agent_move_denied"
    );
}

#[tokio::test]
async fn a_deleted_keeper_is_recreated_on_request() {
    let env = env_with_host(true).await;
    let settings = enable(&env.service, PROJECT).await;
    let keeper = settings.keeper_conversation_id.unwrap();
    let same = env.service.ensure_keeper(&owner(), PROJECT).await.unwrap();
    assert_eq!((same.conversation_id.as_str(), same.created), (keeper.as_str(), false));
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(&keeper)
        .execute(env.pool())
        .await
        .unwrap();
    let fresh = env.service.ensure_keeper(&owner(), PROJECT).await.unwrap();
    assert!(fresh.created);
    assert_ne!(fresh.conversation_id, keeper);
    let view = env.service.board_view(&owner(), PROJECT, false).await.unwrap();
    assert_eq!(
        view.settings.keeper_conversation_id.as_deref(),
        Some(fresh.conversation_id.as_str())
    );
    assert_eq!(
        code(
            env.service
                .ensure_keeper(&BoardActor::system(USER), PROJECT)
                .await
                .unwrap_err()
        ),
        "board_system_forbidden"
    );
}

#[tokio::test]
async fn re_enabling_a_board_recreates_a_missing_keeper() {
    let env = env_with_host(true).await;
    let settings = enable(&env.service, PROJECT).await;
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(settings.keeper_conversation_id.unwrap())
        .execute(env.pool())
        .await
        .unwrap();
    let off = env
        .service
        .update_settings(
            &owner(),
            PROJECT,
            UpdateBoardSettingsRequest {
                version: 1,
                enabled: Some(false),
                template: None,
                settings: None,
            },
        )
        .await
        .unwrap();
    let on = env
        .service
        .update_settings(
            &owner(),
            PROJECT,
            UpdateBoardSettingsRequest {
                version: off.version,
                enabled: Some(true),
                template: None,
                settings: None,
            },
        )
        .await
        .unwrap();
    let keeper = on.keeper_conversation_id.unwrap();
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = ?")
        .bind(&keeper)
        .fetch_one(env.pool())
        .await
        .unwrap();
    assert_eq!(exists, 1);
}
