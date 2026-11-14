//! `SessionChildResultDelivery`: a finished child's report reaches its parent
//! through the cross-session message path, honouring the feature toggle, and
//! never loops back onto the sender.

mod common;

use aionui_common::ChildResultDelivery;
use aionui_session_message::SessionChildResultDelivery;
use common::{USER, setup};

const REPORT: &str =
    "[[AION_CHILD_RESULT child_id=conv_child name=Research status=finished]]\nall done\n[[/AION_CHILD_RESULT]]";

#[tokio::test]
async fn the_report_arrives_in_the_parent_as_a_cross_session_message_from_the_child() {
    let ctx = setup().await;
    ctx.create_conversation("conv_parent", "Parent", "/w/a").await;
    ctx.create_conversation("conv_child", "Research", "/w/a").await;
    let delivery = SessionChildResultDelivery::new(ctx.service.clone());

    delivery
        .deliver_child_result(USER, "conv_child", "conv_parent", REPORT.to_owned())
        .await
        .expect("delivery succeeds");

    let content = ctx.last_user_message_content("conv_parent").await;
    assert!(content.starts_with("[[AION_SESSION_MESSAGE]]"), "{content}");
    assert!(content.contains("from: Research\tconv_child"), "{content}");
    assert!(content.contains(REPORT), "{content}");
    assert_eq!(
        ctx.user_message_count("conv_child").await,
        0,
        "reports only flow child to parent"
    );
}

#[tokio::test]
async fn the_cross_session_toggle_suppresses_the_report() {
    let ctx = setup().await;
    ctx.create_conversation("conv_parent", "Parent", "/w/a").await;
    ctx.create_conversation("conv_child", "Research", "/w/a").await;
    ctx.disable_feature(USER).await;
    let delivery = SessionChildResultDelivery::new(ctx.service.clone());

    let err = delivery
        .deliver_child_result(USER, "conv_child", "conv_parent", REPORT.to_owned())
        .await
        .unwrap_err();

    assert!(err.contains("disabled"), "{err}");
    assert_eq!(ctx.user_message_count("conv_parent").await, 0);
}

#[tokio::test]
async fn a_self_addressed_report_is_refused_as_a_loop() {
    let ctx = setup().await;
    ctx.create_conversation("conv_child", "Research", "/w/a").await;
    let delivery = SessionChildResultDelivery::new(ctx.service.clone());

    let err = delivery
        .deliver_child_result(USER, "conv_child", "conv_child", REPORT.to_owned())
        .await
        .unwrap_err();

    assert!(err.contains("same conversation"), "{err}");
}
