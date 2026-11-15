//! Voice user routes. Handlers only translate HTTP ⇄ service calls; every
//! route sits behind the guards of [`crate::guard::guard_voice_router`].
#![allow(clippy::disallowed_types)]

use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{
    ApiResponse, CreateVoiceSessionRequest, CreateVoiceSessionResponse, EndVoiceSessionRequest,
    UpdateVoiceSettingsRequest, VoiceSessionResponse, VoiceSettingsResponse, VoiceStatusResponse,
};
use aionui_auth::{CurrentUser, RateLimiter, authenticated_action_rate_limit_middleware, extract_cookie_value};
use aionui_common::ApiError;
use aionui_common::constants::COOKIE_NAME;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::error::VoiceError;
use crate::guard::{VoiceHttpGuards, guard_voice_router};
use crate::lifecycle::EndReason;
use crate::state::VoiceRouterState;
use crate::ws::{MAX_CONTROL_FRAME_BYTES, handle_voice_socket};

impl From<VoiceError> for ApiError {
    fn from(err: VoiceError) -> Self {
        match err {
            VoiceError::Disabled => {
                ApiError::coded(StatusCode::FORBIDDEN, "VOICE_DISABLED", "Voice is turned off.", None)
            }
            VoiceError::Busy => ApiError::coded(
                StatusCode::CONFLICT,
                "VOICE_BUSY",
                "A voice call is already live on another device.",
                None,
            ),
            VoiceError::BudgetReached => ApiError::coded(
                StatusCode::TOO_MANY_REQUESTS,
                "VOICE_BUDGET_REACHED",
                "Today's voice budget is used up.",
                None,
            ),
            VoiceError::NoProvider => ApiError::coded(
                StatusCode::PRECONDITION_FAILED,
                "VOICE_NO_PROVIDER",
                "Add an enabled OpenAI provider (official API URL) to use voice.",
                None,
            ),
            VoiceError::NotFound(msg) => ApiError::NotFound(msg),
            VoiceError::BadRequest(msg) => ApiError::BadRequest(msg),
            VoiceError::Provider(_) => ApiError::BadGateway("Voice provider unavailable.".into()),
            VoiceError::Internal(_) => ApiError::Internal("Voice service error.".into()),
        }
    }
}

/// The voice user router with all §3.5 guards applied.
pub fn voice_routes(state: VoiceRouterState, guards: VoiceHttpGuards) -> Router {
    // Admissions and ends are rate limited per user (shared limiter, 20/min).
    let action_limiter = Arc::new(RateLimiter::authenticated_action());
    action_limiter.start_cleanup_task(Duration::from_secs(60));
    let actions = Router::new()
        .route("/api/voice/sessions", post(create_session))
        .route("/api/voice/sessions/{id}/end", post(end_session))
        .route_layer(from_fn_with_state(
            action_limiter,
            authenticated_action_rate_limit_middleware,
        ));
    let routes = Router::new()
        .route("/api/voice/sessions", get(list_sessions))
        .route("/api/voice/settings", get(get_settings).put(put_settings))
        .route("/api/voice/status", get(status))
        .route("/api/voice/ws", get(voice_ws))
        .merge(actions);
    guard_voice_router(routes, guards).with_state(state)
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    extract_cookie_value(headers, COOKIE_NAME)
}

async fn create_session(
    State(state): State<VoiceRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    body: Result<Json<CreateVoiceSessionRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<CreateVoiceSessionResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let created = state
        .service
        .create_session(&user.id, session_token(&headers), req)
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn list_sessions(
    State(state): State<VoiceRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<VoiceSessionResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.list_sessions(&user.id).await?)))
}

/// Body is optional: `{reason}`.
async fn end_session(
    State(state): State<VoiceRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<ApiResponse<VoiceSessionResponse>>, ApiError> {
    let req: EndVoiceSessionRequest = if body.iter().all(u8::is_ascii_whitespace) {
        EndVoiceSessionRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("Invalid JSON request body.".into()))?
    };
    let reason = EndReason::from_client(req.reason.as_deref());
    Ok(Json(ApiResponse::ok(
        state.service.end_session(&user.id, &id, reason).await?,
    )))
}

async fn get_settings(
    State(state): State<VoiceRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<VoiceSettingsResponse>>, ApiError> {
    let settings = state.service.settings(&user.id).await?;
    Ok(Json(ApiResponse::ok(
        state.service.settings_response(&user.id, &settings).await?,
    )))
}

async fn put_settings(
    State(state): State<VoiceRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<UpdateVoiceSettingsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<VoiceSettingsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let settings = state.service.update_settings(&user.id, &req).await?;
    Ok(Json(ApiResponse::ok(
        state.service.settings_response(&user.id, &settings).await?,
    )))
}

async fn status(
    State(state): State<VoiceRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<VoiceStatusResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.status(&user.id).await?)))
}

async fn voice_ws(
    State(state): State<VoiceRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let token = session_token(&headers);
    let service = state.service.clone();
    upgrade
        .max_message_size(MAX_CONTROL_FRAME_BYTES)
        .max_frame_size(MAX_CONTROL_FRAME_BYTES)
        .on_upgrade(move |socket| handle_voice_socket(socket, user.id, token, service))
}
