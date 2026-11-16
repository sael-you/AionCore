#![allow(clippy::disallowed_types)]
//! `/api/browser/*` (C30). Handlers only translate; logic is in `service.rs`.
//! Mounted by the app behind the session-only auth state and the trusted
//! Origin guard (like the MCP market writes): never the runtime-token channel.

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, BROWSER_EXTENSION_ID, BrowserActivityQuery, BrowserApproval, BrowserApprovalDecision, BrowserDevice,
    BrowserDeviceActivity, BrowserErrorCode, BrowserPairingCode, BrowserSettings, BrowserSettingsUpdate,
    BrowserStatusResponse, BrowserWsQuery,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
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
        .route("/api/browser/devices/pairing-code", post(pairing_code))
        .route("/api/browser/devices", get(list_devices))
        .route("/api/browser/devices/{device_id}", delete(revoke_device))
        .route("/api/browser/devices/{device_id}/activity", get(device_activity))
        .route("/api/browser/approvals", get(list_approvals))
        .route("/api/browser/approvals/{approval_id}", post(decide_approval))
        .with_state(state)
}

/// `WS /api/browser/extension` (C34): mounted WITHOUT the session layer. The
/// extension has no AionUi cookie; it authenticates with its first frame (a
/// one-time pairing code or its device token, within 5 s). Only the pinned
/// extension origin may open it (the gateway checks the same).
pub fn browser_extension_routes(state: BrowserRouterState) -> Router {
    Router::new()
        .route("/api/browser/extension", get(extension_ws))
        .with_state(state)
}

pub fn extension_origin() -> String {
    format!("chrome-extension://{BROWSER_EXTENSION_ID}")
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

async fn pairing_code(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<BrowserPairingCode>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service.device_hub().pairing_code(&user.id).map_err(api_error)?,
    )))
}

async fn list_devices(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Json<ApiResponse<Vec<BrowserDevice>>> {
    Json(ApiResponse::ok(s.service.device_hub().devices(&user.id)))
}

async fn revoke_device(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(device_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    s.service.revoke_device(&user.id, &device_id).map_err(api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn device_activity(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(device_id): Path<String>,
    Query(q): Query<BrowserActivityQuery>,
) -> Result<Json<ApiResponse<Vec<BrowserDeviceActivity>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service
            .device_hub()
            .activity(&user.id, &device_id, q.limit)
            .map_err(api_error)?,
    )))
}

async fn list_approvals(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Json<ApiResponse<Vec<BrowserApproval>>> {
    Json(ApiResponse::ok(s.service.device_hub().approvals().list(&user.id)))
}

async fn decide_approval(
    State(s): State<BrowserRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(approval_id): Path<String>,
    Json(d): Json<BrowserApprovalDecision>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    s.service
        .device_hub()
        .approvals()
        .decide(&user.id, &approval_id, d.approve)
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn extension_ws(State(s): State<BrowserRouterState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let origin_ok = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|o| o == extension_origin());
    if !origin_ok {
        return axum::response::IntoResponse::into_response(ApiError::coded(
            StatusCode::FORBIDDEN,
            code_str(BrowserErrorCode::BrowserBadMessage),
            "this socket is for the AionUi browser extension".to_owned(),
            None,
        ));
    }
    learn_host(&s, &headers);
    let hub = s.service.device_hub().clone();
    ws.max_message_size(crate::relay::MAX_EXTENSION_FRAME)
        .on_upgrade(move |socket| hub.run_extension(socket))
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
