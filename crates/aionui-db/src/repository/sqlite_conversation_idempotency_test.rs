//! Idempotent user-message inserts (`message_idempotency_keys`, migration 903).

use super::SqliteConversationRepository;
use crate::init_database_memory;
use crate::models::{ConversationRow, MessageRow};
use crate::repository::conversation::{IConversationRepository, IdempotentInsert, NewIdempotentSend};

const USER: &str = "system_default_user";

fn conversation(id: &str) -> ConversationRow {
    ConversationRow {
        id: id.into(),
        user_id: USER.into(),
        name: "Kanban build".into(),
        r#type: "acp".into(),
        extra: "{}".into(),
        model: None,
        status: Some("pending".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
        project_id: None,
        folder_id: None,
        name_source: None,
        parent_conversation_id: None,
        depth: 0,
    }
}

fn message(id: &str, conversation_id: &str) -> MessageRow {
    MessageRow {
        id: id.into(),
        conversation_id: conversation_id.into(),
        msg_id: Some(id.into()),
        r#type: "text".into(),
        content: r#"{"content":"hi"}"#.into(),
        position: Some("right".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 5,
        backend_turn_id: None,
    }
}

fn key<'a>(k: &'a str, turn: &'a str) -> NewIdempotentSend<'a> {
    NewIdempotentSend {
        idempotency_key: k,
        turn_id: turn,
        delivered_midturn: false,
    }
}

async fn count_messages(repo: &SqliteConversationRepository, conversation_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(&repo.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn keyed_insert_writes_once_and_returns_the_original() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    repo.create(&conversation("c1")).await.unwrap();

    let first = repo
        .insert_message_idempotent(USER, &message("m1", "c1"), &key("d1:1", "t1"))
        .await
        .unwrap();
    assert_eq!(first, IdempotentInsert::Inserted);

    let second = repo
        .insert_message_idempotent(USER, &message("m2", "c1"), &key("d1:1", "t2"))
        .await
        .unwrap();
    match second {
        IdempotentInsert::Existing(row) => {
            assert_eq!(row.msg_id, "m1");
            assert_eq!(row.turn_id, "t1");
        }
        other => panic!("expected the original ids, got {other:?}"),
    }
    assert_eq!(count_messages(&repo, "c1").await, 1, "the retry wrote nothing");

    let found = repo.find_idempotent_send(USER, "c1", "d1:1").await.unwrap().unwrap();
    assert_eq!(found.msg_id, "m1");
    assert!(
        repo.find_idempotent_send("other-user", "c1", "d1:1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(repo.find_idempotent_send(USER, "c1", "d1:2").await.unwrap().is_none());

    repo.update_idempotent_send_turn(USER, "c1", "d1:1", "t9", true)
        .await
        .unwrap();
    let found = repo.find_idempotent_send(USER, "c1", "d1:1").await.unwrap().unwrap();
    assert_eq!((found.turn_id.as_str(), found.delivered_midturn), ("t9", true));
}

#[tokio::test]
async fn concurrent_keyed_inserts_produce_one_row() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    repo.create(&conversation("c1")).await.unwrap();
    let (msg_a, msg_b) = (message("ma", "c1"), message("mb", "c1"));
    let (key_a, key_b) = (key("k", "ta"), key("k", "tb"));
    let a = repo.insert_message_idempotent(USER, &msg_a, &key_a);
    let b = repo.insert_message_idempotent(USER, &msg_b, &key_b);
    let (a, b) = tokio::join!(a, b);
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(
        outcomes.iter().filter(|o| **o == IdempotentInsert::Inserted).count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(count_messages(&repo, "c1").await, 1);
}

#[tokio::test]
async fn keyed_insert_into_foreign_conversation_is_not_found() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    repo.create(&conversation("c1")).await.unwrap();
    let err = repo
        .insert_message_idempotent("intruder", &message("m1", "c1"), &key("k", "t"))
        .await
        .unwrap_err();
    assert!(matches!(err, crate::DbError::NotFound(_)), "{err:?}");
    assert!(repo.find_idempotent_send(USER, "c1", "k").await.unwrap().is_none());
}
