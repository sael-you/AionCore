#![allow(clippy::disallowed_types)]

//! `/api/servers/*` routes (card C36, TASK-130). Merged into the MCP market
//! routers, so they get the same stacks: reads behind the ordinary auth
//! middleware; writes behind the session-only auth state + the shared strict
//! Origin rule (`OriginGuard`), each handler checking the [`AuthChannel`]
//! again (runtime-token and `--local` default user refused, 403). Agents
//! never manage servers; they use them through the built-in server MCPs.
//!
//! [`AuthChannel`]: aionui_auth::AuthChannel

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;

use aionui_api_types::{
    ApiResponse, ClassifyServerCommandRequest, ClassifyServerCommandResponse, CreateServerRequest,
    ListServerAuditQuery, ScanHostKeyRequest, ScanHostKeyResponse, ServerAuditResponse, ServerResponse,
    ServerTestResponse, UpdateServerRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::market_routes::{Channel, McpMarketRouterState, require_session};
use crate::server_pool::{ServerPoolError, ServerPoolService};

pub fn server_read_routes(state: McpMarketRouterState) -> Router {
    Router::new()
        .route("/api/servers", get(list))
        .route("/api/servers/audit", get(audit))
        .route("/api/servers/classify", axum::routing::post(classify))
        .route("/api/servers/{id}", get(get_one))
        .with_state(state)
}

pub fn server_write_routes(state: McpMarketRouterState) -> Router {
    Router::new()
        .route("/api/servers", axum::routing::post(create))
        .route("/api/servers/scan-host-key", axum::routing::post(scan_host_key))
        .route("/api/servers/{id}/test", axum::routing::post(test_connection))
        .route("/api/servers/{id}", axum::routing::put(update).delete(delete_one))
        .with_state(state)
}

pub(crate) fn server_api_error(err: ServerPoolError) -> ApiError {
    match err {
        ServerPoolError::Invalid(msg) => ApiError::coded(StatusCode::BAD_REQUEST, "SERVER_INVALID", msg, None),
        ServerPoolError::NameTaken(msg) => ApiError::coded(StatusCode::CONFLICT, "SERVER_NAME_TAKEN", msg, None),
        ServerPoolError::NotFound(msg) => ApiError::coded(StatusCode::NOT_FOUND, "SERVER_NOT_FOUND", msg, None),
        ServerPoolError::Unreachable(msg) => ApiError::coded(StatusCode::BAD_GATEWAY, "SERVER_UNREACHABLE", msg, None),
        ServerPoolError::Internal(msg) => ApiError::Internal(msg),
    }
}

fn pool(state: &McpMarketRouterState) -> Result<&ServerPoolService, ApiError> {
    state.market.server_pool().ok_or_else(|| {
        ApiError::coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVER_POOL_UNAVAILABLE",
            "the server pool is not configured",
            None,
        )
    })
}

async fn list(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<ServerResponse>>>, ApiError> {
    let servers = pool(&state)?.list(&user.id).await.map_err(server_api_error)?;
    Ok(Json(ApiResponse::ok(servers)))
}

async fn get_one(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ServerResponse>>, ApiError> {
    let server = pool(&state)?.get(&user.id, &id).await.map_err(server_api_error)?;
    Ok(Json(ApiResponse::ok(server)))
}

async fn audit(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListServerAuditQuery>,
) -> Result<Json<ApiResponse<Vec<ServerAuditResponse>>>, ApiError> {
    let rows = pool(&state)?.audit(&user.id, query).await.map_err(server_api_error)?;
    Ok(Json(ApiResponse::ok(rows)))
}

async fn create(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    body: Result<Json<CreateServerRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ServerResponse>>), ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let server = pool(&state)?.create(&user.id, req).await.map_err(server_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(server))))
}

async fn update(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
    body: Result<Json<UpdateServerRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ServerResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let server = pool(&state)?
        .update(&user.id, &id, req)
        .await
        .map_err(server_api_error)?;
    Ok(Json(ApiResponse::ok(server)))
}

async fn delete_one(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_session(&channel)?;
    pool(&state)?.delete(&user.id, &id).await.map_err(server_api_error)?;
    Ok(Json(ApiResponse::success()))
}

/// Read stack: pure classification, runs nothing (the confirmation card uses it).
async fn classify(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ClassifyServerCommandRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ClassifyServerCommandResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let out = pool(&state)?.classify(&user.id, req).await.map_err(server_api_error)?;
    Ok(Json(ApiResponse::ok(out)))
}

async fn scan_host_key(
    State(state): State<McpMarketRouterState>,
    channel: Channel,
    body: Result<Json<ScanHostKeyRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ScanHostKeyResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let out = pool(&state)?.scan_host_key(req).await.map_err(server_api_error)?;
    Ok(Json(ApiResponse::ok(out)))
}

async fn test_connection(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ServerTestResponse>>, ApiError> {
    require_session(&channel)?;
    let pool = pool(&state)?;
    let out = pool
        .test_connection(&user.id, &id, pool.scratch_root())
        .await
        .map_err(server_api_error)?;
    Ok(Json(ApiResponse::ok(out)))
}
