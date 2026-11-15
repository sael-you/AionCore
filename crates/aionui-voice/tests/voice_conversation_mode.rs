//! Conversation voice mode (C26, D51) end to end against the fake provider:
//! admission and ownership, the transcribe-only call, send vs mid-turn add vs
//! queue, "stop" vs "stop the task", "stop voice", the bound conversation's
//! `say`, the typed-approval rule in the voice block, clip metering, and
//! restart behaviour.

mod common;

use std::time::Duration;

use aionui_db::{IVoiceDraftRepository, IVoiceSessionRepository};
use aionui_voice::{ConversationStatus, SendOutcome};
use common::*;
use serde_json::{Value, json};

struct Call {
    h: Harness,
    user: TestUser,
    session: String,
    call_id: String,
    ws: Client,
}

async fn create_conversation_session(h: &Harness, user: &TestUser, focus: Option<&str>) -> (u16, Value) {
    let mut body = json!({"sdp_offer": SDP, "device_id": "web", "mode": "conversation"});
    if let Some(focus) = focus {
        body["focus_conversation_id"] = json!(focus);
    }
    h.send(Req::browser("POST", "/api/voice/sessions", user).json(body).build())
        .await
}

async fn call() -> Call {
    let h = Harness::new(Options::default()).await;
    h.conversations.add("c-build", "Kanban build", Some("p-aionui"));
    h.conversations.add("c-spec", "Kanban spec", Some("p-aionui"));
    h.conversations.add_project("p-aionui", "aionui", None);
    let user = h.enabled_user("alice").await;
    let (status, body) = create_conversation_session(&h, &user, Some("c-build")).await;
    assert_eq!(status, 201, "{body}");
    let session = body["data"]["voice_session_id"].as_str().unwrap().to_owned();
    let row = wait_for(Duration::from_secs(5), || async {
        let row = h.session_row(&user, &session).await;
        row.ready_at.is_some().then_some(row)
    })
    .await
    .expect("call should become ready");
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;
    Call {
        h,
        user,
        session,
        call_id: row.call_id.unwrap(),
        ws,
    }
}

impl Call {
    fn say(&self, text: &str) {
        self.h.fake.push_event(
            &self.call_id,
            json!({"type": "conversation.item.input_audio_transcription.completed", "transcript": text,
                "usage": {"type": "duration", "seconds": 1}}),
        );
    }

    /// Next `voice.followUp` data.
    async fn follow_up(&mut self) -> Value {
        ws_expect(&mut self.ws, "voice.followUp").await["data"].clone()
    }

    fn set_status(&self, status: ConversationStatus) {
        self.h.conversations.set_status("c-build", status);
    }
}

fn running(can_steer: bool, approvals: usize) -> ConversationStatus {
    ConversationStatus {
        is_processing: true,
        pending_confirmations: approvals,
        active_turn_id: Some("turn-1".into()),
        can_steer,
    }
}

// ------------------------------------------------------------- admission

#[tokio::test]
async fn conversation_mode_needs_an_owned_conversation_and_opens_a_transcribe_only_call() {
    let h = Harness::new(Options::default()).await;
    h.conversations.add("c-build", "Kanban build", Some("p-aionui"));
    h.conversations.add_project("p-aionui", "aionui", None);
    let user = h.enabled_user("alice").await;

    let (status, body) = create_conversation_session(&h, &user, None).await;
    assert_eq!(status, 400, "focus required: {body}");
    // A conversation the user does not have (the port is user-scoped): 404.
    let (status, body) = create_conversation_session(&h, &user, Some("c-foreign")).await;
    assert_eq!(status, 404, "{body}");
    let (status, body) = h
        .send(
            Req::browser("POST", "/api/voice/sessions", &user)
                .json(json!({"sdp_offer": SDP, "mode": "dictation", "focus_conversation_id": "c-build"}))
                .build(),
        )
        .await;
    assert_eq!(status, 400, "unknown mode: {body}");
    assert!(
        h.fake.created_sessions().is_empty(),
        "no provider call for a refused admission"
    );

    let (status, body) = create_conversation_session(&h, &user, Some("c-build")).await;
    assert_eq!(status, 201, "{body}");
    let session = &h.fake.created_sessions()[0];
    assert_eq!(session["tools"], json!([]), "an ear only: no tools");
    assert_eq!(session["tool_choice"], "none");
    let vad = &session["audio"]["input"]["turn_detection"];
    assert_eq!(vad["create_response"], false, "the provider never answers");
    assert_eq!(vad["eagerness"], "low");
    let keywords = session["audio"]["input"]["transcription"]["keywords"].to_string();
    assert!(
        keywords.contains("Kanban build") && keywords.contains("aionui"),
        "{keywords}"
    );
}

