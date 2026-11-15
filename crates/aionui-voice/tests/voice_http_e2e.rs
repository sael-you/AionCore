//! §3.5 auth matrix for the voice user routes and `/api/voice/ws`.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;

// ---------------------------------------------------------------- identity

#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let h = Harness::new(Options::default()).await;
    for (method, uri) in [
        ("GET", "/api/voice/settings"),
        ("GET", "/api/voice/status"),
        ("GET", "/api/voice/sessions"),
    ] {
        let (status, body) = h.send(Req::new(method, uri).header("origin", ORIGIN).build()).await;
        assert_eq!(status, 401, "{method} {uri}: {body}");
        assert_eq!(body["code"], "UNAUTHORIZED");
    }
}

/// Production runs `--local`: other routes inject `system_default_user`
/// without a token. Voice must not: no cookie → 401, even in local mode,
/// even for the local default user's id.
#[tokio::test]
async fn local_mode_default_user_is_not_trusted() {
    let h = Harness::new(Options::default()).await;
    let (status, body) = h
        .send(Req::new("GET", "/api/voice/settings").header("origin", ORIGIN).build())
        .await;
    assert_eq!(status, 401, "{body}");

    let forged = TestUser {
        id: "system_default_user".into(),
        token: "not-a-jwt".into(),
    };
    let (status, _) = h
        .send(Req::browser("GET", "/api/voice/settings", &forged).build())
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn cookie_session_is_accepted_and_settings_default_off() {
    let h = Harness::new(Options::default()).await;
    let user = h.user("alice").await;
    let (status, body) = h.send(Req::browser("GET", "/api/voice/settings", &user).build()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["enabled"], false, "voice is off by default");
    assert_eq!(body["data"]["daily_budget_usd"], 5.0);

    let (status, body) = h.send(Req::browser("GET", "/api/voice/status", &user).build()).await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["live_calls"], 0);
    assert_eq!(body["data"]["enabled"], false);
}

#[tokio::test]
async fn revoked_session_is_rejected() {
    let h = Harness::new(Options::default()).await;
    let user = h.user("alice").await;
    h.revoke_sessions(&user).await;
    let (status, body) = h.send(Req::browser("GET", "/api/voice/settings", &user).build()).await;
    assert_eq!(status, 401, "{body}");
}

// --------------------------------------------------- credential precedence

#[tokio::test]
async fn non_cookie_credentials_are_rejected_with_400() {
    let h = Harness::new(Options::default()).await;
    let user = h.user("alice").await;
    let bearer = format!("Bearer {}", user.token);
    for (name, value) in [
        ("authorization", bearer.as_str()),
        ("x-aionui-runtime-token", "dummy"),
        ("x-aionui-user-id", "alice"),
        ("x-aionui-conversation-id", "conv-1"),
        ("sec-websocket-protocol", "token-in-protocol"),
    ] {
        let (status, body) = h
            .send(
                Req::browser("GET", "/api/voice/settings", &user)
                    .header(name, value)
                    .build(),
            )
            .await;
        assert_eq!(status, 400, "{name}: {body}");
        assert_eq!(body["code"], "VOICE_CREDENTIAL_CONFLICT", "{name}");
    }
}

/// §3.5 test: cookie + no CSRF header + dummy runtime-token header → rejected
/// (the global CSRF layer exempts runtime-token requests; precedence must not).
#[tokio::test]
async fn cookie_without_csrf_plus_runtime_token_header_is_rejected() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (status, body) = h
        .send(
            Req::new("POST", "/api/voice/sessions")
                .cookie(&user)
                .header("origin", ORIGIN)
                .header("x-aionui-runtime-token", "dummy")
                .json(json!({"sdp_offer": SDP}))
                .build(),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "VOICE_CREDENTIAL_CONFLICT");
    assert!(h.fake.call_ids().is_empty(), "no provider call was made");
}

// ------------------------------------------------------- Origin, not CSRF

