//! Hierarchical conversations on `ConversationService`: parent validation on
//! create, reparent (cycle / fanout / depth / depth rewrite), and the delete
//! cascade.

use super::*;
use crate::hierarchy::{HierarchyViolation, MAX_CHILDREN_PER_PARENT, MAX_DEPTH};

const U: &str = "user_1";

fn req_with_parent(parent: Option<&str>) -> CreateConversationRequest {
    let mut req = make_create_req();
    req.parent_conversation_id = parent.map(str::to_owned);
    req
}

async fn make_child(svc: &ConversationService, parent: Option<&str>) -> ConversationResponse {
    svc.create(U, req_with_parent(parent)).await.unwrap()
}

fn violation_of(error: ConversationError) -> HierarchyViolation {
    match error {
        ConversationError::Hierarchy { violation, .. } => violation,
        other => panic!("expected a hierarchy violation, got {other:?}"),
    }
}

#[tokio::test]
async fn a_root_has_no_parent_and_depth_zero_and_a_child_is_one_deeper() {
    let (svc, _b, repo, _t) = make_service();
    let root = make_child(&svc, None).await;
    assert_eq!((root.parent_conversation_id.clone(), root.depth), (None, 0));

    let child = make_child(&svc, Some(&root.id)).await;
    assert_eq!(child.parent_conversation_id.as_deref(), Some(root.id.as_str()));
    assert_eq!(child.depth, 1);
    let row = repo.get(U, &child.id).await.unwrap().unwrap();
    assert_eq!(
        (row.parent_conversation_id.as_deref(), row.depth),
        (Some(root.id.as_str()), 1)
    );
}

#[tokio::test]
async fn create_rejects_a_missing_or_foreign_parent_with_parent_not_found() {
    let (svc, _b, _repo, _t) = make_service();
    let err = svc.create(U, req_with_parent(Some("nope"))).await.unwrap_err();
    assert_eq!(violation_of(err), HierarchyViolation::ParentNotFound);

    let other = svc.create("user_2", make_create_req()).await.unwrap();
    let err = svc.create(U, req_with_parent(Some(&other.id))).await.unwrap_err();
    assert_eq!(
        violation_of(err),
        HierarchyViolation::ParentNotFound,
        "another user's id"
    );
}

#[tokio::test]
async fn create_rejects_a_team_member_parent() {
    let (svc, _b, repo, _t) = make_service();
    let member = make_child(&svc, None).await;
    let mut row = repo.get(U, &member.id).await.unwrap().unwrap();
    let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    extra["teamId"] = json!("team_1");
    row.extra = extra.to_string();
    repo.rows.lock().unwrap().retain(|r| r.id != member.id);
    repo.create(&row).await.unwrap();

    let err = svc.create(U, req_with_parent(Some(&member.id))).await.unwrap_err();
    assert_eq!(violation_of(err), HierarchyViolation::ParentNotFound);
}

#[tokio::test]
async fn create_enforces_max_depth() {
    let (svc, _b, _repo, _t) = make_service();
    let mut parent = make_child(&svc, None).await;
    for expected in 1..=MAX_DEPTH {
        parent = make_child(&svc, Some(&parent.id)).await;
        assert_eq!(parent.depth, expected);
    }
    let err = svc.create(U, req_with_parent(Some(&parent.id))).await.unwrap_err();
    assert_eq!(violation_of(err), HierarchyViolation::ParentDepthExceeded);
}

#[tokio::test]
async fn create_enforces_max_children_per_parent() {
    let (svc, _b, _repo, _t) = make_service();
    let root = make_child(&svc, None).await;
    for _ in 0..MAX_CHILDREN_PER_PARENT {
        make_child(&svc, Some(&root.id)).await;
    }
    let err = svc.create(U, req_with_parent(Some(&root.id))).await.unwrap_err();
    assert_eq!(violation_of(err), HierarchyViolation::ParentFanoutExceeded);
}

#[tokio::test]
async fn reparent_rejects_self_and_descendant_as_parent_with_parent_cycle() {
    let (svc, _b, _repo, _t) = make_service();
    let a = make_child(&svc, None).await;
    let b = make_child(&svc, Some(&a.id)).await;
    let c = make_child(&svc, Some(&b.id)).await;

    let err = svc.reparent(U, &a.id, Some(&a.id)).await.unwrap_err();
    assert_eq!(violation_of(err), HierarchyViolation::ParentCycle);
    let err = svc.reparent(U, &a.id, Some(&c.id)).await.unwrap_err();
    assert_eq!(violation_of(err), HierarchyViolation::ParentCycle);
}

