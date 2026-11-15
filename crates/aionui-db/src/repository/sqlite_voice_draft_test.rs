use crate::init_database_memory;
use crate::models::VoiceDraftRow;
use crate::repository::sqlite_voice::SqliteVoiceRepository;
use crate::repository::voice::{CreateVoiceSessionParams, IVoiceSessionRepository};
use crate::repository::voice_draft::{DraftPatch, DraftTransition, IVoiceDraftRepository, NewVoiceTranscript};

const USER: &str = "draft-user";

async fn repo() -> (SqliteVoiceRepository, crate::Database) {
    let db = init_database_memory().await.unwrap();
    for user in [USER, "other-user"] {
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
            .bind(user)
            .bind(user)
            .execute(db.pool())
            .await
            .unwrap();
    }
    let repo = SqliteVoiceRepository::new(db.pool().clone());
    repo.create_starting(CreateVoiceSessionParams {
        id: "vs_1",
        user_id: USER,
        engine: "openai-realtime",
        model: "gpt-realtime-2.1",
        resume_of: None,
        device: None,
        instructions_hash: None,
    })
    .await
    .unwrap();
    (repo, db)
}

fn draft(id: &str, version: i64, state: &str) -> VoiceDraftRow {
    VoiceDraftRow {
        id: id.into(),
        version,
        user_id: USER.into(),
        session_id: Some("vs_1".into()),
        action: "send_message".into(),
        state: state.into(),
        queued_reason: None,
        target_id: Some("c1".into()),
        target_name: Some("Kanban build".into()),
        project_id: None,
        text: Some("rerun the tests".into()),
        payload_hash: "h".into(),
        readback_id: None,
        readback_text: None,
        spoken_confirm: false,
        delivery: None,
        hold_deadline_at: None,
        send_started_at: None,
        outcome_msg_id: None,
        outcome_turn_id: None,
        lease_owner: None,
        lease_until: None,
        expires_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn to<'a>(id: &'a str, version: i64, from: &'a [&'a str], state: &'a str) -> DraftTransition<'a> {
    DraftTransition {
        user_id: USER,
        id,
        version,
        from,
        to: Some(state),
        patch: DraftPatch::default(),
    }
}

#[tokio::test]
async fn versions_and_cas_transitions() {
    let (repo, _db) = repo().await;
    repo.insert_draft(&draft("d1", 1, "drafted")).await.unwrap();
    assert!(
        repo.insert_draft(&draft("d1", 1, "drafted"))
            .await
            .unwrap_err()
            .is_unique_violation()
    );

    // Wrong version or state never applies.
    assert!(
        !repo
            .transition_draft(&to("d1", 2, &["drafted"], "confirmed"))
            .await
            .unwrap()
    );
    assert!(
        !repo
            .transition_draft(&to("d1", 1, &["committing"], "sending"))
            .await
            .unwrap()
    );
    assert!(
        repo.transition_draft(&to("d1", 1, &["drafted"], "confirmed"))
            .await
            .unwrap()
    );

    // A field patch under the same CAS.
    let patch = DraftTransition {
        patch: DraftPatch {
            hold_deadline_at: Some(Some(42)),
            delivery: Some(Some("new_turn")),
            ..Default::default()
        },
        ..to("d1", 1, &["confirmed"], "committing")
    };
    assert!(repo.transition_draft(&patch).await.unwrap());
    let row = repo.get_draft(USER, "d1", 1).await.unwrap().unwrap();
    assert_eq!((row.state.as_str(), row.hold_deadline_at), ("committing", Some(42)));
    assert_eq!(row.delivery.as_deref(), Some("new_turn"));

    // Edits are new versions; latest wins.
    repo.insert_draft(&draft("d1", 2, "drafted")).await.unwrap();
    assert_eq!(repo.latest_draft(USER, "d1").await.unwrap().unwrap().version, 2);
    // Other users see nothing and change nothing.
    assert!(repo.get_draft("other-user", "d1", 1).await.unwrap().is_none());
    let foreign = DraftTransition {
        user_id: "other-user",
        ..to("d1", 2, &["drafted"], "cancelled")
    };
    assert!(!repo.transition_draft(&foreign).await.unwrap());
}

/// §3.3.1 race: cancel vs the send worker — exactly one wins, and a cancelled
/// draft can never be sent afterwards.
#[tokio::test]
async fn cancel_and_send_race_has_one_winner() {
    let (repo, _db) = repo().await;
    for round in 0..20 {
        let id = format!("race-{round}");
        repo.insert_draft(&draft(&id, 1, "committing")).await.unwrap();
        let cancellable = [
            "drafted",
            "ambiguous",
            "pending_transcript",
            "confirmed",
            "committing",
            "queued",
        ];
        let cancel = to(&id, 1, &cancellable, "cancelled");
        let send = to(&id, 1, &["committing", "queued"], "sending");
        let (c, s) = tokio::join!(repo.transition_draft(&cancel), repo.transition_draft(&send));
        assert!(c.unwrap() ^ s.unwrap(), "exactly one transition applies");
        let state = repo.get_draft(USER, &id, 1).await.unwrap().unwrap().state;
        if state == "cancelled" {
            assert!(!repo.transition_draft(&send).await.unwrap(), "cancel is final");
        }
    }
}

#[tokio::test]
async fn leases_states_tool_calls_and_purge() {
    let (repo, db) = repo().await;
    repo.insert_draft(&draft("d1", 1, "sending")).await.unwrap();
    repo.insert_draft(&draft("d2", 1, "sent")).await.unwrap();
    assert_eq!(repo.drafts_in_states(&["sending", "queued"]).await.unwrap().len(), 1);
    assert_eq!(repo.user_drafts_in_states(USER, &["sent"]).await.unwrap().len(), 1);
    assert!(
        repo.user_drafts_in_states("other-user", &["sent"])
            .await
            .unwrap()
            .is_empty()
    );

    assert!(
        repo.claim_draft_lease(USER, "d1", 1, "sending", "w1", 100, 10)
            .await
            .unwrap()
    );
    assert!(
        !repo
            .claim_draft_lease(USER, "d1", 1, "sending", "w2", 100, 50)
            .await
            .unwrap(),
        "held"
    );
    assert!(
        repo.claim_draft_lease(USER, "d1", 1, "sending", "w2", 200, 150)
            .await
            .unwrap(),
        "expired"
    );
    assert!(
        !repo
            .claim_draft_lease(USER, "d1", 1, "queued", "w3", 300, 400)
            .await
            .unwrap(),
        "wrong state"
    );

    assert!(repo.begin_tool_call("vs_1", "call_1", "read_file").await.unwrap());
    assert!(
        !repo.begin_tool_call("vs_1", "call_1", "read_file").await.unwrap(),
        "dedupe"
    );
    repo.finish_tool_call("vs_1", "call_1", "done", 12, 300).await.unwrap();

    repo.add_transcript(&NewVoiceTranscript {
        session_id: "vs_1",
        user_id: USER,
        turn_no: 1,
        role: "user",
        text: "tell the kanban build to rerun",
        expires_at: 1_000,
    })
    .await
    .unwrap();
    assert_eq!(
        repo.purge_voice_data(2_000, 5).await.unwrap(),
        3,
        "1 transcript + 2 draft texts"
    );
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM voice_transcripts")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
    let row = repo.get_draft(USER, "d1", 1).await.unwrap().unwrap();
    assert!(row.text.is_none() && row.payload_hash == "h", "text purged, hash kept");
}

#[tokio::test]
async fn recent_transcripts_and_session_rebind() {
    let (repo, _db) = repo().await;
    for (turn_no, role, text, expires_at) in [
        (1, "user", "expired turn", 1_000),
        (2, "user", "tell the kanban build to rerun", i64::MAX),
        (3, "assistant", "Sure.", i64::MAX),
        (4, "user", "thanks", i64::MAX),
    ] {
        repo.add_transcript(&NewVoiceTranscript {
            session_id: "vs_1",
            user_id: USER,
            turn_no,
            role,
            text,
            expires_at,
        })
        .await
        .unwrap();
    }
    assert_eq!(
        repo.recent_transcripts("vs_1", 2).await.unwrap(),
        vec![("assistant".into(), "Sure.".into()), ("user".into(), "thanks".into())],
        "last turns, oldest first"
    );
    assert_eq!(
        repo.recent_transcripts("vs_1", 10).await.unwrap().len(),
        3,
        "expired skipped"
    );
    assert!(repo.recent_transcripts("vs_other", 10).await.unwrap().is_empty());

    // A pending draft is re-bound to the next call under the same CAS.
    assert!(repo.finish(USER, "vs_1", "ended", "rotation").await.unwrap());
    repo.create_starting(CreateVoiceSessionParams {
        id: "vs_2",
        user_id: USER,
        engine: "openai-realtime",
        model: "gpt-realtime-2.1",
        resume_of: Some("vs_1"),
        device: None,
        instructions_hash: None,
    })
    .await
    .unwrap();
    repo.insert_draft(&draft("d1", 1, "drafted")).await.unwrap();
    let rebind = DraftTransition {
        to: None,
        patch: DraftPatch {
            session_id: Some(Some("vs_2")),
            ..Default::default()
        },
        ..to("d1", 1, &["drafted"], "drafted")
    };
    assert!(repo.transition_draft(&rebind).await.unwrap());
    let row = repo.get_draft(USER, "d1", 1).await.unwrap().unwrap();
    assert_eq!(
        (row.state.as_str(), row.session_id.as_deref()),
        ("drafted", Some("vs_2"))
    );
}
