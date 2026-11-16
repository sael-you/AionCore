//! `openai-realtime` call lifecycle against the fake provider (§3.6, §3.5 budget).

mod common;

use std::time::Duration;

use aionui_db::{CreateVoiceSessionParams, IVoiceSessionRepository};
use common::*;
use serde_json::{Value, json};

fn find_update(h: &Harness, call_id: &str) -> Vec<Value> {
    h.fake
        .received(call_id)
        .into_iter()
        .filter(|e| e["type"] == "session.update")
        .collect()
}

#[tokio::test]
async fn call_create_sideband_gate_and_hangup() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    ws_expect(&mut ws, "voice.state").await;

    let (status, body) = create_session(&h, &user).await;
    assert_eq!(status, 201, "{body}");
    let session = body["data"]["voice_session_id"].as_str().unwrap().to_owned();
    assert!(body["data"]["sdp_answer"].as_str().unwrap().starts_with("v=0"));
    assert_eq!(body["data"]["engine"], "openai-realtime");
    assert_eq!(body["data"]["model"], "gpt-realtime-2.1");

    // Call created with multipart SDP + the server-owned session config, using
    // the user's own key.
    let call_id = h.fake.call_ids().pop().unwrap();
    assert_eq!(h.fake.created_sdps(), vec![SDP.to_owned()]);
    let created = &h.fake.created_sessions()[0];
    assert_eq!(created["model"], "gpt-realtime-2.1");
    assert_eq!(created["audio"]["input"]["turn_detection"]["create_response"], true);
    assert_eq!(created["audio"]["input"]["transcription"]["model"], "gpt-transcribe");
    assert_eq!(created["max_output_tokens"], 600);
    assert_eq!(h.fake.bearer_tokens(), vec![format!("{API_KEY_PREFIX}alice")]);

    // Ready only after session.updated answered our session.update.
    let ready = ws_expect(&mut ws, "voice.ready").await;
    assert_eq!(ready["data"]["voiceSessionId"], session.as_str());
    let updates = find_update(&h, &call_id);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["session"]["instructions"], created["instructions"]);
    let row = h.session_row(&user, &session).await;
    assert_eq!(row.state, "live");
    assert_eq!(row.call_id.as_deref(), Some(call_id.as_str()));
    assert!(row.ready_at.is_some());
    assert!(row.instructions_hash.is_some());

    let (_, status_body) = h.send(Req::browser("GET", "/api/voice/status", &user).build()).await;
    assert_eq!(status_body["data"]["live_calls"], 1);
    assert_eq!(status_body["data"]["active_session_id"], session.as_str());

    let (status, body) = h
        .send(Req::browser("POST", &format!("/api/voice/sessions/{session}/end"), &user).build())
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["state"], "ended");
    assert_eq!(body["data"]["end_reason"], "user");
    assert_eq!(h.fake.hangups(&call_id), 1);
    let ended = ws_expect(&mut ws, "voice.callEnded").await;
    assert_eq!(ended["data"]["voiceSessionId"], session.as_str());
    assert_eq!(ended["data"]["reason"], "user");
    let state = ws_expect(&mut ws, "voice.state").await;
    assert_eq!(state["data"]["state"], "armed");
    assert_eq!(state["data"]["reason"], "user");

    // Ending again is idempotent: no second hangup, no second callEnded.
    let (status, _) = h
        .send(Req::browser("POST", &format!("/api/voice/sessions/{session}/end"), &user).build())
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.fake.hangups(&call_id), 1);
    let mut more = Vec::new();
    while let Some(frame) = ws_next(&mut ws, Duration::from_millis(400)).await {
        more.push(frame);
    }
    assert!(
        more.iter().all(|f| f["name"] != "voice.callEnded"),
        "callEnded sent twice: {more:?}"
    );
}

#[tokio::test]
async fn end_reason_from_client_is_recorded_and_relayed() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, _) = live_session(&h, &user).await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;
    let (status, body) = h
        .send(
            Req::browser("POST", &format!("/api/voice/sessions/{session}/end"), &user)
                .json(json!({"reason": "sleep"}))
                .build(),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["end_reason"], "sleep");
    assert_eq!(ws_expect(&mut ws, "voice.callEnded").await["data"]["reason"], "sleep");
}