#[tokio::test]
async fn jarvis_mode_is_unchanged() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (status, body) = create_session(&h, &user).await;
    assert_eq!(status, 201, "{body}");
    let session = &h.fake.created_sessions()[0];
    assert_ne!(session["tools"], json!([]), "Jarvis keeps its read tools");
    assert_eq!(session["audio"]["input"]["turn_detection"]["create_response"], true);
}

// ------------------------------------------------------------- messages

#[tokio::test]
async fn your_words_go_straight_in_as_a_new_turn_with_the_voice_block() {
    let mut c = call().await;
    c.say("Hey Jarvis, add a test for the 404 case");
    let outcome = c.follow_up().await;
    assert_eq!(outcome["status"], "sent", "{outcome}");
    assert_eq!(outcome["conversationId"], "c-build");
    assert!(outcome.get("clipId").is_none(), "outcomes are text only");

    let sends = c.h.conversations.sends();
    assert_eq!(sends.len(), 1);
    let text = &sends[0].text;
    assert!(
        text.starts_with("add a test for the 404 case\n\n[[AION_VOICE]]"),
        "{text}"
    );
    assert!(text.trim_end().ends_with("[[/AION_VOICE]]"));
    // Owner rule 5: a spoken yes never counts as a typed approval.
    assert!(text.contains("A spoken yes is never a typed approval"), "{text}");
    assert!(text.contains("voice say"));
    assert!(sends[0].key.ends_with(":1"));
    assert!(c.h.speech.lines().is_empty(), "no read-back, no draft");
}

#[tokio::test]
async fn while_the_agent_works_your_words_are_added_mid_turn() {
    let mut c = call().await;
    c.set_status(running(true, 0));
    c.h.conversations.push_outcome(SendOutcome::Delivered {
        msg_id: "m2".into(),
        turn_id: "turn-1".into(),
        midturn: true,
    });
    c.say("also check the mobile layout");
    let outcome = c.follow_up().await;
    assert_eq!(outcome["status"], "added", "{outcome}");
    assert!(
        c.h.conversations.state.lock().unwrap().cancelled.is_empty(),
        "never cancels the turn"
    );
}

#[tokio::test]
async fn an_open_approval_queues_the_message_and_the_worker_sends_it_once_even_after_a_restart() {
    let mut c = call().await;
    c.set_status(running(true, 1));
    c.say("use the staging database");
    let outcome = c.follow_up().await;
    assert_eq!(outcome["status"], "queued", "{outcome}");
    assert!(
        c.h.conversations.sends().is_empty(),
        "nothing sent while the approval is open"
    );
    let queued =
        c.h.voice_repo
            .user_drafts_in_states(&c.user.id, &["queued"])
            .await
            .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].delivery.as_deref(), Some("voice_mode"));

    // Restart: a new service over the same DB; the approval cleared.
    c.h.service.simulate_crash();
    let second = Harness::with_db(c.h.db.clone(), c.h.fake.clone(), Options::default()).await;
    second.conversations.add("c-build", "Kanban build", None);
    second.service.run_draft_sweep(true, aionui_common::now_ms()).await;
    second.service.run_draft_sweep(true, aionui_common::now_ms()).await;
    let sends = second.conversations.sends();
    assert_eq!(sends.len(), 1, "sent once: {sends:?}");
    assert!(sends[0].text.starts_with("use the staging database"));
    assert_eq!(sends[0].key, format!("{}:1", queued[0].id));
    let row = second
        .voice_repo
        .get_draft(&c.user.id, &queued[0].id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(row.state, "queued");
}

// ------------------------------------------------------------- intents

#[tokio::test]
async fn a_bare_stop_only_silences_and_stop_the_task_cancels_the_turn() {
    let mut c = call().await;
    c.set_status(running(true, 0));

    c.say("Stop.");
    let outcome = c.follow_up().await;
    assert_eq!(outcome["status"], "silenced", "{outcome}");
    assert!(
        c.h.conversations.state.lock().unwrap().cancelled.is_empty(),
        "the task keeps running"
    );
    assert!(c.h.conversations.sends().is_empty(), "a control is not posted");

    c.say("stop the task");
    let outcome = c.follow_up().await;
    assert_eq!(outcome["status"], "stopped", "{outcome}");
    assert_eq!(outcome["line"], "Stopped.");
    assert_eq!(
        c.h.conversations.state.lock().unwrap().cancelled,
        vec!["c-build".to_owned()]
    );

    c.set_status(running(true, 0));
    c.say("Cancel that.");
    assert_eq!(c.follow_up().await["status"], "stopped");
    assert_eq!(c.h.conversations.state.lock().unwrap().cancelled.len(), 2);
    assert!(c.h.conversations.sends().is_empty());

    // A sentence that merely contains "stop" is a message.
    c.say("stop the build after the tests pass");
    assert_eq!(c.follow_up().await["status"], "sent");
}

#[tokio::test]
async fn stop_voice_ends_the_call_as_sleep() {
    let c = call().await;
    c.say("stop voice");
    let row = c.h.wait_ended(&c.user, &c.session).await;
    assert_eq!(row.end_reason.as_deref(), Some("sleep"));
    assert!(c.h.conversations.sends().is_empty());
}

