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

#[tokio::test]
async fn the_owners_polite_end_voice_ends_the_call_without_posting() {
    let c = call().await;
    c.say("you can stop the voice experience, please");
    let row = c.h.wait_ended(&c.user, &c.session).await;
    assert_eq!(row.end_reason.as_deref(), Some("sleep"));
    assert!(c.h.conversations.sends().is_empty(), "a control, never a message");
}

#[tokio::test]
async fn a_task_then_stop_voice_posts_the_task_and_ends_voice_mode() {
    let c = call().await;
    c.say("Okay, fix this and stop voice.");
    let row = c.h.wait_ended(&c.user, &c.session).await;
    assert_eq!(row.end_reason.as_deref(), Some("sleep"));
    let sends = c.h.conversations.sends();
    assert_eq!(sends.len(), 1, "the task part is posted once");
    assert!(
        sends[0].text.starts_with("Okay, fix this\n\n[[AION_VOICE]]"),
        "{}",
        sends[0].text
    );
    assert!(!sends[0].text.contains("stop voice"));
    assert!(!c.h.service.is_bound(&c.user.id, "c-build"), "voice mode ended");
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

    // A failed TURN is `turn_failed` (`failed` is a failed send), spoken.
    bus.broadcast(completed("t4", "error"));
    let end = c.follow_up().await;
    assert_eq!(end["status"], "turn_failed", "{end}");
    assert_eq!(end["line"], "Kanban build failed.");
    assert!(end["clipId"].is_string());
}

async fn conversation_harness_with(options: Options) -> (Harness, TestUser, String) {
    let h = Harness::new(options).await;
    h.conversations.add("c-build", "Kanban build", Some("p-aionui"));
    let user = h.enabled_user("alice").await;
    let (status, body) = create_conversation_session(&h, &user, Some("c-build")).await;
    assert_eq!(status, 201, "{body}");
    let session = body["data"]["voice_session_id"].as_str().unwrap().to_owned();
    (h, user, session)
}

#[tokio::test]
async fn after_the_idle_hangup_voice_mode_stays_armed_and_the_agent_may_still_speak() {
    let timings = aionui_voice::VoiceTimings {
        idle_hangup: Duration::from_millis(600),
        heartbeat_stale: Duration::from_secs(60),
        ..fast_timings()
    };
    let (h, user, session) = conversation_harness_with(Options { timings }).await;
    // A running turn keeps the call open past the idle limit.
    h.conversations.set_status("c-build", running(true, 0));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        h.session_row(&user, &session).await.state,
        "live",
        "a running turn is not idle"
    );

    h.conversations.set_status("c-build", ConversationStatus::default());
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("idle"));
    assert!(
        h.service.is_bound(&user.id, "c-build"),
        "the idle hangup keeps voice mode bound"
    );
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    let said = h
        .service
        .agent_say(&user.id, "c-build", "Done, all green.")
        .await
        .unwrap();
    assert_eq!(said.status, "delivered");
    let wake = ws_expect(&mut ws, "voice.wake").await;
    assert_eq!(wake["data"]["reason"], "says", "announced without a call: {wake}");
}