/// Voice has no CSRF layer of its own (the C11 UI sends no CSRF header in
/// local identity): every write passes with the cookie + an allowed Origin
/// and no `x-csrf-token`, and is refused without an Origin or with a foreign
/// one. The webui identity's global CSRF layer is covered in the app e2e.
#[tokio::test]
async fn writes_need_an_allowed_origin_not_a_csrf_token() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let writes = [
        (
            "PUT",
            "/api/voice/settings".to_owned(),
            json!({"daily_budget_usd": 2.0}),
            200,
        ),
        ("POST", "/api/voice/sessions".to_owned(), json!({"sdp_offer": SDP}), 201),
    ];
    let mut session = String::new();
    for (method, uri, body, ok) in &writes {
        let (status, resp) = h
            .send(Req::new(method, uri).cookie(&user).json(body.clone()).build())
            .await;
        assert_eq!(status, 403, "{method} {uri} without Origin: {resp}");
        assert_eq!(resp["code"], "VOICE_ORIGIN_REQUIRED");
        let (status, resp) = h
            .send(
                Req::new(method, uri)
                    .cookie(&user)
                    .header("origin", "https://evil.test")
                    .json(body.clone())
                    .build(),
            )
            .await;
        assert_eq!(status, 403, "{method} {uri} foreign Origin: {resp}");
        assert_eq!(resp["code"], "VOICE_ORIGIN_DENIED");
        let (status, resp) = h
            .send(
                Req::new(method, uri)
                    .cookie(&user)
                    .header("origin", ORIGIN)
                    .json(body.clone())
                    .build(),
            )
            .await;
        assert_eq!(status, *ok, "{method} {uri} allowed Origin, no CSRF header: {resp}");
        if let Some(id) = resp["data"]["voice_session_id"].as_str() {
            session = id.to_owned();
        }
    }
    let end = format!("/api/voice/sessions/{session}/end");
    let (status, resp) = h.send(Req::new("POST", &end).cookie(&user).build()).await;
    assert_eq!((status, resp["code"].as_str()), (403, Some("VOICE_ORIGIN_REQUIRED")));
    let (status, resp) = h
        .send(Req::new("POST", &end).cookie(&user).header("origin", ORIGIN).build())
        .await;
    assert_eq!(status, 200, "end with an allowed Origin, no CSRF header: {resp}");
}

// ----------------------------------------------------------------- Origin

#[tokio::test]
async fn origin_allowlist_is_enforced() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;

    let (status, body) = h
        .send(
            Req::new("GET", "/api/voice/settings")
                .cookie(&user)
                .header("origin", "https://evil.test")
                .build(),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], "VOICE_ORIGIN_DENIED");

    let (status, body) = h
        .send(
            Req::new("POST", "/api/voice/sessions")
                .cookie(&user)
                .json(json!({"sdp_offer": SDP}))
                .build(),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], "VOICE_ORIGIN_REQUIRED");

    // Same-origin GET without Origin (browsers omit it) is fine.
    let (status, _) = h.send(Req::new("GET", "/api/voice/status").cookie(&user).build()).await;
    assert_eq!(status, 200);
    assert!(h.fake.call_ids().is_empty());
}

// ------------------------------------------------------------- isolation

#[tokio::test]
async fn sessions_are_isolated_between_users() {
    let h = Harness::new(Options::default()).await;
    let alice = h.enabled_user("alice").await;
    let bob = h.enabled_user("bob").await;
    let (id, _) = live_session(&h, &alice).await;

    let (status, body) = h
        .send(Req::browser("POST", &format!("/api/voice/sessions/{id}/end"), &bob).build())
        .await;
    assert_eq!(status, 404, "{body}");
    let (_, list) = h.send(Req::browser("GET", "/api/voice/sessions", &bob).build()).await;
    assert_eq!(list["data"].as_array().unwrap().len(), 0);
    let (_, status_body) = h.send(Req::browser("GET", "/api/voice/status", &bob).build()).await;
    assert_eq!(status_body["data"]["live_calls"], 0);

    // Alice's call is untouched by Bob's attempts.
    assert_eq!(h.session_row(&alice, &id).await.state, "live");
    let (_, list) = h.send(Req::browser("GET", "/api/voice/sessions", &alice).build()).await;
    assert_eq!(list["data"][0]["id"], id.as_str());
}

