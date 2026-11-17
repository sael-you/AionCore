#![allow(clippy::disallowed_types)]
//! `/api/devices/*` (C38 W1). Handlers only translate; logic is in
//! `service.rs`. Mounted by the app behind the session-only auth state and
//! the trusted Origin guard, like `/api/browser/*`.

use aionui_api_types::ApiResponse;
use aionui_api_types::devices::{
    Device, DeviceAuditPage, DeviceAuditQuery, DeviceErrorCode, DeviceJob, DeviceJobStarted, DeviceRouteKind,
    DeviceRouteUpdate, DeviceUpdate,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};

use crate::error::DevicesError;
use crate::state::DevicesRouterState;

pub fn devices_routes(state: DevicesRouterState) -> Router {
    Router::new()
        .route("/api/devices", get(list))
        .route("/api/devices/jobs/{job_id}", get(job))
        .route("/api/devices/{id}", put(update))
        .route("/api/devices/{id}/routes/{route}", put(update_route))
        .route("/api/devices/{id}/install", post(install))
        .route("/api/devices/{id}/remove", post(remove))
        .route("/api/devices/{id}/audit", get(audit))
        .with_state(state)
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
    Json(body): Json<DeviceUpdate>,
) -> Result<Json<ApiResponse<Device>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        s.service.update(&user.id, &id, &body).await.map_err(api_error)?,
    )))
}

async fn update_route(
    State(s): State<DevicesRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, route)): Path<(String, String)>,
    Json(body): Json<DeviceRouteUpdate>,
) -> Result<Json<ApiResponse<Device>>, ApiError> {
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
