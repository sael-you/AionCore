//! Board list summaries carry the Board Butler's progress keys of `metadata`
//! (`progress`, `status_note`, `updated_at`) on the board view and the paged
//! card list, and nothing else of it.

mod common;

use aionui_api_types::{BoardCardListQuery, UpdateBoardCardRequest};
use common::*;
use serde_json::json;

#[tokio::test]
async fn board_view_and_card_list_carry_only_the_progress_metadata() {
    let env = env().await;
    enable(&env.service, PROJECT).await;
    let tracked = env
        .service
        .create_card(&owner(), PROJECT, create("tracked"))
        .await
        .unwrap();
    let plain = env
        .service
        .create_card(&owner(), PROJECT, create("plain"))
        .await
        .unwrap();
    env.service
        .update_card(
            &owner(),
            &tracked.id,
            UpdateBoardCardRequest {
                version: tracked.version,
                metadata: Some(json!({
                    "progress": 60,
                    "status_note": "clippy green, tests running",
                    "updated_at": 1_763_200_000_000_i64,
                    "packet": {"brief": "large text"},
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    env.service
        .update_card(
            &owner(),
            &plain.id,
            UpdateBoardCardRequest {
                version: plain.version,
                metadata: Some(json!({"packet": {"brief": "no progress"}})),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let expected = json!({
        "progress": 60,
        "status_note": "clippy green, tests running",
        "updated_at": 1_763_200_000_000_i64,
    });

    let view = env.service.board_view(&owner(), PROJECT, false).await.unwrap();
    let list = env
        .service
        .list_cards(USER, PROJECT, &BoardCardListQuery::default(), None)
        .await
        .unwrap();
    for (source, cards) in [("board view", &view.cards), ("card list", &list.items)] {
        let find = |id: &str| cards.iter().find(|card| card.id == id).unwrap();
        assert_eq!(find(&tracked.id).metadata, Some(expected.clone()), "{source}");
        assert_eq!(find(&plain.id).metadata, None, "{source}");
        let wire = serde_json::to_value(find(&plain.id)).unwrap();
        assert!(wire.get("metadata").is_none(), "{source}: no progress, no field");
    }
}
