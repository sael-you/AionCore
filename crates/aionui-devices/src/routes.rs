#![allow(clippy::disallowed_types)]
//! `/api/devices/*` (C38 W1). Handlers only translate; logic is in
//! `service.rs`. Mounted by the app behind the session-only auth state and
//! the trusted Origin guard, like `/api/browser/*`.

use aionui_api_types::ApiResponse;
use aionui_api_types::devices::{
    Device, DeviceAuditPage, DeviceAuditQuery, DeviceErrorCode, DeviceJob, DeviceJobStarted, DevicePairingCode,
    DevicePairingCodeRequest, DeviceRouteKind, DeviceRouteUpdate, DeviceUpdate, RELAY_MAX_FRAME_BYTES,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, Path, Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};

use crate::error::DevicesError;
use crate::state::DevicesRouterState;

pub fn devices_routes(state: DevicesRouterState) -> Router {
    Router::new()
        .route("/api/devices", get(list))
        .route("/api/devices/jobs/{job_id}", get(job))
        .route("/api/devices/pairing-codes", post(pairing_code))
        .route("/api/devices/approvals", get(list_approvals))
        .route("/api/devices/approvals/{approval_id}", post(decide_approval))
        .route("/api/devices/{id}", put(update))
        .route("/api/devices/{id}/routes/{route}", put(update_route))
        .route("/api/devices/{id}/install", post(install))
        .route("/api/devices/{id}/remove", post(remove))
        .route("/api/devices/{id}/audit", get(audit))
        .with_state(state)
}

/// `WS /api/devices/relay` (C38 W2): mounted WITHOUT the session layer and
/// outside the browser WebSocket-origin rule. Non-browser clients only: any
/// `Origin` header is refused; the first frame authenticates.
pub fn devices_relay_routes(state: DevicesRouterState) -> Router {
    Router::new()
        .route("/api/devices/relay", get(relay_ws))
        // The phone's browser has no session: the short-lived link token is
        // the credential (K8).
        .route("/api/devices/companion.apk", get(companion_apk))
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct ApkQuery {
    #[serde(default)]
    t: String,
}

async fn companion_apk(State(s): State<DevicesRouterState>, Query(q): Query<ApkQuery>) -> Response {
    let Some(downloads) = s.service.companion_downloads() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !downloads.check(&q.t) {
        return api_error(DevicesError::coded(
            DeviceErrorCode::DevicePairingInvalid,
            "the download link expired; make a new pairing code",
        ))
        .into_response();
    }
    match downloads.read_apk().await {
        Ok(bytes) => (
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/vnd.android.package-archive",
                ),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"aionui-companion.apk\"",
                ),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "companion APK not readable");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn relay_ws(
    State(s): State<DevicesRouterState>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return api_error(DevicesError::coded(
            DeviceErrorCode::DeviceOriginRefused,
            "the device relay does not accept browsers",
        ))
        .into_response();
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let service = s.service.clone();
    ws.max_message_size(RELAY_MAX_FRAME_BYTES)
        .max_frame_size(RELAY_MAX_FRAME_BYTES)
        .on_upgrade(move |socket| service.run_relay(socket))
}

/// The host the owner reaches AionUi under: the web host forwards it as
/// `X-Forwarded-Host` (it rewrites `Host`).
fn public_host(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|h| h.trim().to_owned())
        .filter(|h| {
            !h.is_empty()
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
        })
}

async fn pairing_code(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    body: Result<Json<DevicePairingCodeRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DevicePairingCode>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        s.service
            .pairing_code(&user.id, req.kind, public_host(&headers).as_deref())
            .map_err(api_error)?,
    )))
}

/// The wire string of a code (`DEVICE_NOT_FOUND`, …).
pub fn code_str(code: DeviceErrorCode) -> &'static str {
    match code {
        DeviceErrorCode::DeviceNotFound => "DEVICE_NOT_FOUND",
        DeviceErrorCode::DevicePairingInvalid => "DEVICE_PAIRING_INVALID",
        DeviceErrorCode::DeviceRevoked => "DEVICE_REVOKED",
        DeviceErrorCode::DeviceDisabled => "DEVICE_DISABLED",
        DeviceErrorCode::DeviceOffline => "DEVICE_OFFLINE",
        DeviceErrorCode::DeviceVerbRefused => "DEVICE_VERB_REFUSED",
        DeviceErrorCode::DeviceApprovalDenied => "DEVICE_APPROVAL_DENIED",
        DeviceErrorCode::DeviceApprovalTimeout => "DEVICE_APPROVAL_TIMEOUT",
        DeviceErrorCode::DeviceApprovalExpired => "DEVICE_APPROVAL_EXPIRED",
        DeviceErrorCode::DeviceBadMessage => "DEVICE_BAD_MESSAGE",
        DeviceErrorCode::DeviceBusy => "DEVICE_BUSY",
        DeviceErrorCode::DeviceJobNotFound => "DEVICE_JOB_NOT_FOUND",
        DeviceErrorCode::DeviceInUse => "DEVICE_IN_USE",
        DeviceErrorCode::DeviceOriginRefused => "DEVICE_ORIGIN_REFUSED",
    }
}