/// `voice.hello` naming a call that is not ready yet answers `armed` without
/// a reason — display only, never an end (the UI ends only on callEnded).
#[tokio::test]
async fn hello_before_ready_is_armed_without_reason_then_ready() {
    let mut timings = fast_timings();
    timings.engine.config_gate = Duration::from_secs(2);
    let h = Harness::new(Options { timings }).await;
    let user = h.enabled_user("alice").await;
    // Hold the config gate open so the call stays "not ready" for a while.
    h.fake.set_ack_session_update(false);
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    ws_expect(&mut ws, "voice.state").await;
    let (status, body) = create_session(&h, &user).await;
    assert_eq!(status, 201, "{body}");
    let session = body["data"]["voice_session_id"].as_str().unwrap().to_owned();
    ws_send(&mut ws, hello(Some(&session))).await;
    let state = ws_expect(&mut ws, "voice.state").await;
    assert_eq!(state["data"]["state"], "armed");
    assert!(state["data"].get("reason").is_none(), "{state}");
    // The gate times out → the call ends with exactly one callEnded.
    let ended = ws_expect(&mut ws, "voice.callEnded").await;
    assert_eq!(ended["data"]["reason"], "config_timeout");
}

#[tokio::test]
async fn sideband_attach_retries_503() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    h.fake.set_attach_503(3);
    let (_, call_id) = live_session(&h, &user).await;
    assert_eq!(h.fake.attach_attempts(), 4);
    assert_eq!(find_update(&h, &call_id).len(), 1);
}

#[tokio::test]
async fn missing_session_updated_hangs_up() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    h.fake.set_ack_session_update(false);
    let (status, body) = create_session(&h, &user).await;
    assert_eq!(status, 201, "{body}");
    let session = body["data"]["voice_session_id"].as_str().unwrap().to_owned();
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.end_reason.as_deref(), Some("config_timeout"));
    assert!(row.ready_at.is_none());
    let call_id = h.fake.call_ids().pop().unwrap();
    assert_eq!(h.fake.hangups(&call_id), 1);
}

#[tokio::test]
async fn provider_refusing_the_call_is_502_and_recorded_failed() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    h.fake.set_create_status(Some(400));
    let (status, body) = create_session(&h, &user).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["code"], "BAD_GATEWAY");
    let (_, list) = h.send(Req::browser("GET", "/api/voice/sessions", &user).build()).await;
    assert_eq!(list["data"][0]["state"], "failed");
    assert_eq!(list["data"][0]["end_reason"], "create_failed");
    // The failed row does not block the next admission.
    h.fake.set_create_status(None);
    live_session(&h, &user).await;
}

#[tokio::test]
async fn admission_rules() {
    let h = Harness::new(Options::default()).await;

    let off = h.user("off-user").await;
    let (status, body) = create_session(&h, &off).await;
    assert_eq!((status, body["code"].as_str()), (403, Some("VOICE_DISABLED")));

    let nokey = h.enabled_user("no-provider-user").await;
    let (status, body) = create_session(&h, &nokey).await;
    assert_eq!((status, body["code"].as_str()), (412, Some("VOICE_NO_PROVIDER")));

    let user = h.enabled_user("alice").await;
    live_session(&h, &user).await;
    let (status, body) = create_session(&h, &user).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("VOICE_BUSY")),
        "one live call per user"
    );

    let (status, body) = h
        .send(
            Req::browser("POST", "/api/voice/sessions", &user)
                .json(json!({"sdp_offer": "not an sdp"}))
                .build(),
        )
        .await;
    assert_eq!((status, body["code"].as_str()), (400, Some("BAD_REQUEST")));
    let (status, _) = h
        .send(
            Req::browser("POST", "/api/voice/sessions", &user)
                .json(json!({}))
                .build(),
        )
        .await;
    assert_eq!(status, 400, "missing sdp");
    assert_eq!(
        h.fake.call_ids().len(),
        1,
        "refused admissions never reach the provider"
    );
}

