#![allow(clippy::disallowed_types)]
//! Agent router `/api/runtime/devices/…` (C38 W3): what the `aionui-devices`
//! MCP bridge (`aioncore mcp-devices-stdio`) calls.
//!
//! Mounted OUTSIDE the auth and CSRF middleware, like
//! `aionui-board/src/runtime_routes.rs`: it validates the
//! `x-aionui-{user-id,conversation-id,runtime-token}` headers itself
//! (`RuntimeTokenService::validate`, scope `ConversationHelper`), so the
//! token-bound conversation is the caller: the body cannot name another user
//! or conversation, and approvals / audit rows carry that conversation.
//! Every call goes through [`DevicesService::agent_call`] (switch, verb
//! allowlist, rate limit, crucial gate, audit); there is no switch, pairing,
//! approval or install endpoint here.

use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::devices::{DeviceKind, DeviceStatus};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::{AgentCall, MAC_VERBS, PHONE_VERBS};
use crate::routes::{code_str, status_of};
use crate::service::DevicesService;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";

#[derive(Clone)]
pub struct DevicesRuntimeRouterState {
    pub service: Arc<DevicesService>,
    pub runtime_token_service: Arc<RuntimeTokenService>,
}

pub fn devices_runtime_routes(state: DevicesRuntimeRouterState) -> Router {
    Router::new()
        .route("/api/runtime/devices", get(list))
        .route("/api/runtime/devices/call", post(call))
        .route("/api/runtime/devices/addon-run", post(addon_run))
        .with_state(state)
}

type Reply = (StatusCode, Json<Value>);

fn header(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// `(user_id, conversation_id)` of a valid conversation-helper token.
fn authenticate(state: &DevicesRuntimeRouterState, headers: &HeaderMap) -> Result<(String, String), Reply> {
    let caller = (|| {
        let user_id = header(headers, HEADER_USER_ID)?;
        let conversation_id = header(headers, HEADER_CONVERSATION_ID)?;
        let token = header(headers, HEADER_RUNTIME_TOKEN)?;
        state
            .runtime_token_service
            .validate(
                Some(&token),
                &user_id,
                &conversation_id,
                RuntimeTokenScope::ConversationHelper,
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            )
            .ok()?;
        Some((user_id, conversation_id))
    })();
    caller.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "code": "UNAUTHORIZED", "message": "a valid conversation runtime token is required"})),
        )
    })
}

/// The phones and Macs of the caller's user, with what the agent may call.
async fn list(State(s): State<DevicesRuntimeRouterState>, headers: HeaderMap) -> Reply {
    let (user_id, _conversation) = match authenticate(&s, &headers) {
        Ok(c) => c,
        Err(reply) => return reply,
    };
    match s.service.list(&user_id).await {
        Ok(devices) => {
            let rows: Vec<Value> = devices
                .into_iter()
                // Phones, Macs, and add-ons that are installed on a helper.
                .filter(|d| match d.kind {
                    DeviceKind::Phone | DeviceKind::Mac => true,
                    DeviceKind::Addon => d.installed,
                    _ => false,
                })
                .filter(|d| d.status != DeviceStatus::NeedsPairing)
                .map(|d| {
                    let verbs: &[&str] = match d.kind {
                        DeviceKind::Phone => PHONE_VERBS,
                        DeviceKind::Mac => MAC_VERBS,
                        _ => &["addon_run_task"],
                    };
                    json!({
                        "device_id": d.id, "kind": d.kind, "name": d.name, "status": d.status,
                        "enabled": d.enabled, "enabled_until": d.enabled_until, "verbs": verbs,
                    })
                })
                .collect();
            (StatusCode::OK, Json(json!({"ok": true, "devices": rows})))
        }
        Err(error) => failure(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallBody {
    device_id: String,
    verb: String,
    #[serde(default)]
    args: Value,
}

/// The body is parsed only after the token check, so a caller without a
/// token always gets 401, whatever it sent.
async fn call(State(s): State<DevicesRuntimeRouterState>, headers: HeaderMap, body: Bytes) -> Reply {
    let (user_id, conversation_id) = match authenticate(&s, &headers) {
        Ok(c) => c,
        Err(reply) => return reply,
    };
    let Ok(body) = serde_json::from_slice::<CallBody>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "code": "DEVICE_BAD_MESSAGE", "message": "expected {device_id, verb, args}"})),
        );
    };
    let args = if body.args.is_null() { json!({}) } else { body.args };
    let result = s
        .service
        .agent_call(AgentCall {
            user_id,
            conversation_id: Some(conversation_id),
            device_id: body.device_id,
            verb: body.verb,
            args,
        })
        .await;
    match result {
        Ok(result) => (StatusCode::OK, Json(json!({"ok": true, "result": result}))),
        Err(error) => failure(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddonRunBody {
    addon_id: String,
    phone_id: String,
    task: String,
    #[serde(default)]
    max_minutes: Option<u32>,
}

/// Run an add-on task. `DevicesService::addon_run_task` checks both
/// switches, asks the owner with the full task text and audits; nothing is
/// asked or checked twice here.
async fn addon_run(State(s): State<DevicesRuntimeRouterState>, headers: HeaderMap, body: Bytes) -> Reply {
    let (user_id, conversation_id) = match authenticate(&s, &headers) {
        Ok(c) => c,
        Err(reply) => return reply,
    };
    let Ok(body) = serde_json::from_slice::<AddonRunBody>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"ok": false, "code": "DEVICE_BAD_MESSAGE", "message": "expected {addon_id, phone_id, task, max_minutes?}"}),
            ),
        );
    };
    let outcome = s
        .service
        .addon_run_task(crate::addons::AddonTask {
            user_id,
            addon_id: body.addon_id,
            phone_id: body.phone_id,
            task: body.task,
            max_minutes: body.max_minutes,
            conversation_id: Some(conversation_id),
        })
        .await;
    match outcome {
        Ok(outcome) => (
            StatusCode::OK,
            Json(json!({"ok": true, "result": serde_json::to_value(outcome).unwrap_or(Value::Null)})),
        ),
        Err(error) => failure(error),
    }
}

fn failure(error: crate::error::DevicesError) -> Reply {
    match error.code() {
        Some(code) => (
            status_of(code),
            Json(json!({"ok": false, "code": code_str(code), "message": error.to_string()})),
        ),
        None => {
            tracing::error!(error = %error, "devices agent request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "code": "INTERNAL", "message": "devices request failed"})),
            )
        }
    }
}
