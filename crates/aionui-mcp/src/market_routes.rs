#![allow(clippy::disallowed_types)]

//! `/api/mcp/market/*` routes (card C15). Two routers:
//! - [`mcp_market_read_routes`]: reads (refs, `set` flags, status, snapshot,
//!   and the C27 kit reads of `kit_routes`),
//!   mounted behind the ordinary auth middleware;
//! - [`mcp_market_write_routes`]: every market write (instances, approval
//!   record, secrets, assignments, C27 kits and attachments). The composition layer mounts it behind an
//!   auth state that accepts a verified browser session only (no runtime-token
//!   channel, no `--local` default user, R1 #1), and each handler checks the
//!   [`AuthChannel`] again, failing closed.

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};

use aionui_api_types::{
    ApiResponse, CreateMcpMarketAssignmentRequest, CreateMcpMarketInstanceRequest, DuplicateMcpMarketInstanceRequest,
    ListMcpMarketAssignmentsQuery, McpMarketAssignmentResponse, McpMarketInstanceResponse, McpMarketSecretStatus,
    McpMarketSnapshotQuery, McpMarketStatusResponse, McpMarketTemplate, McpMarketTemplateRefreshResponse,
    SetMcpMarketApprovalRequest, SetMcpMarketSecretRequest, UpdateMcpMarketInstanceRequest,
};
use aionui_auth::{AuthChannel, CurrentUser};
use aionui_common::ApiError;

use crate::market::{MarketError, McpMarketService};

#[derive(Clone)]
pub struct McpMarketRouterState {
    pub market: McpMarketService,
}

pub(crate) fn market_api_error(err: MarketError) -> ApiError {
    match err {
        MarketError::Invalid(msg) => ApiError::coded(StatusCode::BAD_REQUEST, "MCP_MARKET_INVALID", msg, None),
        MarketError::TemplateSecretMissing(msg) => {
            ApiError::coded(StatusCode::CONFLICT, "MCP_TEMPLATE_SECRET_MISSING", msg, None)
        }
        MarketError::NotFound(msg) => ApiError::NotFound(msg),
        MarketError::Conflict(msg) => ApiError::Conflict(msg),
        MarketError::Forbidden(msg) => ApiError::coded(StatusCode::FORBIDDEN, "MCP_MARKET_FORBIDDEN", msg, None),
        MarketError::Unavailable(msg) => {
            ApiError::coded(StatusCode::SERVICE_UNAVAILABLE, "MCP_MARKET_UNAVAILABLE", msg, None)
        }
        MarketError::Internal(msg) => ApiError::Internal(msg),
        MarketError::Kit(code, msg) => ApiError::coded(
            StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::BAD_REQUEST),
            code.as_str(),
            msg,
            None,
        ),
    }
}

pub(crate) type Channel = Option<Extension<AuthChannel>>;

/// Writes need a verified browser session; the runtime-token channel (agent
/// helper CLIs) and `--local`'s injected default user are refused.
pub(crate) fn require_session(channel: &Channel) -> Result<(), ApiError> {
    match channel.as_ref().map(|Extension(c)| c) {
        Some(AuthChannel::Session) => Ok(()),
        Some(AuthChannel::RuntimeToken { .. }) => Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "MCP_MARKET_SESSION_REQUIRED",
            "MCP market changes need the owner's browser session; agents cannot make them",
            None,
        )),
        Some(AuthChannel::Local) => Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "MCP_MARKET_SESSION_REQUIRED",
            "MCP market changes need a verified browser session (not available to local-mode callers)",
            None,
        )),
        None => {
            tracing::error!("mcp market write route reached without an AuthChannel; refusing");
            Err(ApiError::Internal("authentication context missing".into()))
        }
    }
}

pub fn mcp_market_read_routes(state: McpMarketRouterState) -> Router {
    Router::new()
        .route("/api/mcp/market/templates", get(list_templates))
        .route("/api/mcp/market/instances", get(list_instances))
        .route("/api/mcp/market/instances/{id}", get(get_instance))
        .route("/api/mcp/market/secrets", get(list_secrets))
        .route("/api/mcp/market/assignments", get(list_assignments))
        .route("/api/mcp/market/status", get(status))
        .route("/api/mcp/market/snapshot", get(snapshot))
        .with_state(state.clone())
        .merge(crate::kit_routes::kit_read_routes(state.clone()))
        .merge(crate::server_routes::server_read_routes(state))
}