#[tokio::test]
async fn resume_of_replaces_the_previous_call() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (first, first_call) = live_session(&h, &user).await;
    let (status, body) = h
        .send(
            Req::browser("POST", "/api/voice/sessions", &user)
                .json(json!({"sdp_offer": SDP, "resume_of": first}))
                .build(),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let row = h.session_row(&user, &first).await;
    assert_eq!(row.end_reason.as_deref(), Some("resumed"));
    assert_eq!(h.fake.hangups(&first_call), 1);
}

#[tokio::test]
async fn daily_budget_cap_refuses_admission() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (status, _) = h
        .send(
            Req::browser("PUT", "/api/voice/settings", &user)
                .json(json!({"daily_budget_usd": 0.01}))
                .build(),
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = create_session(&h, &user).await;
    assert_eq!((status, body["code"].as_str()), (429, Some("VOICE_BUDGET_REACHED")));

    // Spend from earlier sessions today counts too.
    h.send(
        Req::browser("PUT", "/api/voice/settings", &user)
            .json(json!({"daily_budget_usd": 1.0}))
            .build(),
    )
    .await;
    h.voice_repo
        .create_starting(CreateVoiceSessionParams {
            id: "vs_old",
            user_id: &user.id,
            engine: "openai-realtime",
            model: "gpt-realtime-2.1",
            resume_of: None,
            device: None,
            instructions_hash: None,
        })
        .await
        .unwrap();
    h.voice_repo
        .add_usage(
            &user.id,
            "vs_old",
            &aionui_db::VoiceUsageDelta {
                cost_micros: 990_000,
                ..Default::default()
            },
            aionui_common::now_ms(),
        )
        .await
        .unwrap();
    h.voice_repo.finish(&user.id, "vs_old", "ended", "user").await.unwrap();
    let (status, body) = create_session(&h, &user).await;
    assert_eq!((status, body["code"].as_str()), (429, Some("VOICE_BUDGET_REACHED")));
}

#[tokio::test]
async fn usage_is_metered_and_reconciled() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;

    // Backend-issued response: reserved, then reconciled to the reported usage.
    h.service.request_response(&user.id, &session).await.unwrap();
    let row = wait_for(Duration::from_secs(5), || async {
        let row = h.session_row(&user, &session).await;
        (row.responses == 1).then_some(row)
    })
    .await
    .expect("response metered");
    // Full-model price of the captured usage: 356×$4 + 90×$24 + 73×$64 per 1M.
    assert_eq!(row.cost_micros, 1424 + 2160 + 4672);
    assert_eq!(row.input_tokens, 356);
    assert_eq!(row.output_tokens, 163);
    assert_eq!(row.unknown_responses, 0);
    assert!(h.fake.received_types(&call_id).contains(&"response.create".to_owned()));

    // Transcription usage is charged per second; captions reach the socket.
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;
    h.fake.push_event(
        &call_id,
        json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "i1",
            "transcript": "what is the kanban build doing", "usage": {"type": "duration", "seconds": 3}}),
    );
    let caption = ws_expect(&mut ws, "voice.caption").await;
    assert_eq!(caption["data"]["role"], "user");
    assert_eq!(caption["data"]["final"], true);
    assert_eq!(caption["data"]["text"], "what is the kanban build doing");
    let row = wait_for(Duration::from_secs(5), || async {
        let row = h.session_row(&user, &session).await;
        (row.stt_seconds == 3).then_some(row)
    })
    .await
    .unwrap();
    assert_eq!(row.cost_micros, 8256 + 60);

    // Playback end is relayed for the "your turn" tone.
    h.fake.push_event(
        &call_id,
        json!({"type": "output_audio_buffer.started", "response_id": "resp_9"}),
    );
    assert_eq!(ws_expect(&mut ws, "voice.state").await["data"]["state"], "speaking");
    h.fake.push_event(
        &call_id,
        json!({"type": "output_audio_buffer.stopped", "response_id": "resp_9"}),
    );
    let finished = ws_expect(&mut ws, "voice.assistantAudioFinished").await;
    assert_eq!(finished["data"]["responseId"], "resp_9");
}

