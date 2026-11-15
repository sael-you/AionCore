//! Admission and lifecycle races found in the S1a review: aborted requests,
//! kill switch during open, recovered clocks, `mark_live` failure, and
//! token-scoped re-validation.

mod common;

use std::time::Duration;

use aionui_api_types::{CreateVoiceSessionRequest, UpdateVoiceSettingsRequest};
use aionui_db::IVoiceSessionRepository;
use aionui_voice::VoiceError;
use axum::body::Body;
use axum::http::{HeaderValue, Request};
use common::*;

fn request() -> CreateVoiceSessionRequest {
    CreateVoiceSessionRequest {
        engine: None,
        sdp_offer: SDP.into(),
        device_id: Some("web".into()),
        focus_conversation_id: None,
        resume_of: None,
    }
}

async fn open_rows(h: &Harness) -> Vec<aionui_db::VoiceSessionRow> {
    h.voice_repo.list_open().await.unwrap()
}

/// Review #1: a request dropped while the provider is answering must not
/// strand a `starting` row (which blocks the user) or leak the call.
#[tokio::test]
async fn dropped_request_mid_open_still_reaches_a_runner() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    h.fake.set_create_delay(Duration::from_millis(400));

    let dropped = tokio::time::timeout(
        Duration::from_millis(100),
        h.service.create_session(&user.id, Some(user.token.clone()), request()),
    )
    .await;
    assert!(dropped.is_err(), "the request future was dropped mid-open");

    let row = wait_for(Duration::from_secs(5), || async {
        open_rows(&h)
            .await
            .into_iter()
            .find(|r| r.state == "live" && r.ready_at.is_some())
    })
    .await
    .expect("the admission finished in the background and the call is under a runner");
    assert!(open_rows(&h).await.iter().all(|r| r.state != "starting"));

    // The orphaned call is still controllable and the user is not blocked.
    h.service
        .end_session(&user.id, &row.id, aionui_voice::EndReason::User)
        .await
        .unwrap();
    assert_eq!(h.fake.hangups(row.call_id.as_deref().unwrap()), 1);
    h.fake.set_create_delay(Duration::ZERO);
    live_session(&h, &user).await;
}