#[tokio::test]
async fn reparent_rewrites_the_depth_of_the_whole_subtree_and_broadcasts() {
    let (svc, broadcaster, repo, _t) = make_service();
    let r1 = make_child(&svc, None).await;
    let r2 = make_child(&svc, None).await;
    let a = make_child(&svc, Some(&r1.id)).await;
    let b = make_child(&svc, Some(&a.id)).await;
    broadcaster.take_events();

    // r2 (depth 0) -> a(1) -> b(2) keeps the same depths; move under a depth-1
    // parent instead to force a rewrite: r2's own child.
    let r2_kid = make_child(&svc, Some(&r2.id)).await;
    let moved = svc.reparent(U, &a.id, Some(&r2_kid.id)).await.unwrap();
    assert_eq!(moved.parent_conversation_id.as_deref(), Some(r2_kid.id.as_str()));
    assert_eq!(moved.depth, 2);
    assert_eq!(repo.get(U, &b.id).await.unwrap().unwrap().depth, 3);

    let events = broadcaster.take_events();
    assert!(
        events
            .iter()
            .any(|e| e.name == "conversation.listChanged" && e.data["action"] == "updated"),
        "reparent must broadcast list-changed"
    );

    // null detaches to a root and pulls the subtree up.
    let detached = svc.reparent(U, &a.id, None).await.unwrap();
    assert_eq!((detached.parent_conversation_id, detached.depth), (None, 0));
    assert_eq!(repo.get(U, &b.id).await.unwrap().unwrap().depth, 1);
}

#[tokio::test]
async fn reparent_rejects_a_move_that_overflows_max_depth() {
    let (svc, _b, _repo, _t) = make_service();
    // chain x(0) -> y(1) -> z(2)
    let x = make_child(&svc, None).await;
    let y = make_child(&svc, Some(&x.id)).await;
    let z = make_child(&svc, Some(&y.id)).await;
    // target chain p(0) -> q(1) -> r(2)
    let p = make_child(&svc, None).await;
    let q = make_child(&svc, Some(&p.id)).await;
    let r = make_child(&svc, Some(&q.id)).await;
    let _ = z;
    // Moving x (subtree height 2) under r (depth 2) => x=3, y=4, z=5.
    let err = svc.reparent(U, &x.id, Some(&r.id)).await.unwrap_err();
    assert_eq!(violation_of(err), HierarchyViolation::ParentDepthExceeded);
}

#[tokio::test]
async fn reparent_rejects_a_full_target_parent() {
    let (svc, _b, _repo, _t) = make_service();
    let full = make_child(&svc, None).await;
    for _ in 0..MAX_CHILDREN_PER_PARENT {
        make_child(&svc, Some(&full.id)).await;
    }
    let loose = make_child(&svc, None).await;
    let err = svc.reparent(U, &loose.id, Some(&full.id)).await.unwrap_err();
    assert_eq!(violation_of(err), HierarchyViolation::ParentFanoutExceeded);
}

#[tokio::test]
async fn reparent_of_an_unknown_conversation_is_not_found() {
    let (svc, _b, _repo, _t) = make_service();
    let err = svc.reparent(U, "nope", None).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }), "{err:?}");
}