#[tokio::test]
async fn provider_response_over_budget_is_cancelled_and_call_hung_up() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    // Enough for exactly one reservation (≈ $0.04 on the full model), not two.
    h.send(
        Req::browser("PUT", "/api/voice/settings", &user)
            .json(json!({"daily_budget_usd": 0.07}))
            .build(),
    )
    .await;
    let (session, call_id) = live_session(&h, &user).await;
    h.fake.push_event(
        &call_id,
        json!({"type": "response.created", "response": {"id": "resp_a"}}),
    );
    h.fake.push_event(
        &call_id,
        json!({"type": "response.created", "response": {"id": "resp_b"}}),
    );
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("budget"));
    assert!(h.fake.received_types(&call_id).contains(&"response.cancel".to_owned()));
    assert_eq!(h.fake.hangups(&call_id), 1);
}

#[tokio::test]
async fn budget_warning_at_80_percent_then_hangup_at_100() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    h.send(
        Req::browser("PUT", "/api/voice/settings", &user)
            .json(json!({"daily_budget_usd": 0.1}))
            .build(),
    )
    .await;
    let (session, call_id) = live_session(&h, &user).await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;

    // 85 % spent on transcription: 4250 s × 20 µUSD.
    h.fake.push_event(
        &call_id,
        json!({"type": "conversation.item.input_audio_transcription.completed", "transcript": "x",
            "usage": {"type": "duration", "seconds": 4250}}),
    );
    let budget = ws_expect(&mut ws, "voice.budget").await;
    assert_eq!(budget["data"]["warning"], "80");
    assert_eq!(h.session_row(&user, &session).await.state, "live");

    h.fake.push_event(
        &call_id,
        json!({"type": "conversation.item.input_audio_transcription.completed", "transcript": "y",
            "usage": {"type": "duration", "seconds": 1000}}),
    );
    let budget = ws_expect(&mut ws, "voice.budget").await;
    assert_eq!(budget["data"]["warning"], "100");
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("budget"));
    assert_eq!(h.fake.hangups(&call_id), 1);
}

#[tokio::test]
async fn create_response_toggle_sends_only_turn_detection() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;

    h.service.set_create_response(&user.id, &session, false).await.unwrap();
    let updates = find_update(&h, &call_id);
    assert_eq!(updates.len(), 2);
    let toggle = &updates[1]["session"];
    assert_eq!(toggle["audio"]["input"]["turn_detection"]["create_response"], false);
    assert!(toggle.get("instructions").is_none(), "cache prefix untouched");
    assert!(toggle.get("tools").is_none());

    h.service.set_create_response(&user.id, &session, true).await.unwrap();
    assert_eq!(
        find_update(&h, &call_id)[2]["session"]["audio"]["input"]["turn_detection"]["create_response"],
        true
    );

    // Without acknowledgement the toggle fails (gate), the call stays up.
    h.fake.set_ack_session_update(false);
    assert!(h.service.set_create_response(&user.id, &session, false).await.is_err());
    assert_eq!(h.session_row(&user, &session).await.state, "live");

    // Another user cannot toggle it.
    let bob = h.enabled_user("bob").await;
    assert!(h.service.set_create_response(&bob.id, &session, false).await.is_err());
}

#[tokio::test]
async fn lossy_reattach_charges_in_flight_response_and_signals_ready_again() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;

    // A provider-initiated response is in flight when the sideband drops.
    h.fake.push_event(
        &call_id,
        json!({"type": "response.created", "response": {"id": "resp_lost"}}),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.fake.drop_sidebands(&call_id);

    let offline = ws_expect(&mut ws, "voice.state").await;
    assert_eq!(offline["data"]["state"], "offline");
    assert_eq!(offline["data"]["reason"], "reconnecting");
    // Muted until voice.ready; the call did not end.
    loop {
        let frame = ws_next(&mut ws, Duration::from_secs(5))
            .await
            .expect("ready after re-attach");
        assert_ne!(frame["name"], "voice.callEnded", "re-attach must not end the call");
        if frame["name"] == "voice.ready" {
            break;
        }
    }
    assert_eq!(h.fake.attach_attempts(), 2);
    assert_eq!(find_update(&h, &call_id).len(), 2, "config re-applied on re-attach");

    let row = h.session_row(&user, &session).await;
    assert_eq!(row.state, "live");
    assert_eq!(row.unknown_responses, 1);
    assert!(row.cost_micros > 0, "unknown usage charged at the reservation");
}