#[tokio::test]
async fn a_restart_during_a_bound_turn_is_spoken_once_on_the_next_hello() {
    let c = call().await;
    c.say("refactor the upload test");
    wait_for(Duration::from_secs(5), || async {
        (!c.h.conversations.sends().is_empty()).then_some(())
    })
    .await
    .expect("sent");
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.h.service.simulate_crash();

    let second = Harness::with_db(c.h.db.clone(), c.h.fake.clone(), Options::default()).await;
    second.conversations.add("c-build", "Kanban build", None);
    assert_eq!(second.service.recover_open_sessions().await, 0);
    let addr = second.serve().await;
    let mut ws = ws_connect(&addr, &c.user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    let line = ws_expect(&mut ws, "voice.followUp").await["data"].clone();
    assert_eq!(line["status"], "interrupted", "{line}");
    assert_eq!(line["conversationId"], "c-build");
    assert!(line["line"].as_str().unwrap().contains("restart interrupted the task"));
    assert!(line["clipId"].is_string(), "spoken through the normal TTS path");

    // Once only.
    let mut again = ws_connect(&addr, &c.user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut again, hello(None)).await;
    assert!(!saw_follow_up(&mut again).await, "spoken once");
}

#[tokio::test]
async fn a_restart_after_the_turn_ended_says_nothing() {
    let c = call().await;
    // No voice message in this call: nothing was interrupted.
    c.h.service.simulate_crash();
    let second = Harness::with_db(c.h.db.clone(), c.h.fake.clone(), Options::default()).await;
    second.conversations.add("c-build", "Kanban build", None);
    second.service.recover_open_sessions().await;
    let addr = second.serve().await;
    let mut ws = ws_connect(&addr, &c.user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    assert!(!saw_follow_up(&mut ws).await);
}

/// Whether a `voice.followUp` arrives within 800 ms.
async fn saw_follow_up(ws: &mut Client) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
    while let Some(frame) = ws_next(ws, deadline.saturating_duration_since(tokio::time::Instant::now())).await {
        if frame["name"] == "voice.followUp" {
            return true;
        }
    }
    false
}

// ------------------------------------------------------------- review fixes

async fn set_budget(h: &Harness, user: &TestUser, usd: f64) {
    h.service
        .update_settings(
            &user.id,
            &aionui_api_types::UpdateVoiceSettingsRequest {
                daily_budget_usd: Some(usd),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn speech_past_the_daily_cap_is_refused() {
    let c = call().await;
    set_budget(&c.h, &c.user, 0.001).await;
    let refused = c.h.service.agent_say(&c.user.id, "c-build", &"word ".repeat(40)).await;
    assert!(
        matches!(refused, Err(aionui_voice::VoiceError::BudgetReached)),
        "{refused:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(c.h.speech.lines().is_empty(), "nothing synthesized past the cap");
}

#[tokio::test]
async fn spoken_clips_feed_the_live_meter_and_the_call_hangs_up_at_the_cap() {
    let h = Harness::new(Options::default()).await;
    h.conversations.add("c-build", "Kanban build", None);
    let user = h.enabled_user("alice").await;
    // Three 299-char clips = 897 chars × 60 µUSD = exactly the cap.
    set_budget(&h, &user, 0.05382).await;
    let (status, body) = create_conversation_session(&h, &user, Some("c-build")).await;
    assert_eq!(status, 201, "{body}");
    let session = body["data"]["voice_session_id"].as_str().unwrap().to_owned();
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    let word = "a".repeat(299);
    let text = format!("{word} {word} {word}");
    h.service.agent_say(&user.id, "c-build", &text).await.unwrap();
    let budget = ws_expect(&mut ws, "voice.budget").await;
    assert_eq!(budget["data"]["warning"], "100", "{budget}");
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("budget"));
    // The budget ends voice mode: nothing more is spoken today.
    assert!(!h.service.is_bound(&user.id, "c-build"));
    assert!(h.service.agent_say(&user.id, "c-build", "more").await.is_err());
}

#[tokio::test]
async fn a_later_utterance_never_overtakes_a_queued_one() {
    let mut c = call().await;
    c.set_status(running(true, 1));
    c.say("use the staging database");
    assert_eq!(c.follow_up().await["status"], "queued");
    // The approval was granted, the turn ended: the next words still wait behind.
    c.set_status(ConversationStatus::default());
    c.say("and skip the migration");
    assert_eq!(c.follow_up().await["status"], "queued");
    assert!(c.h.conversations.sends().is_empty());
    c.h.service.run_draft_sweep(false, 0).await;
    c.h.service.run_draft_sweep(false, 0).await;
    let texts: Vec<String> = c.h.conversations.sends().into_iter().map(|s| s.text).collect();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[0].starts_with("use the staging database"));
    assert!(texts[1].starts_with("and skip the migration"));
}

#[tokio::test]
async fn the_bound_agents_lines_arrive_in_the_order_they_were_said() {
    let mut c = call().await;
    *c.h.speech.slow.lock().unwrap() = Some(("FIRST".into(), Duration::from_millis(400)));
    c.h.service
        .agent_say(&c.user.id, "c-build", "FIRST line.")
        .await
        .unwrap();
    c.h.service
        .agent_say(&c.user.id, "c-build", "Second line.")
        .await
        .unwrap();
    assert_eq!(c.follow_up().await["line"], "FIRST line.");
    assert_eq!(c.follow_up().await["line"], "Second line.");
}

#[tokio::test]
async fn a_spoken_control_beats_a_pending_ask_and_only_the_bound_ask_takes_answers() {
    let mut c = call().await;
    c.set_status(running(true, 0));
    let ask = {
        let service = c.h.service.clone();
        let uid = c.user.id.clone();
        tokio::spawn(async move {
            service
                .agent_ask(&uid, "c-build", "Which port?", Duration::from_secs(5))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.say("stop the task");
    let outcome = loop {
        let frame = c.follow_up().await;
        if frame["status"] != "asks" {
            break frame;
        }
    };
    assert_eq!(outcome["status"], "stopped", "{outcome}");
    assert_eq!(
        c.h.conversations.state.lock().unwrap().cancelled,
        vec!["c-build".to_owned()]
    );
    assert_eq!(ask.await.unwrap().unwrap().status, "cancelled");

    // A message answers the bound conversation's ask and is not posted.
    let ask = {
        let service = c.h.service.clone();
        let uid = c.user.id.clone();
        tokio::spawn(async move {
            service
                .agent_ask(&uid, "c-build", "Which port?", Duration::from_secs(5))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.say("port eight thousand");
    let answered = ask.await.unwrap().unwrap();
    assert_eq!(answered.status, "answered");
    assert_eq!(answered.answer.as_deref(), Some("port eight thousand"));
    assert!(c.h.conversations.sends().is_empty(), "an answer is never posted");
}

#[tokio::test]
async fn a_failed_cancel_never_claims_nothing_runs() {
    let mut c = call().await;
    c.set_status(running(true, 0));
    c.h.conversations.state.lock().unwrap().cancel_fails = true;
    c.say("cancel that");
    let outcome = c.follow_up().await;
    assert_eq!(outcome["status"], "failed", "{outcome}");
    assert!(outcome["line"].as_str().unwrap().contains("could not stop"));
}

#[tokio::test]
async fn the_binding_holds_while_the_armed_device_is_connected() {
    let timings = aionui_voice::VoiceTimings {
        idle_hangup: Duration::from_millis(400),
        heartbeat_stale: Duration::from_secs(60),
        binding_grace: Duration::from_millis(300),
        ..fast_timings()
    };
    let (h, user, session) = conversation_harness_with(Options { timings }).await;
    let addr = h.serve().await;
    let ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    let mut ws = ws;
    ws_send(&mut ws, hello(None)).await;
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("idle"));
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        h.service.is_bound(&user.id, "c-build"),
        "armed device connected: still bound"
    );
    drop(ws);
    let unbound = wait_for(Duration::from_secs(5), || async {
        (!h.service.is_bound(&user.id, "c-build")).then_some(())
    })
    .await;
    assert!(unbound.is_some(), "device gone: the grace runs out");
}

fn hello_in(mode: &str) -> Value {
    let mut frame = hello(None);
    frame["data"]["mode"] = json!(mode);
    frame
}

#[tokio::test]
async fn switching_the_armed_device_to_jarvis_ends_the_binding() {
    let timings = aionui_voice::VoiceTimings {
        idle_hangup: Duration::from_millis(400),
        heartbeat_stale: Duration::from_secs(60),
        // Long grace: only the switch can end the binding inside this test.
        binding_grace: Duration::from_secs(600),
        ..fast_timings()
    };
    let (h, user, session) = conversation_harness_with(Options { timings }).await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello_in("conversation")).await;
    ws_expect(&mut ws, "voice.state").await;
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("idle"));
    assert!(h.service.is_bound(&user.id, "c-build"), "conversation mode stays armed");

    // The device re-arms in Jarvis mode: its new socket must not renew the binding.
    drop(ws);
    let mut jarvis = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut jarvis, hello_in("jarvis")).await;
    ws_expect(&mut jarvis, "voice.state").await;
    assert!(
        !h.service.is_bound(&user.id, "c-build"),
        "Jarvis mode: the old binding is gone while the socket stays connected"
    );
    let said = h.service.agent_say(&user.id, "c-build", "Done.").await;
    assert!(
        said.as_ref().map_or(true, |s| s.status != "delivered"),
        "the old conversation no longer speaks as bound: {said:?}"
    );
}

#[tokio::test]
async fn a_jarvis_hello_never_unbinds_a_live_conversation_call() {
    let c = call().await;
    let addr = c.h.serve().await;
    // Another device armed in Jarvis mode, or an old client without a mode.
    for frame in [hello_in("jarvis"), hello(None)] {
        let mut other = ws_connect(&addr, &c.user, Some(ORIGIN)).await.unwrap();
        ws_send(&mut other, frame).await;
        ws_expect(&mut other, "voice.state").await;
        assert!(
            c.h.service.is_bound(&c.user.id, "c-build"),
            "the live call keeps its binding"
        );
    }
    assert_eq!(c.h.session_row(&c.user, &c.session).await.state, "live");
}

#[tokio::test]
async fn a_call_on_another_device_leaves_no_binding_once_it_ends() {
    let timings = aionui_voice::VoiceTimings {
        idle_hangup: Duration::from_millis(400),
        heartbeat_stale: Duration::from_secs(60),
        // Long grace: only the fix can end the binding inside this test.
        binding_grace: Duration::from_secs(600),
        ..fast_timings()
    };
    let (h, user, session) = conversation_harness_with(Options { timings }).await;
    let addr = h.serve().await;
    // Device A runs the conversation call; device B is armed in Jarvis mode.
    let mut phone = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut phone, hello_in("conversation")).await;
    ws_expect(&mut phone, "voice.state").await;
    let mut desk = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut desk, hello_in("jarvis")).await;
    ws_expect(&mut desk, "voice.state").await;
    assert!(h.service.is_bound(&user.id, "c-build"), "A's live call keeps it");

    // A goes away; its call then ends. B's Jarvis socket must not keep it alive.
    drop(phone);
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("idle"));
    assert!(
        !h.service.is_bound(&user.id, "c-build"),
        "only a Jarvis-mode device is left: the binding ends with the call"
    );
}