#[tokio::test]
async fn deleting_a_parent_deletes_every_descendant_deepest_first() {
    let (svc, broadcaster, repo, _t) = make_service();
    let root = make_child(&svc, None).await;
    let a = make_child(&svc, Some(&root.id)).await;
    let b = make_child(&svc, Some(&root.id)).await;
    let a1 = make_child(&svc, Some(&a.id)).await;
    let bystander = make_child(&svc, None).await;
    broadcaster.take_events();

    svc.delete(U, &root.id).await.unwrap();

    for gone in [&root.id, &a.id, &b.id, &a1.id] {
        assert!(repo.get(U, gone).await.unwrap().is_none(), "{gone} should be deleted");
    }
    assert!(repo.get(U, &bystander.id).await.unwrap().is_some());

    // One `deleted` event per conversation; the grandchild goes before its
    // parent, and the requested root goes last.
    let order: Vec<String> = broadcaster
        .take_events()
        .iter()
        .filter(|e| e.name == "conversation.listChanged" && e.data["action"] == "deleted")
        .map(|e| e.data["conversation_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(order.len(), 4);
    let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
    assert!(pos(&a1.id) < pos(&a.id), "{order:?}");
    assert_eq!(order.last().unwrap(), &root.id, "{order:?}");
}

// ── Child result reports ───────────────────────────────────────────

#[derive(Default)]
struct RecordingDelivery {
    sent: std::sync::Mutex<Vec<(String, String, String)>>,
    fail: bool,
}

#[async_trait::async_trait]
impl aionui_common::ChildResultDelivery for RecordingDelivery {
    async fn deliver_child_result(
        &self,
        _user_id: &str,
        child_conversation_id: &str,
        parent_conversation_id: &str,
        message: String,
    ) -> Result<(), String> {
        self.sent.lock().unwrap().push((
            child_conversation_id.to_owned(),
            parent_conversation_id.to_owned(),
            message,
        ));
        if self.fail {
            Err("rate_limited".to_owned())
        } else {
            Ok(())
        }
    }
}

fn text_message(conv: &str, id: &str, position: &str, text: &str, at: i64) -> MessageRow {
    MessageRow {
        id: id.to_owned(),
        conversation_id: conv.to_owned(),
        msg_id: None,
        r#type: "text".to_owned(),
        content: json!({ "content": text }).to_string(),
        position: Some(position.to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: at,
        backend_turn_id: None,
    }
}

async fn child_with_transcript(repo: &Arc<MockRepo>, extra: serde_json::Value, parent: Option<&str>) -> String {
    let id = "child-1".to_owned();
    repo.create(&ConversationRow {
        id: id.clone(),
        user_id: U.to_owned(),
        name: "Research".to_owned(),
        r#type: "acp".to_owned(),
        extra: extra.to_string(),
        model: None,
        status: Some("finished".to_owned()),
        source: Some("aionui".to_owned()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
        project_id: None,
        folder_id: None,
        name_source: None,
        parent_conversation_id: parent.map(str::to_owned),
        depth: i64::from(parent.is_some()),
    })
    .await
    .unwrap();
    repo.insert_message(U, &text_message(&id, "m1", "right", "go research", 10))
        .await
        .unwrap();
    repo.insert_message(U, &text_message(&id, "m2", "left", "the final findings", 20))
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn a_finished_child_reports_its_final_text_to_its_parent() {
    let (_svc, _b, repo, _t) = make_service();
    let child = child_with_transcript(&repo, json!({ "report_to_parent": true }), Some("parent-1")).await;
    let delivery = Arc::new(RecordingDelivery::default());

    crate::child_report::report_child_result(
        repo.clone(),
        delivery.clone(),
        U.to_owned(),
        child.clone(),
        crate::hierarchy::ChildResultStatus::Finished,
    )
    .await;

    let sent = delivery.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, child);
    assert_eq!(sent[0].1, "parent-1");
    assert_eq!(
        sent[0].2,
        "[[AION_CHILD_RESULT child_id=child-1 name=Research status=finished]]\nthe final findings\n[[/AION_CHILD_RESULT]]"
    );
}

#[tokio::test]
async fn report_to_parent_false_roots_and_missing_flag_defaults() {
    let (_svc, _b, repo, _t) = make_service();
    let delivery = Arc::new(RecordingDelivery::default());
    let run = |repo: Arc<MockRepo>, delivery: Arc<RecordingDelivery>, id: String| {
        crate::child_report::report_child_result(
            repo,
            delivery,
            U.to_owned(),
            id,
            crate::hierarchy::ChildResultStatus::Error,
        )
    };

    // Opt-out: nothing is sent.
    let id = child_with_transcript(&repo, json!({ "report_to_parent": false }), Some("parent-1")).await;
    run(repo.clone(), delivery.clone(), id).await;
    assert!(delivery.sent.lock().unwrap().is_empty());

    // A root never reports.
    repo.rows.lock().unwrap().clear();
    let id = child_with_transcript(&repo, json!({}), None).await;
    run(repo.clone(), delivery.clone(), id).await;
    assert!(delivery.sent.lock().unwrap().is_empty());

    // No flag at all defaults to reporting, with the status carried through.
    repo.rows.lock().unwrap().clear();
    repo.messages.lock().unwrap().clear();
    let id = child_with_transcript(&repo, json!({}), Some("parent-1")).await;
    run(repo.clone(), delivery.clone(), id).await;
    let sent = delivery.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].2.contains("status=error]]"), "{}", sent[0].2);
}

#[tokio::test]
async fn a_failing_delivery_is_swallowed() {
    let (_svc, _b, repo, _t) = make_service();
    let id = child_with_transcript(&repo, json!({}), Some("parent-1")).await;
    let delivery = Arc::new(RecordingDelivery {
        fail: true,
        ..Default::default()
    });

    // Must return normally: delivery trouble never fails turn completion.
    crate::child_report::report_child_result(
        repo.clone(),
        delivery.clone(),
        U.to_owned(),
        id,
        crate::hierarchy::ChildResultStatus::Cancelled,
    )
    .await;

    assert_eq!(delivery.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_flat_list_returns_children_with_parent_and_depth() {
    let (svc, _b, _repo, _t) = make_service();
    let root = make_child(&svc, None).await;
    let kid = make_child(&svc, Some(&root.id)).await;
    let grand = make_child(&svc, Some(&kid.id)).await;

    let list = svc
        .list(U, aionui_api_types::ListConversationsQuery::default())
        .await
        .unwrap();

    let find = |id: &str| {
        list.items
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("{id} missing from list"))
    };
    assert_eq!(
        (find(&root.id).parent_conversation_id.clone(), find(&root.id).depth),
        (None, 0)
    );
    assert_eq!(find(&kid.id).parent_conversation_id.as_deref(), Some(root.id.as_str()));
    assert_eq!(find(&kid.id).depth, 1);
    assert_eq!(find(&grand.id).parent_conversation_id.as_deref(), Some(kid.id.as_str()));
    assert_eq!(find(&grand.id).depth, 2);
    let json = serde_json::to_value(find(&grand.id)).unwrap();
    assert_eq!(json["depth"], serde_json::json!(2));
    assert_eq!(json["parent_conversation_id"], serde_json::json!(kid.id));
}
