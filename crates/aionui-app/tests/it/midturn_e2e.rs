//! E2E tests for mid-turn message delivery (B5, mid-turn interjection Task 4).
//!
//! While a turn is ACTIVE:
//! - a backend with `supports_midturn_delivery=true` accepts a second message
//!   with HTTP 200 and folds it into the CURRENT turn (response `turn_id` ==
//!   the active turn's id, no new turn opened);
//! - a backend without it keeps today's 409 CONFLICT.

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::StatusCode;
use serde_json::json;
use tokio::sync::Mutex;
use tower::ServiceExt;

use aionui_ai_agent::{AgentInstance, IAgentTask, IMockAgent, WorkerTaskManagerImpl};
use aionui_app::{AppConfig, AppServices};
use common::{body_json, get_with_token, json_with_token, setup_and_login};

/// A mock agent whose event channel stays OPEN (the sender is held), so the
/// spawned turn relay keeps waiting for frames and the turn claim stays held —
/// modelling a long-running turn. `deliver_midturn` records the delivered
/// message instead of opening a turn.
struct MidturnMockAgent {
    conversation_id: String,
    supports_midturn: bool,
    tx: tokio::sync::broadcast::Sender<aionui_ai_agent::AgentStreamEvent>,
    delivered: Arc<Mutex<Vec<String>>>,
    send_called: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl IAgentTask for MidturnMockAgent {
    fn agent_type(&self) -> aionui_common::AgentType {
        aionui_common::AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        "/tmp/test"
    }
    fn status(&self) -> Option<aionui_common::ConversationStatus> {
        None
    }
    fn last_activity_at(&self) -> aionui_common::TimestampMs {
        aionui_common::now_ms()
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<aionui_ai_agent::AgentStreamEvent> {
        self.tx.subscribe()
    }
    fn supports_midturn_delivery(&self) -> bool {
        self.supports_midturn
    }
    async fn send_message(
        &self,
        _data: aionui_ai_agent::types::SendMessageData,
    ) -> Result<(), aionui_ai_agent::AgentSendError> {
        self.send_called.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn cancel(&self) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<aionui_common::AgentKillReason>) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl IMockAgent for MidturnMockAgent {
    async fn deliver_midturn(
        &self,
        data: aionui_ai_agent::types::SendMessageData,
    ) -> Result<(), aionui_ai_agent::AgentSendError> {
        self.delivered.lock().await.push(data.content);
        Ok(())
    }
}

struct MidturnRig {
    app: axum::Router,
    services: AppServices,
    delivered: Arc<Mutex<Vec<String>>>,
}

async fn build_midturn_app(supports_midturn: bool) -> MidturnRig {
    let db = aionui_db::init_database_memory().await.unwrap();
    let delivered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let delivered_for_factory = Arc::clone(&delivered);
    let factory: std::sync::Arc<
        dyn Fn(
                aionui_ai_agent::types::BuildTaskOptions,
            )
                -> futures_util::future::BoxFuture<'static, Result<AgentInstance, aionui_ai_agent::AgentError>>
            + Send
            + Sync,
    > = std::sync::Arc::new(move |opts| {
        let delivered = Arc::clone(&delivered_for_factory);
        Box::pin(async move {
            let (tx, _keep_open) = tokio::sync::broadcast::channel(16);
            Ok(AgentInstance::Mock(std::sync::Arc::new(MidturnMockAgent {
                conversation_id: opts.conversation_id().to_owned(),
                supports_midturn,
                tx,
                delivered,
                send_called: Arc::new(AtomicBool::new(false)),
            })))
        })
    });
    let wtm: std::sync::Arc<dyn aionui_ai_agent::IWorkerTaskManager> =
        std::sync::Arc::new(WorkerTaskManagerImpl::new(factory));
    let services = AppServices::from_config(db, &AppConfig::default())
        .await
        .unwrap()
        .with_worker_task_manager(wtm);
    let app = aionui_app::create_router(&services).await.expect("build router");
    MidturnRig {
        app,
        services,
        delivered,
    }
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str) -> String {
    let req = json_with_token(
        "POST",
        "/api/conversations",
        json!({ "type": "acp", "name": "midturn", "extra": { "backend": "gemini" } }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    json["data"]["id"].as_str().unwrap().to_owned()
}

async fn send_message(
    app: &mut axum::Router,
    conv_id: &str,
    content: &str,
    token: &str,
    csrf: &str,
) -> (StatusCode, serde_json::Value) {
    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({ "content": content }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Wait until the conversation's agent is registered with the task manager.
///
/// `send_message` returns 202 as soon as the turn is CLAIMED, but the agent
/// itself is built lazily inside the detached turn task. The mid-turn branch
/// needs `task_manager.get_task(...)` to be populated, so a second send issued
/// before registration completes legitimately falls through to the claim and
/// gets 409. Without this barrier the test races that spawn and fails
/// intermittently under parallel load.
async fn wait_for_agent_registered(services: &aionui_app::AppServices, conv_id: &str) {
    for _ in 0..200 {
        if services.worker_task_manager.get_task(conv_id).is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("agent for {conv_id} never registered with the task manager");
}

/// Brief Step 1: during an active turn, a second message to a
/// `supports_midturn_delivery` backend gets HTTP 200 (not 409) and the response
/// `turn_id` equals the CURRENT active turn's id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn midturn_send_returns_200_with_the_active_turn_id() {
    let MidturnRig {
        mut app,
        services,
        delivered,
    } = build_midturn_app(true).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pw").await;
    let conv_id = create_conversation(&mut app, &token, &csrf).await;

    // Turn 1 opens and stays active: the mock's event channel never finishes.
    let (status1, body1) = send_message(&mut app, &conv_id, "first message", &token, &csrf).await;
    assert_eq!(status1, StatusCode::ACCEPTED, "first send scheduled a turn: {body1}");
    let turn1 = body1["data"]["turn_id"].as_str().unwrap().to_owned();
    // The mid-turn branch reads the REGISTERED agent, which turn 1 builds
    // asynchronously; wait for it rather than racing the spawn.
    wait_for_agent_registered(&services, &conv_id).await;

    // Mid-turn second message: 200, folded into the SAME turn.
    let (status2, body2) = send_message(&mut app, &conv_id, "midturn interjection", &token, &csrf).await;
    assert_eq!(
        status2,
        StatusCode::OK,
        "mid-turn send to a supporting backend must be 200, got {status2}: {body2}"
    );
    let turn2 = body2["data"]["turn_id"].as_str().unwrap();
    assert_eq!(
        turn2, turn1,
        "mid-turn delivery folds into the CURRENT turn — no new turn id"
    );
    assert_eq!(
        body2["data"]["delivered_midturn"], true,
        "the response flags mid-turn delivery for the frontend"
    );

    // Delivered through the mid-turn path (not a second normal turn).
    let delivered = delivered.lock().await.clone();
    assert_eq!(
        delivered,
        vec!["midturn interjection".to_owned()],
        "the message must go through the mid-turn delivery path"
    );

    // The user message is persisted with the pending-receipt status and the
    // list view shows it.
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/{conv_id}/messages"),
            &token,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let items = body["data"]["items"].as_array().unwrap();
    let row = items
        .iter()
        .find(|m| m["content"]["content"] == "midturn interjection")
        .expect("mid-turn user message persisted");
    assert_eq!(
        row["status"], "pending",
        "a mid-turn message starts in the pending-receipt state"
    );
}

/// Backends WITHOUT mid-turn delivery keep today's behavior: a second send
/// during an active turn is 409 CONFLICT with the running-conversation error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn midturn_send_to_non_supporting_backend_stays_409() {
    let MidturnRig { mut app, services, .. } = build_midturn_app(false).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pw").await;
    let conv_id = create_conversation(&mut app, &token, &csrf).await;

    let (status1, _) = send_message(&mut app, &conv_id, "first message", &token, &csrf).await;
    assert_eq!(status1, StatusCode::ACCEPTED);

    let (status2, body2) = send_message(&mut app, &conv_id, "second message", &token, &csrf).await;
    assert_eq!(
        status2,
        StatusCode::CONFLICT,
        "non-supporting backend keeps the 409 gate, got {status2}: {body2}"
    );
    let err = body2["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("already running"),
        "the running-conversation error is preserved, got: {err}"
    );
}

// ── Idempotent sends (voice, PLAN v15 §3.3.6) ────────────────────────────────

fn keyed_request(content: &str) -> aionui_api_types::SendMessageRequest {
    serde_json::from_value(json!({ "content": content })).unwrap()
}

async fn user_messages(services: &AppServices, conv_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND position = 'right'")
        .bind(conv_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap()
}

async fn admin_id(services: &AppServices) -> String {
    sqlx::query_scalar("SELECT id FROM users WHERE username = 'admin'")
        .fetch_one(services.database.pool())
        .await
        .unwrap()
}

/// A re-send with the same key while the original's turn is still running
/// returns the original ids: no second row, no new turn, nothing delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyed_resend_during_the_running_turn_returns_the_original_ids() {
    let MidturnRig {
        mut app,
        services,
        delivered,
    } = build_midturn_app(true).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pw").await;
    let conv_id = create_conversation(&mut app, &token, &csrf).await;
    let user_id = admin_id(&services).await;
    let tm = services.worker_task_manager.clone();
    let svc = &services.conversation_service;

    let first = svc
        .send_message_idempotent(&user_id, &conv_id, keyed_request("voice draft"), &tm, "draft-1:1")
        .await
        .unwrap();
    assert!(!first.delivered_midturn);
    wait_for_agent_registered(&services, &conv_id).await;

    let again = svc
        .send_message_idempotent(&user_id, &conv_id, keyed_request("voice draft"), &tm, "draft-1:1")
        .await
        .unwrap();
    assert_eq!(
        (again.msg_id.as_str(), again.turn_id.as_str()),
        (first.msg_id.as_str(), first.turn_id.as_str())
    );
    assert!(!again.delivered_midturn);
    assert_eq!(user_messages(&services, &conv_id).await, 1);
    assert!(
        delivered.lock().await.is_empty(),
        "the retry was not steered into the turn"
    );

    // A different key during the running turn steers (mid-turn) and is
    // itself idempotent.
    let steer = svc
        .send_message_idempotent(&user_id, &conv_id, keyed_request("change it"), &tm, "draft-1:2")
        .await
        .unwrap();
    assert!(steer.delivered_midturn);
    assert_eq!(steer.turn_id, first.turn_id);
    let steer_again = svc
        .send_message_idempotent(&user_id, &conv_id, keyed_request("change it"), &tm, "draft-1:2")
        .await
        .unwrap();
    assert_eq!(steer_again.msg_id, steer.msg_id);
    assert!(steer_again.delivered_midturn);
    assert_eq!(delivered.lock().await.len(), 1, "delivered once");
    assert_eq!(user_messages(&services, &conv_id).await, 2);
}

/// Two concurrent sends with the same key persist one message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_keyed_sends_persist_one_message() {
    let MidturnRig { mut app, services, .. } = build_midturn_app(false).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pw").await;
    let conv_id = create_conversation(&mut app, &token, &csrf).await;
    let user_id = admin_id(&services).await;
    let tm = services.worker_task_manager.clone();
    let svc = &services.conversation_service;

    let (a, b) = tokio::join!(
        svc.send_message_idempotent(&user_id, &conv_id, keyed_request("once"), &tm, "draft-2:1"),
        svc.send_message_idempotent(&user_id, &conv_id, keyed_request("once"), &tm, "draft-2:1"),
    );
    let ok: Vec<_> = [a, b].into_iter().filter_map(Result::ok).collect();
    assert!(!ok.is_empty(), "at least one send succeeded");
    if let [x, y] = ok.as_slice() {
        assert_eq!(x.msg_id, y.msg_id, "both answers name the same message");
    }
    assert_eq!(user_messages(&services, &conv_id).await, 1);
}

/// Keys are validated; an unkeyed send is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotency_keys_are_validated() {
    let MidturnRig { mut app, services, .. } = build_midturn_app(false).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "pw").await;
    let conv_id = create_conversation(&mut app, &token, &csrf).await;
    let user_id = admin_id(&services).await;
    let tm = services.worker_task_manager.clone();
    let err = services
        .conversation_service
        .send_message_idempotent(&user_id, &conv_id, keyed_request("x"), &tm, "  ")
        .await
        .unwrap_err();
    assert!(
        matches!(err, aionui_conversation::ConversationError::BadRequest { .. }),
        "{err:?}"
    );
}
