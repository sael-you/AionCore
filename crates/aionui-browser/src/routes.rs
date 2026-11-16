#![allow(clippy::disallowed_types)]
//! `/api/browser/*` (C30). Handlers only translate; logic is in `service.rs`.
//! Mounted by the app behind the session-only auth state and the trusted
//! Origin guard (like the MCP market writes): never the runtime-token channel.

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, BrowserErrorCode, BrowserSettings, BrowserSettingsUpdate, BrowserStatusResponse, BrowserWsQuery,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::error::BrowserError;
use crate::panel::{PanelBackend, run_panel};
use crate::service::BrowserService;

#[derive(Clone)]
pub struct BrowserRouterState {
    pub service: Arc<BrowserService>,
}

pub fn browser_routes(state: BrowserRouterState) -> Router {
    Router::new()
        .route("/api/browser/settings", get(get_settings).put(put_settings))
        .route("/api/browser/status", get(get_status))
        .route("/api/browser/instances/{n}/clear", post(clear_instance))
        .route("/api/browser/instances/{n}/stop", post(stop_instance))
        .route("/api/browser/ws", get(panel_ws))
        .with_state(state)
}

pub fn code_str(code: BrowserErrorCode) -> &'static str {
    match code {
        BrowserErrorCode::BrowserUnavailable => "BROWSER_UNAVAILABLE",
        BrowserErrorCode::BrowserStartFailed => "BROWSER_START_FAILED",
        BrowserErrorCode::BrowserNotRunning => "BROWSER_NOT_RUNNING",
        BrowserErrorCode::BrowserUrlBlocked => "BROWSER_URL_BLOCKED",
        BrowserErrorCode::BrowserAgentsDisabled => "BROWSER_AGENTS_DISABLED",
        BrowserErrorCode::BrowserMethodBlocked => "BROWSER_METHOD_BLOCKED",
        BrowserErrorCode::BrowserTabLimit => "BROWSER_TAB_LIMIT",
        BrowserErrorCode::BrowserInstanceLimit => "BROWSER_INSTANCE_LIMIT",
        BrowserErrorCode::BrowserSettingOutOfRange => "BROWSER_SETTING_OUT_OF_RANGE",
        BrowserErrorCode::BrowserTabNotFound => "BROWSER_TAB_NOT_FOUND",
        BrowserErrorCode::BrowserBadMessage => "BROWSER_BAD_MESSAGE",
        BrowserErrorCode::BrowserBusy => "BROWSER_BUSY",
        BrowserErrorCode::BrowserPairingInvalid => "BROWSER_PAIRING_INVALID",
        BrowserErrorCode::BrowserDeviceRevoked => "BROWSER_DEVICE_REVOKED",
        BrowserErrorCode::BrowserDeviceNotFound => "BROWSER_DEVICE_NOT_FOUND",
        BrowserErrorCode::BrowserApprovalDenied => "BROWSER_APPROVAL_DENIED",
        BrowserErrorCode::BrowserApprovalTimeout => "BROWSER_APPROVAL_TIMEOUT",
        BrowserErrorCode::BrowserApprovalExpired => "BROWSER_APPROVAL_EXPIRED",
    }
}

/// The host the owner reaches AionUi under: the web host forwards it as
/// `X-Forwarded-Host` (it rewrites `Host`).
fn learn_host(s: &BrowserRouterState, headers: &HeaderMap) {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next());
    if let Some(host) = host {
        s.service.guard_ref().learn_aionui_host(host);
    }
}

fn api_error(e: BrowserError) -> ApiError {
    match e.code() {
        Some(code) => {
            let status = match code {
                BrowserErrorCode::BrowserSettingOutOfRange | BrowserErrorCode::BrowserBadMessage => {
                    StatusCode::BAD_REQUEST
                }
                BrowserErrorCode::BrowserInstanceLimit => StatusCode::BAD_REQUEST,
                BrowserErrorCode::BrowserBusy => StatusCode::CONFLICT,
                BrowserErrorCode::BrowserTabNotFound | BrowserErrorCode::BrowserDeviceNotFound => StatusCode::NOT_FOUND,
                BrowserErrorCode::BrowserApprovalExpired => StatusCode::GONE,
                BrowserErrorCode::BrowserPairingInvalid | BrowserErrorCode::BrowserDeviceRevoked => {
                    StatusCode::UNAUTHORIZED
                }
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            ApiError::coded(status, code_str(code), e.to_string(), None)
        }
        None => {
            tracing::error!(error = %e, "browser request failed");
            ApiError::Internal("browser request failed".into())
        }
    }
}

async fn get_settings(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<BrowserSettings>>, ApiError> {
    Ok(Json(ApiResponse::ok(s.service.settings(&user.id).map_err(api_error)?)))
}

async fn put_settings(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(update): Json<BrowserSettingsUpdate>,
) -> Result<Json<ApiResponse<BrowserSettings>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service.update_settings(&user.id, &update).await.map_err(api_error)?,
    )))
}

async fn get_status(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<BrowserStatusResponse>>, ApiError> {
    learn_host(&s, &headers);
    Ok(Json(ApiResponse::ok(s.service.status(&user.id).map_err(api_error)?)))
}

async fn clear_instance(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(n): Path<u8>,
) -> Result<Json<ApiResponse<BrowserStatusResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service.clear(&user.id, n).await.map_err(api_error)?,
    )))
}

async fn stop_instance(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(n): Path<u8>,
) -> Result<Json<ApiResponse<BrowserStatusResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service.stop(&user.id, n).await.map_err(api_error)?,
    )))
}

async fn panel_ws(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<BrowserWsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    learn_host(&s, &headers);
    let instance = q.instance.unwrap_or(1);
    let max = s.service.settings(&user.id).map_err(api_error)?.max_instances;
    if instance == 0 || instance > max {
        return Err(api_error(BrowserError::coded(
            BrowserErrorCode::BrowserInstanceLimit,
            "instance above the limit",
        )));
    }
    let backend: Arc<dyn PanelBackend> = s.service.clone();
    let user_id = user.id.clone();
    Ok(ws
        .max_message_size(1024 * 1024)
        .on_upgrade(move |socket| run_panel(socket, backend, user_id, instance)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_maps_to_its_wire_string() {
        for code in [
            BrowserErrorCode::BrowserUnavailable,
            BrowserErrorCode::BrowserStartFailed,
            BrowserErrorCode::BrowserNotRunning,
            BrowserErrorCode::BrowserUrlBlocked,
            BrowserErrorCode::BrowserAgentsDisabled,
            BrowserErrorCode::BrowserMethodBlocked,
            BrowserErrorCode::BrowserTabLimit,
            BrowserErrorCode::BrowserInstanceLimit,
            BrowserErrorCode::BrowserSettingOutOfRange,
            BrowserErrorCode::BrowserTabNotFound,
            BrowserErrorCode::BrowserBadMessage,
            BrowserErrorCode::BrowserBusy,
            BrowserErrorCode::BrowserPairingInvalid,
            BrowserErrorCode::BrowserDeviceRevoked,
            BrowserErrorCode::BrowserDeviceNotFound,
            BrowserErrorCode::BrowserApprovalDenied,
            BrowserErrorCode::BrowserApprovalTimeout,
            BrowserErrorCode::BrowserApprovalExpired,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), code_str(code));
        }
    }
}