#[tokio::test]
async fn reattach_gives_up_after_deadline() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;
    h.fake.set_attach_503(10_000);
    h.fake.drop_sidebands(&call_id);
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.end_reason.as_deref(), Some("control_lost"));
    assert_eq!(h.fake.hangups(&call_id), 1);
}

#[tokio::test]
async fn provider_side_end_is_recorded() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;
    h.fake.end_call_from_provider(&call_id);
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("provider_ended"));
}

#[tokio::test]
async fn idle_hard_cap_and_rotation_timers_hang_up() {
    type Tweak = fn(&mut aionui_voice::VoiceTimings);
    let cases: [(&str, Tweak); 3] = [
        ("idle", |t: &mut aionui_voice::VoiceTimings| {
            t.idle_hangup = Duration::from_millis(300)
        }),
        ("hard_cap", |t: &mut aionui_voice::VoiceTimings| {
            t.hard_cap = Duration::from_millis(300)
        }),
        ("rotation", |t: &mut aionui_voice::VoiceTimings| {
            t.rotation_after = Duration::from_millis(300);
            t.rotation_quiet = Duration::from_millis(100);
        }),
    ];
    for (reason, tweak) in cases {
        let mut timings = fast_timings();
        tweak(&mut timings);
        let h = Harness::new(Options { timings }).await;
        let user = h.enabled_user("alice").await;
        let (session, call_id) = live_session(&h, &user).await;
        let row = h.wait_ended(&user, &session).await;
        assert_eq!(row.end_reason.as_deref(), Some(reason));
        assert_eq!(row.state, "ended");
        assert_eq!(h.fake.hangups(&call_id), 1, "{reason}");
    }
}

#[tokio::test]
async fn device_frozen_or_silent_hangs_up_the_call() {
    // Frozen page (§3.8).
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, _) = live_session(&h, &user).await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;
    ws_send(
        &mut ws,
        json!({"name": "voice.frozen", "data": {"voiceSessionId": session}}),
    )
    .await;
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("device_frozen"));

    // No heartbeat for the stale window.
    let mut timings = fast_timings();
    timings.heartbeat_stale = Duration::from_millis(400);
    let h = Harness::new(Options { timings }).await;
    let user = h.enabled_user("alice").await;
    let (session, _) = live_session(&h, &user).await;
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("device_stale"));
}

/// §3.5: a password change / logout-everywhere during a live call ends it at
/// the next re-validation even with no socket open.
#[tokio::test]
async fn call_revalidation_hangs_up_after_session_revocation() {
    let mut timings = fast_timings();
    timings.revalidate_every = Duration::from_millis(300);
    let h = Harness::new(Options { timings }).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;
    h.revoke_sessions(&user).await;
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("auth_revoked"));
    assert_eq!(h.fake.hangups(&call_id), 1);
}