// ------------------------------------------------------------- agent speech

#[tokio::test]
async fn the_bound_conversation_speaks_without_jarvis_restrictions_and_others_stay_restricted() {
    let c = call().await;
    // Bound: no voice send needed, no name prefix, confirm words allowed, > 500 chars split.
    let long = format!("Yes, the tests pass. {}", "The upload test was slow. ".repeat(30));
    assert!(long.chars().count() > 500);
    let said = c.h.service.agent_say(&c.user.id, "c-build", &long).await.unwrap();
    assert_eq!(said.status, "delivered");
    let lines = wait_for(Duration::from_secs(5), || async {
        let lines = c.h.speech.lines();
        (lines.len() >= 3).then_some(lines)
    })
    .await
    .expect("split into clips");
    assert!(lines[0].starts_with("Yes, the tests pass."), "{lines:?}");
    assert!(lines.iter().all(|l| !l.contains("says:") && l.chars().count() <= 300));

    // Unbound conversation: the C9 rules still apply.
    assert!(
        c.h.service
            .agent_say(&c.user.id, "c-spec", "tests are green")
            .await
            .is_err(),
        "no voice send to that conversation"
    );
    assert!(c.h.service.is_bound(&c.user.id, "c-build"));
    assert!(!c.h.service.is_bound(&c.user.id, "c-spec"));
    assert!(!c.h.service.is_bound("bob", "c-build"), "bound per user");
}

#[tokio::test]
async fn ending_voice_mode_unbinds_the_conversation() {
    let c = call().await;
    c.h.service
        .end_session(&c.user.id, &c.session, aionui_voice::EndReason::User)
        .await
        .unwrap();
    assert!(!c.h.service.is_bound(&c.user.id, "c-build"));
    assert!(c.h.service.agent_say(&c.user.id, "c-build", "done").await.is_err());
}

#[tokio::test]
async fn spoken_clips_are_charged_to_the_daily_budget() {
    let c = call().await;
    let before = c.h.session_row(&c.user, &c.session).await.cost_micros;
    let text = "The build is green again.";
    c.h.service.agent_say(&c.user.id, "c-build", text).await.unwrap();
    let after = wait_for(Duration::from_secs(5), || async {
        let cost = c.h.session_row(&c.user, &c.session).await.cost_micros;
        (cost > before).then_some(cost)
    })
    .await
    .expect("clip charged");
    assert_eq!(after - before, 60 * text.chars().count() as i64);
    let today =
        c.h.voice_repo
            .cost_since(&c.user.id, aionui_common::now_ms() - 86_400_000)
            .await
            .unwrap();
    assert!(today >= after);
}

#[tokio::test]
async fn a_restart_hangs_a_conversation_call_up_instead_of_reattaching_it_as_jarvis() {
    let c = call().await;
    c.h.service.simulate_crash();
    let second = Harness::with_db(c.h.db.clone(), c.h.fake.clone(), Options::default()).await;
    assert_eq!(second.service.recover_open_sessions().await, 0);
    let row = second.session_row(&c.user, &c.session).await;
    assert_eq!(row.end_reason.as_deref(), Some("restart"));
    assert!(
        !second.service.is_bound(&c.user.id, "c-build"),
        "the binding is in memory only"
    );
}

#[tokio::test]
async fn turn_end_is_spoken_only_when_the_agent_said_nothing() {
    use aionui_realtime::EventBroadcaster;
    let mut c = call().await;
    let bus = std::sync::Arc::new(aionui_realtime::BroadcastEventBus::new(64));
    c.h.service.start_background(bus.clone());
    let completed = |turn: &str, outcome: &str| {
        aionui_api_types::WebSocketMessage::new(
            "turn.completed",
            json!({"user_id": "alice", "conversation_id": "c-build", "turn_id": turn,
                   "status": "finished", "last_outcome": outcome}),
        )
    };

    // The agent spoke this turn: the end is a text cue only.
    c.h.service
        .agent_say(&c.user.id, "c-build", "All green.")
        .await
        .unwrap();
    let said = c.follow_up().await;
    assert_eq!(said["status"], "says");
    bus.broadcast(completed("t1", "finished"));
    let end = c.follow_up().await;
    assert_eq!(end["status"], "finished");
    assert!(end.get("clipId").is_none(), "{end}");

    // It forgot to speak: "<name> finished." is spoken.
    bus.broadcast(completed("t2", "finished"));
    let end = c.follow_up().await;
    assert_eq!(end["line"], "Kanban build finished.");
    assert!(end["clipId"].is_string(), "{end}");

    // A cancelled turn was already answered with "Stopped.": text only.
    bus.broadcast(completed("t3", "cancelled"));
    let end = c.follow_up().await;
    assert!(end.get("clipId").is_none(), "{end}");
}