pub fn status_of(code: DeviceErrorCode) -> StatusCode {
    match code {
        DeviceErrorCode::DeviceNotFound | DeviceErrorCode::DeviceJobNotFound => StatusCode::NOT_FOUND,
        DeviceErrorCode::DeviceBadMessage => StatusCode::BAD_REQUEST,
        DeviceErrorCode::DevicePairingInvalid | DeviceErrorCode::DeviceRevoked => StatusCode::UNAUTHORIZED,
        DeviceErrorCode::DeviceDisabled
        | DeviceErrorCode::DeviceVerbRefused
        | DeviceErrorCode::DeviceApprovalDenied
        | DeviceErrorCode::DeviceOriginRefused => StatusCode::FORBIDDEN,
        DeviceErrorCode::DeviceBusy | DeviceErrorCode::DeviceInUse => StatusCode::CONFLICT,
        DeviceErrorCode::DeviceApprovalExpired => StatusCode::GONE,
        DeviceErrorCode::DeviceApprovalTimeout => StatusCode::REQUEST_TIMEOUT,
        DeviceErrorCode::DeviceOffline => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub fn api_error(e: DevicesError) -> ApiError {
    match e.code() {
        Some(code) => ApiError::coded(status_of(code), code_str(code), e.to_string(), None),
        None => {
            tracing::error!(error = %e, "devices request failed");
            ApiError::Internal("devices request failed".into())
        }
    }
}

async fn list_approvals(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Json<ApiResponse<Vec<aionui_api_types::devices::DeviceApproval>>> {
    Json(ApiResponse::ok(s.service.approvals(&user.id)))
}

async fn decide_approval(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(approval_id): Path<String>,
    body: Result<Json<aionui_api_types::devices::DeviceApprovalDecision>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(body) = body.map_err(ApiError::from)?;
    let allow = body.decision == aionui_api_types::devices::DeviceApprovalVerdict::Allow;
    s.service
        .decide_approval(&user.id, &approval_id, allow)
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn list(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<Device>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service.list(&user.id).await.map_err(api_error)?,
    )))
}

async fn update(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<DeviceUpdate>, JsonRejection>,
) -> Result<Json<ApiResponse<Device>>, ApiError> {
    let Json(body) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        s.service.update(&user.id, &id, &body).await.map_err(api_error)?,
    )))
}

async fn update_route(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, route)): Path<(String, String)>,
    body: Result<Json<DeviceRouteUpdate>, JsonRejection>,
) -> Result<Json<ApiResponse<Device>>, ApiError> {
    let Json(body) = body.map_err(ApiError::from)?;
    let route: DeviceRouteKind = serde_json::from_value(serde_json::Value::String(route))
        .map_err(|_| api_error(DevicesError::coded(DeviceErrorCode::DeviceBadMessage, "unknown route")))?;
    Ok(Json(ApiResponse::ok(
        s.service
            .update_route(&user.id, &id, route, body.enabled)
            .await
            .map_err(api_error)?,
    )))
}

async fn install(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeviceJobStarted>>, ApiError> {
    let job_id = s.service.start_job(&user.id, &id, false).await.map_err(api_error)?;
    Ok(Json(ApiResponse::ok(DeviceJobStarted { job_id })))
}

async fn remove(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeviceJobStarted>>, ApiError> {
    let job_id = s.service.start_job(&user.id, &id, true).await.map_err(api_error)?;
    Ok(Json(ApiResponse::ok(DeviceJobStarted { job_id })))
}

async fn job(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(job_id): Path<String>,
) -> Result<Json<ApiResponse<DeviceJob>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service.job(&user.id, &job_id).map_err(api_error)?,
    )))
}

async fn audit(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(q): Query<DeviceAuditQuery>,
) -> Result<Json<ApiResponse<DeviceAuditPage>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service
            .audit(&user.id, &id, q.cursor.as_deref(), q.limit)
            .await
            .map_err(api_error)?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_maps_to_its_wire_string() {
        for code in [
            DeviceErrorCode::DeviceNotFound,
            DeviceErrorCode::DevicePairingInvalid,
            DeviceErrorCode::DeviceRevoked,
            DeviceErrorCode::DeviceDisabled,
            DeviceErrorCode::DeviceOffline,
            DeviceErrorCode::DeviceVerbRefused,
            DeviceErrorCode::DeviceApprovalDenied,
            DeviceErrorCode::DeviceApprovalTimeout,
            DeviceErrorCode::DeviceApprovalExpired,
            DeviceErrorCode::DeviceBadMessage,
            DeviceErrorCode::DeviceBusy,
            DeviceErrorCode::DeviceJobNotFound,
            DeviceErrorCode::DeviceInUse,
            DeviceErrorCode::DeviceOriginRefused,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), code_str(code));
        }
        assert_eq!(status_of(DeviceErrorCode::DeviceInUse), StatusCode::CONFLICT);
    }
}
