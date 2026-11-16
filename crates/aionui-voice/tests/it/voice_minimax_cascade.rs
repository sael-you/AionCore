//! `minimax-cascade` engine (TASK-91) against the fake provider: the realtime
//! call is only the ear, MiniMax-M3 answers with the same tools, and every
//! sentence comes back as a TTS clip (`voice.followUp` status `reply`).
//! Latency per turn is measured from the final user transcript to the first
//! reply clip frame on the device socket.

use crate::common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

const BRAIN_FAILED_LINE: &str = "Sorry, I could not answer that. Please try again.";

struct Call {
    h: Harness,
    user: TestUser,
    session: String,
    call_id: String,
    ws: Client,
    /// Assistant captions seen while reading replies.
    captions: Vec<Value>,
}

async fn cascade_user(h: &Harness, id: &str) -> TestUser {
    let user = h.enabled_user(id).await;
    h.service
        .update_settings(
            &user.id,
            &aionui_api_types::UpdateVoiceSettingsRequest {
                engine: Some("minimax-cascade".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    user
}

async fn cascade_call() -> Call {
    let h = Harness::new(Options::default()).await;
    let user = cascade_user(&h, "mm-alice").await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    let (session, call_id) = live_session(&h, &user).await;
    Call {
        h,
        user,
        session,
        call_id,
        ws,
        captions: Vec::new(),
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

    fn speech_started(&self) {
        self.h.fake.push_event(
            &self.call_id,
            json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 100}),
        );
    }

    /// Reply frames until `count` arrived (other frames skipped).
    async fn replies(&mut self, count: usize) -> Vec<Value> {
        let mut out = Vec::new();
        while out.len() < count {
            let frame = ws_next(&mut self.ws, Duration::from_secs(10))
                .await
                .expect("socket closed or timed out waiting for a reply");
            if frame["name"] == "voice.caption" && frame["data"]["role"] == "assistant" {
                self.captions.push(frame["data"].clone());
            }
            if frame["name"] == "voice.followUp" && frame["data"]["status"] == "reply" {
                out.push(frame["data"].clone());
            }
        }
        out
    }

    /// One turn: say `text`, return (ms to the first reply clip, the replies).
    async fn turn(&mut self, text: &str, sentences: usize) -> (u128, Vec<Value>) {
        let t0 = Instant::now();
        self.say(text);
        let first = self.replies(1).await;
        let first_ms = t0.elapsed().as_millis();
        let mut all = first;
        all.extend(self.replies(sentences - 1).await);
        (first_ms, all)
    }
}

fn lines(replies: &[Value]) -> Vec<String> {
    replies.iter().map(|r| r["line"].as_str().unwrap().to_owned()).collect()
}

fn p50(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

#[tokio::test]
async fn cascade_answers_with_minimax_and_speaks_each_sentence_as_a_clip() {
    let mut c = cascade_call().await;
    // The ear: a transcribe-only realtime call (mini model, no tools, no responses).
    let ear = c.h.fake.created_sessions().last().cloned().unwrap();
    assert_eq!(ear["model"], "gpt-realtime-2.1-mini", "{ear}");
    assert_eq!(ear["audio"]["input"]["turn_detection"]["create_response"], false);
    assert_eq!(ear["tools"], json!([]));
    let row = c.h.session_row(&c.user, &c.session).await;
    assert_eq!(
        (row.engine.as_str(), row.model.as_str()),
        ("minimax-cascade", "MiniMax-M3")
    );

    c.h.fake.set_chat_script(vec![
        json!({"text": "You have two projects. They are aionui and clauterm."}),
    ]);
    let (first_ms, replies) = c.turn("What projects do I have?", 2).await;
    eprintln!("cascade latency (no provider delay): final transcript -> first reply clip {first_ms} ms");
    assert_eq!(
        lines(&replies),
        vec!["You have two projects.", "They are aionui and clauterm."]
    );
    for reply in &replies {
        assert_eq!(reply["conversationId"], "");
        assert_eq!(reply["voiceSessionId"], c.session.as_str());
        assert!(reply["clipId"].as_str().is_some_and(|id| !id.is_empty()), "{reply}");
    }
    // The clips are the speech kit's synthesis of exactly those sentences.
    assert_eq!(c.h.speech.lines(), lines(&replies));

    // The brain request: MiniMax-M3, thinking off, priority, the Jarvis tools,
    // the MiniMax key (never the OpenAI one), the transcript as the user turn.
    let request = c.h.fake.chat_requests().pop().unwrap();
    assert_eq!(request["model"], "MiniMax-M3");
    assert_eq!(request["thinking"]["type"], "disabled");
    assert_eq!(request["service_tier"], "priority");
    assert_eq!(request["stream"], true);
    assert_eq!(request["_bearer"], format!("{API_KEY_PREFIX}minimax-mm-alice"));
    let tools = request["tools"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .any(|t| t["type"] == "function" && t["function"]["name"] == "list_projects")
    );
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert!(messages[0]["content"].as_str().unwrap().starts_with("You are Jarvis"));
    assert_eq!(messages.last().unwrap()["content"], "What projects do I have?");

    // The assistant caption, then the metering of the brain turn (1000 prompt
    // tokens, 800 cached, 40 out at M3 priority = 234 µUSD), STT and clips.
    let caption = c.captions.last().expect("assistant caption");
    assert_eq!(caption["text"], "You have two projects. They are aionui and clauterm.");
    assert_eq!(caption["final"], true);
    let row = wait_for(Duration::from_secs(3), || async {
        let row = c.h.session_row(&c.user, &c.session).await;
        (row.responses == 1).then_some(row)
    })
    .await
    .expect("brain turn metered");
    assert_eq!(
        (row.input_tokens, row.cached_input_tokens, row.output_tokens),
        (1000, 800, 40)
    );
    assert_eq!(row.unknown_responses, 0);
    let tts = 60 * ("You have two projects.".len() + "They are aionui and clauterm.".len()) as i64;
    assert_eq!(row.cost_micros, 234 + 20 + tts, "brain + 1 s STT + clip characters");
}

#[tokio::test]
async fn cascade_runs_the_jarvis_tools_and_answers_from_their_result() {
    let mut c = cascade_call().await;
    c.h.conversations.add_project("p1", "aionui", None);
    c.h.fake.set_chat_script(vec![
        json!({"text": "Let me check that.", "tool_calls": [{"name": "list_projects", "arguments": "{}"}]}),
        json!({"text": "You have one project, aionui."}),
    ]);
    let t0 = Instant::now();
    let (first_ms, replies) = c.turn("Which projects are there?", 2).await;
    let answer_ms = t0.elapsed().as_millis();
    eprintln!("cascade tool turn latency (no provider delay): filler {first_ms} ms, answer after tool {answer_ms} ms");
    assert_eq!(
        lines(&replies),
        vec!["Let me check that.", "You have one project, aionui."]
    );

    let requests = c.h.fake.chat_requests();
    assert_eq!(requests.len(), 2, "one turn for the tool call, one for the answer");
    let messages = requests[1]["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
        .expect("assistant tool call kept in the history");
    assert_eq!(call["tool_calls"][0]["id"], "call_fake_0");
    assert_eq!(call["tool_calls"][0]["function"]["name"], "list_projects");
    let result = messages.last().unwrap();
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], "call_fake_0");
    assert!(result["content"].as_str().unwrap().contains("aionui"), "{result}");

    // Both brain turns are metered as responses.
    let row = wait_for(Duration::from_secs(3), || async {
        let row = c.h.session_row(&c.user, &c.session).await;
        (row.responses == 2).then_some(row)
    })
    .await
    .expect("both brain turns metered");
    assert_eq!(row.unknown_responses, 0);
}

#[tokio::test]
async fn barge_in_stops_the_brain_and_drops_sentences_not_yet_spoken() {
    let mut c = cascade_call().await;
    c.h.fake.set_chat_timing(Duration::ZERO, Duration::from_millis(60));
    c.h.fake.set_chat_script(vec![json!({"text":
        "First sentence is here. Second sentence follows now. Third sentence is long enough. Fourth one ends it."})]);
    c.say("Tell me everything.");
    let first = c.replies(1).await;
    assert_eq!(lines(&first), vec!["First sentence is here."]);
    c.speech_started();
    // Nothing of the cancelled answer is delivered after the barge-in.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while let Some(frame) = ws_next(
        &mut c.ws,
        deadline.saturating_duration_since(tokio::time::Instant::now()),
    )
    .await
    {
        assert!(
            !(frame["name"] == "voice.followUp" && frame["data"]["status"] == "reply"),
            "reply after barge-in: {frame}"
        );
    }
    assert!(!c.h.speech.lines().iter().any(|l| l.contains("Fourth")));
    // The cancelled turn is charged at its reservation (tokens may be billed).
    let row = wait_for(Duration::from_secs(3), || async {
        let row = c.h.session_row(&c.user, &c.session).await;
        (row.responses == 1).then_some(row)
    })
    .await
    .expect("cancelled turn metered");
    assert_eq!(row.unknown_responses, 1);

    // The next utterance gets a fresh answer.
    c.h.fake.set_chat_timing(Duration::ZERO, Duration::ZERO);
    c.h.fake.set_chat_script(vec![json!({"text": "Okay, stopping there."})]);
    let (_, replies) = c.turn("Stop, that is enough.", 1).await;
    assert_eq!(lines(&replies), vec!["Okay, stopping there."]);
}

#[tokio::test]
async fn a_failed_brain_turn_is_spoken_and_reported() {
    let mut c = cascade_call().await;
    c.h.fake.set_chat_script(vec![json!({"status": 500})]);
    c.say("What is running?");
    let error = ws_expect(&mut c.ws, "voice.error").await;
    assert_eq!(error["data"]["code"], "VOICE_PROVIDER_ERROR");
    assert_eq!(error["data"]["message"], "minimax_brain_failed");
    let replies = c.replies(1).await;
    assert_eq!(lines(&replies), vec![BRAIN_FAILED_LINE]);
}

#[tokio::test]
async fn cascade_needs_a_minimax_provider() {
    let h = Harness::new(Options::default()).await;
    // OpenAI row only: the ear would work, the brain has no key.
    let user = cascade_user(&h, "bob").await;
    let (status, body) = create_session(&h, &user).await;
    assert_eq!(status, 412, "{body}");
    assert_eq!(body["code"], "VOICE_NO_MINIMAX_PROVIDER");
    assert!(h.fake.call_ids().is_empty(), "no provider call was created");
    assert!(!body.to_string().contains(API_KEY_PREFIX));
}

#[tokio::test]
async fn the_browser_realtime_request_is_accepted_but_other_engines_are_not() {
    let h = Harness::new(Options::default()).await;
    let user = cascade_user(&h, "mm-carol").await;
    // What every browser build sends (openaiRealtime.ts createSession).
    let (status, body) = h
        .send(
            Req::browser("POST", "/api/voice/sessions", &user)
                .json(json!({"sdp_offer": SDP, "device_id": "web", "engine": "openai-relay"}))
                .build(),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = h
        .send(
            Req::browser("POST", "/api/voice/sessions", &user)
                .json(json!({"sdp_offer": SDP, "device_id": "web", "engine": "openai-realtime"}))
                .build(),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["data"]["engine"], "minimax-cascade");
    assert_eq!(body["data"]["model"], "MiniMax-M3");
    assert!(body["data"]["sdp_answer"].as_str().unwrap().starts_with("v=0"));
}

/// Owner decision (TASK-37): a restart no longer ends cascade calls; the
/// recovered call resumes and its brain still knows the earlier dialogue.
#[tokio::test]
async fn a_restart_mid_call_resumes_the_cascade_with_its_prior_context() {
    let mut c = cascade_call().await;
    c.h.fake.set_chat_script(vec![json!({"text": "Your build is green."})]);
    c.turn("How is my build doing?", 1).await;
    let (repo, session) = (c.h.voice_repo.clone(), c.session.clone());
    wait_for(Duration::from_secs(5), || async {
        let turns = aionui_db::IVoiceDraftRepository::recent_transcripts(repo.as_ref(), &session, 10)
            .await
            .unwrap();
        (turns.len() == 2).then_some(())
    })
    .await
    .expect("the dialogue is persisted with the session");

    // "Restart": the old process dies, a fresh service runs over the same DB.
    c.h.service.simulate_crash();
    let restarted = Harness::with_db(c.h.db.clone(), c.h.fake.clone(), Options::default()).await;
    assert_eq!(restarted.service.recover_open_sessions().await, 1, "resumed, not ended");
    let row = restarted.session_row(&c.user, &c.session).await;
    assert_eq!((row.state.as_str(), row.engine.as_str()), ("live", "minimax-cascade"));
    let fake = c.h.fake.clone();
    let call_id = c.call_id.clone();
    wait_for(Duration::from_secs(5), || async {
        (fake.open_sidebands(&call_id) >= 1).then_some(())
    })
    .await
    .expect("the ear re-attached");

    // The next utterance reaches the brain with the earlier turns in its history.
    let before = c.h.fake.chat_requests().len();
    c.h.fake.set_chat_script(vec![json!({"text": "It still is."})]);
    c.say("And is it still green?");
    let request = wait_for(Duration::from_secs(10), || async {
        let requests = fake.chat_requests();
        (requests.len() > before).then(|| requests.last().cloned().unwrap())
    })
    .await
    .expect("the recovered call answers");
    let messages: Vec<(String, String)> = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_owned(),
                m["content"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let pos = |role: &str, text: &str| {
        messages
            .iter()
            .position(|(r, t)| r == role && t == text)
            .unwrap_or_else(|| panic!("{role}: {text} missing in {messages:?}"))
    };
    let (asked, answered, now) = (
        pos("user", "How is my build doing?"),
        pos("assistant", "Your build is green."),
        pos("user", "And is it still green?"),
    );
    assert!(asked < answered && answered < now, "{messages:?}");
}

/// Latency per turn with the S0-B provider timings on the fake: brain first
/// output 600 ms (M3 priority P50), 20 ms between chunks, TTS 300 ms per clip
/// (bidi P50). The backend's own share is the rest.
#[tokio::test]
async fn turn_latency_with_s0_provider_timings() {
    let mut c = cascade_call().await;
    // Backend overhead alone: no provider delay.
    let mut overhead = Vec::new();
    for n in 0..5 {
        c.h.fake
            .set_chat_script(vec![json!({"text": format!("Answer number {n} is ready now.")})]);
        overhead.push(c.turn(&format!("Question {n}?"), 1).await.0);
    }
    c.h.fake
        .set_chat_timing(Duration::from_millis(600), Duration::from_millis(20));
    *c.h.speech.slow.lock().unwrap() = Some((String::new(), Duration::from_millis(300)));
    let mut modelled = Vec::new();
    for n in 0..5 {
        c.h.fake.set_chat_script(vec![
            json!({"text": format!("Answer number {n} is ready now. A second sentence follows.")}),
        ]);
        modelled.push(c.turn(&format!("Question {n} again?"), 2).await.0);
    }
    eprintln!(
        "cascade latency per turn, final transcript -> first reply clip: backend only {overhead:?} ms (P50 {}), \
         with S0 provider timings {modelled:?} ms (P50 {})",
        p50(overhead.clone()),
        p50(modelled.clone())
    );
    // 600 ms brain + ≈ 100 ms of chunks for the first sentence + 300 ms TTS.
    assert!(p50(modelled.clone()) >= 900, "{modelled:?}");
    assert!(p50(modelled) < 1500, "backend adds well under 0.5 s");
    assert!(p50(overhead) < 300);
}

#[tokio::test]
async fn the_answer_ends_only_after_its_last_sentence_was_delivered() {
    let mut c = cascade_call().await;
    // The last sentence takes a while to synthesize.
    *c.h.speech.slow.lock().unwrap() = Some(("clauterm".into(), Duration::from_millis(800)));
    c.h.fake.set_chat_script(vec![
        json!({"text": "You have two projects. They are aionui and clauterm."}),
    ]);
    c.say("What projects do I have?");
    let mut order = Vec::new();
    loop {
        let frame = ws_next(&mut c.ws, Duration::from_secs(10))
            .await
            .expect("socket closed or timed out");
        if frame["name"] == "voice.followUp" && frame["data"]["status"] == "reply" {
            order.push("reply");
        }
        if frame["name"] == "voice.assistantAudioFinished" {
            assert_eq!(frame["data"]["voiceSessionId"], c.session.as_str());
            order.push("finished");
            break;
        }
    }
    assert_eq!(
        order,
        vec!["reply", "reply", "finished"],
        "the end comes after the last clip"
    );
}

// ------------------------------------------------------------- TASK-112

/// The cascade waits for a silence after a final before it answers, and a
/// fragment inside that silence joins the same user turn (one brain request).
#[tokio::test]
async fn the_cascade_waits_for_a_silence_and_merges_fragments() {
    let timings = aionui_voice::VoiceTimings {
        cascade_answer_silence: Duration::from_millis(1500),
        ..fast_timings()
    };
    let h = Harness::new(Options { timings }).await;
    let user = cascade_user(&h, "mm-dora").await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    let (_session, call_id) = live_session(&h, &user).await;
    // The ear: the most tolerant end-of-turn the Realtime API documents.
    let ear = h.fake.created_sessions().last().cloned().unwrap();
    assert_eq!(ear["audio"]["input"]["turn_detection"]["type"], "semantic_vad");
    assert_eq!(ear["audio"]["input"]["turn_detection"]["eagerness"], "low", "{ear}");

    h.fake
        .set_chat_script(vec![json!({"text": "You have two projects, aionui and clauterm."})]);
    let say = |text: &str| {
        h.fake.push_event(
            &call_id,
            json!({"type": "conversation.item.input_audio_transcription.completed", "transcript": text,
                "usage": {"type": "duration", "seconds": 1}}),
        );
    };
    let t0 = std::time::Instant::now();
    say("What projects");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(h.fake.chat_requests().is_empty(), "no answer inside the silence");
    say("do I have?");
    let request = wait_for(Duration::from_secs(5), || async { h.fake.chat_requests().pop() })
        .await
        .expect("the brain answers after the silence");
    assert!(t0.elapsed() >= Duration::from_millis(2100), "{:?}", t0.elapsed());
    assert_eq!(h.fake.chat_requests().len(), 1, "one user turn, one answer");
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["content"], "What projects do I have?");
}