#[tokio::test]
async fn restart_recovery_reattaches_live_calls_and_closes_stale_rows() {
    let first = Harness::new(Options::default()).await;
    let user = first.enabled_user("alice").await;
    let (session, call_id) = live_session(&first, &user).await;
    // A row stuck in `starting` (process died during creation).
    let bob = first.enabled_user("bob").await;
    first
        .voice_repo
        .create_starting(CreateVoiceSessionParams {
            id: "vs_stuck",
            user_id: &bob.id,
            engine: "openai-realtime",
            model: "gpt-realtime-2.1",
            resume_of: None,
            device: None,
            instructions_hash: None,
        })
        .await
        .unwrap();

    // "Restart": the old process dies, a fresh service runs over the same DB.
    first.service.simulate_crash();
    let second = Harness::with_db(first.db.clone(), first.fake.clone(), Options::default()).await;
    assert_eq!(second.service.recover_open_sessions().await, 1);
    wait_for(Duration::from_secs(5), || async {
        (find_update(&second, &call_id).len() == 2).then_some(())
    })
    .await
    .expect("sideband re-attached with the config re-applied");
    let row = second.session_row(&user, &session).await;
    assert_eq!(row.state, "live");
    assert_eq!(
        row.unknown_responses, 1,
        "the downtime is charged as one unknown response"
    );

    let stuck = second.session_row(&bob, "vs_stuck").await;
    assert_eq!(stuck.state, "failed");
    assert_eq!(stuck.end_reason.as_deref(), Some("restart"));

    // The recovered call is controllable by the new process.
    let (status, _) = second
        .send(Req::browser("POST", &format!("/api/voice/sessions/{session}/end"), &user).build())
        .await;
    assert_eq!(status, 200);
    assert_eq!(second.fake.hangups(&call_id), 1);
}

#[tokio::test]
async fn recovery_hangs_up_when_voice_was_turned_off() {
    let first = Harness::new(Options::default()).await;
    let user = first.enabled_user("alice").await;
    let (session, call_id) = live_session(&first, &user).await;
    // Turn voice off behind the service's back (as if done by another process).
    sqlx::query("UPDATE voice_settings SET enabled = 0 WHERE user_id = ?")
        .bind(&user.id)
        .execute(first.db.pool())
        .await
        .unwrap();
    first.service.simulate_crash();
    let second = Harness::with_db(first.db.clone(), first.fake.clone(), Options::default()).await;
    assert_eq!(second.service.recover_open_sessions().await, 0);
    let row = second.session_row(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("disabled"));
    assert!(second.fake.is_hung_up(&call_id));
}

/// Logging rule (core AGENTS.md): no transcripts or keys in logs, even at
/// debug. The subscriber is process-wide, so the other tests of this binary
/// that run meanwhile are checked too.
#[tokio::test]
async fn logs_never_contain_transcripts_or_keys() {
    use std::sync::{Arc, Mutex, OnceLock};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    static BUF: OnceLock<Buf> = OnceLock::new();
    let buf = BUF.get_or_init(Buf::default).clone();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("only this test installs a subscriber");

    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;
    h.fake.push_event(
        &call_id,
        json!({"type": "conversation.item.input_audio_transcription.completed",
            "transcript": "TRANSCRIPT-MARKER deploy the thing", "usage": {"type": "duration", "seconds": 1}}),
    );
    h.fake.push_event(
        &call_id,
        json!({"type": "response.output_audio_transcript.done", "transcript": "ASSISTANT-MARKER sure"}),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.service
        .end_session(&user.id, &session, aionui_voice::EndReason::User)
        .await
        .unwrap();

    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("voice call ended"), "lifecycle is logged: {logs}");
    assert!(logs.contains("voice provider event"), "debug adds provider event types");
    assert!(!logs.contains("TRANSCRIPT-MARKER"));
    assert!(!logs.contains("ASSISTANT-MARKER"));
    assert!(!logs.contains(API_KEY_PREFIX), "API key leaked into logs");
    assert!(!logs.contains(&user.token), "session token leaked into logs");
}

/// TASK-65: a call recovered after a restart whose device never reconnects
/// still goes stale (liveness counts from the recovery).
#[tokio::test]
async fn a_recovered_call_without_a_device_goes_stale() {
    let first = Harness::new(Options::default()).await;
    let user = first.enabled_user("alice").await;
    let (session, _) = live_session(&first, &user).await;
    first.service.simulate_crash();
    let mut timings = fast_timings();
    timings.heartbeat_stale = Duration::from_millis(400);
    let second = Harness::with_db(first.db.clone(), first.fake.clone(), Options { timings }).await;
    assert_eq!(second.service.recover_open_sessions().await, 1);
    let row = second.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("device_stale"));
}