#[tokio::test]
async fn responses_never_contain_provider_keys() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (id, _) = live_session(&h, &user).await;
    for uri in ["/api/voice/settings", "/api/voice/status", "/api/voice/sessions"] {
        let (_, body) = h.send(Req::browser("GET", uri, &user).build()).await;
        assert!(!body.to_string().contains(API_KEY_PREFIX), "{uri} leaked a key");
    }
    let (_, body) = h
        .send(Req::browser("POST", &format!("/api/voice/sessions/{id}/end"), &user).build())
        .await;
    assert!(!body.to_string().contains(API_KEY_PREFIX));
}

// --------------------------------------------------------------- settings

#[tokio::test]
async fn settings_update_validates_input() {
    let h = Harness::new(Options::default()).await;
    let user = h.user("alice").await;
    let (status, body) = h
        .send(
            Req::browser("PUT", "/api/voice/settings", &user)
                .json(json!({"enabled": true, "model": "gpt-realtime-2.1-mini", "daily_budget_usd": 3}))
                .build(),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["model"], "gpt-realtime-2.1-mini");
    assert_eq!(body["data"]["daily_budget_usd"], 3.0);

    for bad in [
        json!({"model": "gpt-4o"}),
        json!({"daily_budget_usd": -1}),
        json!({"engine": "minimax-cascade"}),
        json!({"api_key": "sk-should-not-be-accepted"}),
    ] {
        let (status, body) = h
            .send(
                Req::browser("PUT", "/api/voice/settings", &user)
                    .json(bad.clone())
                    .build(),
            )
            .await;
        assert_eq!(status, 400, "{bad}: {body}");
        assert_eq!(body["code"], "BAD_REQUEST");
    }
}

// --------------------------------------------------------------- sockets

#[tokio::test]
async fn ws_upgrade_requires_cookie_and_allowed_origin() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let addr = h.serve().await;

    assert_eq!(ws_connect(&addr, &user, None).await.err(), Some(403));
    assert_eq!(
        ws_connect(&addr, &user, Some("https://evil.test")).await.err(),
        Some(403)
    );
    let nobody = TestUser {
        id: "nobody".into(),
        token: "garbage".into(),
    };
    assert_eq!(ws_connect(&addr, &nobody, Some(ORIGIN)).await.err(), Some(401));

    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.expect("upgrade");
    ws_send(&mut ws, hello(None)).await;
    let state = ws_expect(&mut ws, "voice.state").await;
    assert_eq!(state["data"]["state"], "armed");
}

#[tokio::test]
async fn ws_rejects_token_in_sec_websocket_protocol() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let addr = h.serve().await;
    let mut request = format!("ws://{addr}/api/voice/ws").into_client_request().unwrap();
    request.headers_mut().insert("origin", ORIGIN.parse().unwrap());
    request
        .headers_mut()
        .insert("sec-websocket-protocol", user.token.parse().unwrap());
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 400),
        other => panic!("expected 400, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn ws_first_frame_must_be_hello() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, json!({"name": "voice.heartbeat", "data": {"t": 1.0}})).await;
    let frames = ws_until_closed(&mut ws, Duration::from_secs(3)).await.expect("closed");
    assert_eq!(frames.last().unwrap()["data"]["code"], "HELLO_REQUIRED");

    // Silence: closed after the hello timeout.
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    let frames = ws_until_closed(&mut ws, Duration::from_secs(3)).await.expect("closed");
    assert_eq!(frames.last().unwrap()["data"]["code"], "HELLO_REQUIRED");
}

#[tokio::test]
async fn ws_hello_is_refused_when_voice_is_off() {
    let h = Harness::new(Options::default()).await;
    let user = h.user("alice").await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    let frames = ws_until_closed(&mut ws, Duration::from_secs(3)).await.expect("closed");
    assert_eq!(frames.last().unwrap()["data"]["code"], "VOICE_DISABLED");
}

