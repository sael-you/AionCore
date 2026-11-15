//! Agent → user routes (§3.4, §3.5 "agent routes"): `/api/runtime/voice/*`,
//! mounted outside auth/CSRF like the other runtime routes. Every call is
//! validated with the conversation-helper runtime token, bound to the
//! (user, conversation) pair in the `x-aionui-*` headers.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{ApiResponse, VoiceAgentCapabilities, VoiceAgentReply, VoiceAskRequest, VoiceSayRequest};
use aionui_auth::{
    IRuntimeTokenVerifier, RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_TOKEN_HEADER, RUNTIME_USER_ID_HEADER,
};
use aionui_common::ApiError;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::agent::{MAX_WAIT, SAY_PER_MINUTE};
use crate::service::VoiceService;

#[derive(Clone)]
pub struct VoiceAgentRouterState {
    pub service: VoiceService,
    pub verifier: Arc<dyn IRuntimeTokenVerifier>,
}

pub fn voice_agent_routes(state: VoiceAgentRouterState) -> Router {
    Router::new()
        .route("/api/runtime/voice/say", post(say))
        .route("/api/runtime/voice/ask", post(ask))
        .route("/api/runtime/voice/ask/{id}", get(wait))
        .route("/api/runtime/voice/capabilities", get(capabilities))
        .with_state(state)
}

struct Caller {
    user_id: String,
    conversation_id: String,
}

fn caller(state: &VoiceAgentRouterState, headers: &HeaderMap) -> Result<Caller, ApiError> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let (Some(token), Some(user_id), Some(conversation_id)) = (
        header(RUNTIME_TOKEN_HEADER),
        header(RUNTIME_USER_ID_HEADER),
        header(RUNTIME_CONVERSATION_ID_HEADER),
    ) else {
        tracing::warn!("voice agent route refused: runtime headers missing");
        return Err(ApiError::Unauthorized("runtime auth failed".into()));
    };
    if !state
        .verifier
        .verify_conversation_helper(&token, &user_id, &conversation_id)
    {
        tracing::warn!("voice agent route refused: runtime token rejected");
        return Err(ApiError::Unauthorized("runtime auth failed".into()));
    }
    Ok(Caller {
        user_id,
        conversation_id,
    })
}

async fn say(
    State(state): State<VoiceAgentRouterState>,
    headers: HeaderMap,
    body: Result<Json<VoiceSayRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<VoiceAgentReply>>, ApiError> {
    let caller = caller(&state, &headers)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let reply = state
        .service
        .agent_say(&caller.user_id, &caller.conversation_id, &req.text)
        .await?;
    Ok(Json(ApiResponse::ok(reply)))
}

async fn ask(
    State(state): State<VoiceAgentRouterState>,
    headers: HeaderMap,
    body: Result<Json<VoiceAskRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<VoiceAgentReply>>, ApiError> {
    let caller = caller(&state, &headers)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let timeout = Duration::from_secs(req.timeout_s.unwrap_or(MAX_WAIT.as_secs()));
    let reply = state
        .service
        .agent_ask(&caller.user_id, &caller.conversation_id, &req.question, timeout)
        .await?;
    Ok(Json(ApiResponse::ok(reply)))
}

async fn wait(
    State(state): State<VoiceAgentRouterState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<VoiceAgentReply>>, ApiError> {
    let caller = caller(&state, &headers)?;
    let reply = state
        .service
        .agent_wait(&caller.user_id, &caller.conversation_id, &id)
        .await?;
    Ok(Json(ApiResponse::ok(reply)))
}

async fn capabilities(
    State(state): State<VoiceAgentRouterState>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<VoiceAgentCapabilities>>, ApiError> {
    let caller = caller(&state, &headers)?;
    let bound = state.service.is_bound(&caller.user_id, &caller.conversation_id);
    Ok(Json(ApiResponse::ok(VoiceAgentCapabilities {
        max_wait_s: MAX_WAIT.as_secs(),
        say_per_minute: SAY_PER_MINUTE,
        note: "A voice answer is never permission for a destructive action; ask for an approval instead.".into(),
        voice_mode: if bound { "on" } else { "off" }.into(),
    })))
}