pub fn mcp_market_write_routes(state: McpMarketRouterState) -> Router {
    Router::new()
        .route("/api/mcp/market/instances", post(create_instance))
        .route(
            "/api/mcp/market/instances/{id}",
            put(update_instance).delete(delete_instance),
        )
        .route("/api/mcp/market/instances/{id}/duplicate", post(duplicate_instance))
        .route("/api/mcp/market/instances/{id}/revoke", post(revoke_instance))
        .route(
            "/api/mcp/market/instances/{id}/refresh-template",
            post(refresh_template),
        )
        .route("/api/mcp/market/instances/{id}/approval", put(set_approval))
        .route(
            "/api/mcp/market/instances/{id}/secrets/{key}",
            put(set_secret).delete(clear_secret),
        )
        .route("/api/mcp/market/assignments", post(create_assignments))
        .route(
            "/api/mcp/market/assignments/{id}",
            axum::routing::delete(delete_assignment),
        )
        .with_state(state.clone())
        .merge(crate::kit_routes::kit_write_routes(state.clone()))
        .merge(crate::server_routes::server_write_routes(state))
}

// -- reads ------------------------------------------------------------------

async fn list_templates(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<McpMarketTemplate>>>, ApiError> {
    let templates = state.market.list_templates(&user.id).await.map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(templates)))
}

async fn list_instances(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<McpMarketInstanceResponse>>>, ApiError> {
    let list = state.market.list_instances(&user.id).await.map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(list)))
}

async fn get_instance(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<McpMarketInstanceResponse>>, ApiError> {
    let inst = state
        .market
        .get_instance(&user.id, &id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(inst)))
}

async fn list_secrets(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<McpMarketSecretStatus>>>, ApiError> {
    let list = state
        .market
        .list_secret_status(&user.id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(list)))
}

async fn list_assignments(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<ListMcpMarketAssignmentsQuery>,
) -> Result<Json<ApiResponse<Vec<McpMarketAssignmentResponse>>>, ApiError> {
    let list = state
        .market
        .list_assignments(&user.id, q.project_id.as_deref(), q.instance_id.as_deref())
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(list)))
}

async fn status(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<McpMarketStatusResponse>>, ApiError> {
    let s = state.market.status(&user.id).await.map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(s)))
}

async fn snapshot(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<McpMarketSnapshotQuery>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let lock = state
        .market
        .snapshot(&user.id, q.project_id.as_deref(), q.global.unwrap_or(false))
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(lock)))
}

// -- writes (verified browser session only) ---------------------------------

async fn create_instance(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    body: Result<Json<CreateMcpMarketInstanceRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<McpMarketInstanceResponse>>), ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let inst = state
        .market
        .create_instance(&user.id, req)
        .await
        .map_err(market_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(inst))))
}

async fn update_instance(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
    body: Result<Json<UpdateMcpMarketInstanceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<McpMarketInstanceResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let inst = state
        .market
        .update_instance(&user.id, &id, req)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(inst)))
}

async fn delete_instance(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_session(&channel)?;
    state
        .market
        .delete_instance(&user.id, &id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::success()))
}

async fn duplicate_instance(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
    body: Result<Json<DuplicateMcpMarketInstanceRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<McpMarketInstanceResponse>>), ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let inst = state
        .market
        .duplicate_instance(&user.id, &id, &req.name)
        .await
        .map_err(market_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(inst))))
}

async fn revoke_instance(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<McpMarketInstanceResponse>>, ApiError> {
    require_session(&channel)?;
    let inst = state
        .market
        .revoke_instance(&user.id, &id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(inst)))
}

async fn refresh_template(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<McpMarketTemplateRefreshResponse>>, ApiError> {
    require_session(&channel)?;
    let refreshed = state
        .market
        .refresh_template(&user.id, &id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(refreshed)))
}

async fn set_approval(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
    body: Result<Json<SetMcpMarketApprovalRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<McpMarketInstanceResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let inst = state
        .market
        .set_approval(&user.id, &id, req.approval_ref)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(inst)))
}

async fn set_secret(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path((id, key)): Path<(String, String)>,
    body: Result<Json<SetMcpMarketSecretRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<McpMarketSecretStatus>>, ApiError> {
    require_session(&channel)?;
    // The rejection text of a malformed body never echoes the body.
    let Json(req) = body.map_err(|_| ApiError::BadRequest("body must be {\"value\": \"...\"}".into()))?;
    let status = state
        .market
        .set_secret(&user.id, &id, &key, &req.value)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(status)))
}

async fn clear_secret(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path((id, key)): Path<(String, String)>,
) -> Result<Json<ApiResponse<McpMarketSecretStatus>>, ApiError> {
    require_session(&channel)?;
    let status = state
        .market
        .clear_secret(&user.id, &id, &key)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(status)))
}

async fn create_assignments(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    body: Result<Json<CreateMcpMarketAssignmentRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<Vec<McpMarketAssignmentResponse>>>), ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let rows = state
        .market
        .create_assignments(&user.id, req)
        .await
        .map_err(market_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(rows))))
}

async fn delete_assignment(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_session(&channel)?;
    state
        .market
        .delete_assignment(&user.id, &id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::success()))
}