#[tokio::test]
async fn ws_frames_for_another_users_session_are_refused() {
    let h = Harness::new(Options::default()).await;
    let alice = h.enabled_user("alice").await;
    let bob = h.enabled_user("bob").await;
    let (alice_session, _) = live_session(&h, &alice).await;
    let addr = h.serve().await;

    let mut ws = ws_connect(&addr, &bob, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&alice_session))).await;
    let error = ws_expect(&mut ws, "voice.error").await;
    assert_eq!(error["data"]["code"], "SESSION_MISMATCH");

    // Bob's frozen frame naming Alice's session must not hang up her call.
    ws_send(&mut ws, hello(None)).await;
    ws_expect(&mut ws, "voice.state").await;
    ws_send(
        &mut ws,
        json!({"name": "voice.frozen", "data": {"voiceSessionId": alice_session}}),
    )
    .await;
    let error = ws_expect(&mut ws, "voice.error").await;
    assert_eq!(error["data"]["code"], "SESSION_MISMATCH");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.session_row(&alice, &alice_session).await.state, "live");
}

#[tokio::test]
async fn ws_rate_limit_and_unknown_frames() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    ws_expect(&mut ws, "voice.state").await;

    ws_send(&mut ws, json!({"name": "voice.nope", "data": {}})).await;
    assert_eq!(ws_expect(&mut ws, "voice.error").await["data"]["code"], "INVALID_FRAME");

    for _ in 0..12 {
        ws_send(&mut ws, json!({"name": "voice.heartbeat", "data": {"t": 1.0}})).await;
    }
    assert_eq!(ws_expect(&mut ws, "voice.error").await["data"]["code"], "RATE_LIMITED");
}

#[tokio::test]
async fn ws_oversized_frame_closes_the_socket() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(None)).await;
    ws_expect(&mut ws, "voice.state").await;
    let big = "x".repeat(70 * 1024);
    ws_send(
        &mut ws,
        json!({"name": "voice.heartbeat", "data": {"t": 1.0, "pad": big}}),
    )
    .await;
    assert!(
        ws_until_closed(&mut ws, Duration::from_secs(3)).await.is_some(),
        "server must close on a frame above 64 KB"
    );
}

/// §3.5 identity row: live sockets re-validate the cookie every 60 s
/// (shortened here); a revoked session closes the socket and hangs up.
#[tokio::test]
async fn ws_revalidation_closes_socket_and_call_after_session_revocation() {
    let mut timings = fast_timings();
    timings.revalidate_every = Duration::from_millis(300);
    let h = Harness::new(Options { timings }).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;

    h.revoke_sessions(&user).await;
    let frames = ws_until_closed(&mut ws, Duration::from_secs(5))
        .await
        .expect("socket closed");
    assert!(frames.iter().any(|f| f["data"]["code"] == "AUTH_EXPIRED"), "{frames:?}");
    let row = h.wait_ended(&user, &session).await;
    assert_eq!(row.end_reason.as_deref(), Some("auth_revoked"));
    assert_eq!(h.fake.hangups(&call_id), 1);
}

// ------------------------------------------------------------ kill switch

#[tokio::test]
async fn kill_switch_hangs_up_closes_sockets_and_refuses_sessions() {
    let h = Harness::new(Options::default()).await;
    let user = h.enabled_user("alice").await;
    let (session, call_id) = live_session(&h, &user).await;
    let addr = h.serve().await;
    let mut ws = ws_connect(&addr, &user, Some(ORIGIN)).await.unwrap();
    ws_send(&mut ws, hello(Some(&session))).await;
    ws_expect(&mut ws, "voice.ready").await;

    let (status, body) = h
        .send(
            Req::browser("PUT", "/api/voice/settings", &user)
                .json(json!({"enabled": false}))
                .build(),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    let row = h.session_row(&user, &session).await;
    assert_eq!(row.state, "ended");
    assert_eq!(row.end_reason.as_deref(), Some("disabled"));
    assert_eq!(h.fake.hangups(&call_id), 1, "provider hangup sent");
    assert!(h.fake.is_hung_up(&call_id));
    let frames = ws_until_closed(&mut ws, Duration::from_secs(3))
        .await
        .expect("socket closed");
    assert!(
        frames.iter().any(|f| f["data"]["code"] == "VOICE_DISABLED"),
        "{frames:?}"
    );
    let ended: Vec<_> = frames.iter().filter(|f| f["name"] == "voice.callEnded").collect();
    assert_eq!(ended.len(), 1, "{frames:?}");
    assert_eq!(ended[0]["data"]["reason"], "disabled");

    let (status, body) = create_session(&h, &user).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], "VOICE_DISABLED");
}