/// Review #2: turning voice off while an admission is waiting on the
/// provider ends that new call too.
#[tokio::test]
async fn kill_switch_during_open_hangs_up_the_new_call() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    h.fake.set_create_delay(Duration::from_millis(400));

    let service = h.service.clone();
    let (uid, token) = (user.id.clone(), user.token.clone());
    let admission = tokio::spawn(async move { service.create_session(&uid, Some(token), request()).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.service
        .update_settings(
            &user.id,
            &UpdateVoiceSettingsRequest {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let result = admission.await.unwrap();
    assert!(matches!(result, Err(VoiceError::Disabled)), "{result:?}");
    let call_id = h.fake.call_ids().pop().expect("the provider call was created");
    assert_eq!(h.fake.hangups(&call_id), 1, "the new call was hung up");
    assert!(open_rows(&h).await.is_empty());
    let (_, list) = h.send(Req::browser("GET", "/api/voice/sessions", &user).build()).await;
    assert_eq!(list["data"][0]["end_reason"], "disabled");
}

/// Review #3: a recovered call keeps the hard cap of its original start.
#[tokio::test]
async fn recovered_call_keeps_its_hard_cap_clock() {
    let first = Harness::new(Options::default()).await;
    let user = first.enabled_user("alice").await;
    let (session, call_id) = live_session(&first, &user).await;
    // The call started 56 minutes ago (hard cap 55).
    sqlx::query("UPDATE voice_sessions SET created_at = ? WHERE id = ?")
        .bind(aionui_common::now_ms() - 56 * 60_000)
        .bind(&session)
        .execute(first.db.pool())
        .await
        .unwrap();
    first.service.simulate_crash();

    let second = Harness::with_db(first.db.clone(), first.fake.clone(), Options::default()).await;
    assert_eq!(second.service.recover_open_sessions().await, 1);
    let row = second.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("hard_cap"));
    assert!(second.fake.is_hung_up(&call_id));
}

/// Review #4: if persisting `live` fails after the provider created the
/// call, the call is hung up instead of leaking.
#[tokio::test]
async fn mark_live_failure_hangs_up_the_provider_call() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    h.fake.set_create_delay(Duration::from_millis(400));
    let service = h.service.clone();
    let (uid, token) = (user.id.clone(), user.token.clone());
    let admission = tokio::spawn(async move { service.create_session(&uid, Some(token), request()).await });

    // Remove the `starting` row while the provider is answering.
    let row = wait_for(Duration::from_secs(2), || async { open_rows(&h).await.pop() })
        .await
        .expect("starting row");
    sqlx::query("DELETE FROM voice_sessions WHERE id = ?")
        .bind(&row.id)
        .execute(h.db.pool())
        .await
        .unwrap();

    let result = admission.await.unwrap();
    assert!(result.is_err(), "{result:?}");
    let call_id = h.fake.call_ids().pop().unwrap();
    assert_eq!(h.fake.hangups(&call_id), 1, "the created call was hung up");
}

/// Review #6: one socket's failing cookie ends only the calls on that cookie
/// or bound to that socket.
#[tokio::test]
async fn socket_token_failure_ends_only_its_own_calls() {
    let mut timings = fast_timings();
    timings.revalidate_every = Duration::from_millis(300);
    let h = Harness::new(Options { timings }).await;
    let user = h.enabled_user("alice").await;
    // The call runs on cookie A (the creating request).
    let (session, _) = live_session(&h, &user).await;

    // Socket on cookie B (a second login), not bound to the call; B gets
    // logged out. JWT `iat` has second resolution: wait for a distinct token.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let other = TestUser {
        id: user.id.clone(),
        token: h.jwt.sign_with_session_generation(&user.id, &user.id, 0).unwrap(),
    };
    assert_ne!(other.token, user.token);
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &other, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    ws_expect(&mut ws, "voice.state").await;
    h.jwt.blacklist_token(&other.token);

    let frames = ws_until_closed(&mut ws, Duration::from_secs(3))
        .await
        .expect("socket B closed");
    assert!(frames.iter().any(|f| f["data"]["code"] == "AUTH_EXPIRED"), "{frames:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        h.session_row(&user, &session).await.state,
        "live",
        "the call on cookie A survives"
    );
}

/// Review #6: a recovered call (no creating request) adopts the cookie of
/// the first socket that binds it, and ends when that cookie is revoked.
#[tokio::test]
async fn recovered_call_adopts_the_binding_socket_cookie() {
    let mut timings = fast_timings();
    timings.revalidate_every = Duration::from_millis(300);
    let first = Harness::new(Options { timings }).await;
    let user = first.enabled_user("alice").await;
    let (session, _) = live_session(&first, &user).await;
    first.service.simulate_crash();

    let second = Harness::with_db(first.db.clone(), first.fake.clone(), Options { timings }).await;
    assert_eq!(second.service.recover_open_sessions().await, 1);
    let addr = second.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;
    drop(ws);

    second.revoke_sessions(&user).await;
    let row = second.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("auth_revoked"));
}

/// Review #9: a present but unparseable Origin is refused, not ignored.
#[tokio::test]
async fn unparseable_origin_is_refused() {
    let h = Harness::new(Options::default()).await;
    let user = h.user("alice").await;
    let mut request = Request::builder()
        .method("GET")
        .uri("/api/voice/status")
        .header("cookie", format!("aionui-session={}", user.token))
        .body(Body::empty())
        .unwrap();
    request
        .headers_mut()
        .insert("origin", HeaderValue::from_bytes(b"https://voice.test\xff").unwrap());
    let (status, body) = h.send(request).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], "VOICE_ORIGIN_DENIED");
}
