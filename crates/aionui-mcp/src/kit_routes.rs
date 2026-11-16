#![allow(clippy::disallowed_types)]

//! `/api/kits/*` routes (card C27, D49/D50). Merged into the MCP market
//! routers, so they get the same stacks: reads behind the ordinary auth
//! middleware; writes behind the session-only auth state + the shared strict
//! Origin rule (`OriginGuard`), each handler checking the [`AuthChannel`]
//! again (runtime-token and `--local` default user refused, 403).

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};

use aionui_api_types::{
    ApiResponse, AttachKitItemRequest, CreateKitRequest, KitAttachmentResponse, KitEffectiveQuery,
    KitEffectiveSetResponse, KitImportPreviewResponse, KitImportRequest, KitImportResponse, KitResponse,
    ListKitAttachmentsQuery, ListKitsQuery, UpdateKitRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::market_routes::{Channel, McpMarketRouterState, market_api_error, require_session};

pub fn kit_read_routes(state: McpMarketRouterState) -> Router {
    Router::new()
        .route("/api/kits", get(list_kits))
        .route("/api/kits/attachments", get(list_attachments))
        .route("/api/kits/effective", get(effective))
        .route("/api/kits/{id}", get(get_kit))
        .route("/api/projects/{project_id}/kit", get(get_project_kit))
        .with_state(state)
}

pub fn kit_write_routes(state: McpMarketRouterState) -> Router {
    Router::new()
        .route("/api/kits", post(create_kit))
        .route("/api/kits/attachments", post(attach))
        .route("/api/kits/attachments/{id}", delete(detach))
        .route("/api/kits/{id}", axum::routing::put(update_kit).delete(delete_kit))
        // C28 §5: both on the write stack (session channel + OriginGuard): the
        // preview reads a file that holds credentials, even if it returns none.
        .route("/api/projects/{project_id}/kit/import-preview", get(import_preview))
        .route("/api/projects/{project_id}/kit/import", post(import))
        .with_state(state)
}

// -- reads ------------------------------------------------------------------

async fn list_kits(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListKitsQuery>,
) -> Result<Json<ApiResponse<Vec<KitResponse>>>, ApiError> {
    let kits = state
        .market
        .list_kits(&user.id, query)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(kits)))
}

/// C28: the project kit of a project, or 404 `KIT_NOT_FOUND`.
async fn get_project_kit(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<KitResponse>>, ApiError> {
    let kit = state
        .market
        .get_project_kit(&user.id, &project_id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(kit)))
}

async fn get_kit(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<KitResponse>>, ApiError> {
    let kit = state.market.get_kit(&user.id, &id).await.map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(kit)))
}

async fn list_attachments(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListKitAttachmentsQuery>,
) -> Result<Json<ApiResponse<Vec<KitAttachmentResponse>>>, ApiError> {
    let rows = state
        .market
        .list_kit_attachments(&user.id, query)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(rows)))
}

async fn effective(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<KitEffectiveQuery>,
) -> Result<Json<ApiResponse<KitEffectiveSetResponse>>, ApiError> {
    let set = state
        .market
        .kit_effective_set(&user.id, query)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(set)))
}

// -- writes -----------------------------------------------------------------

async fn create_kit(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    body: Result<Json<CreateKitRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<KitResponse>>), ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let kit = state.market.create_kit(&user.id, req).await.map_err(market_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(kit))))
}

async fn update_kit(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
    body: Result<Json<UpdateKitRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KitResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let kit = state
        .market
        .update_kit(&user.id, &id, req)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(kit)))
}

async fn delete_kit(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_session(&channel)?;
    state.market.delete_kit(&user.id, &id).await.map_err(market_api_error)?;
    Ok(Json(ApiResponse::success()))
}

async fn attach(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    body: Result<Json<AttachKitItemRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KitAttachmentResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let row = state
        .market
        .attach_kit_item(&user.id, req)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(row)))
}

async fn detach(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_session(&channel)?;
    state
        .market
        .detach_kit_item(&user.id, &id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::success()))
}

/// C28 §5: which `.mcp.json` servers match the catalogue and which secret
/// fields the file holds (names and found flags only).
async fn import_preview(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<KitImportPreviewResponse>>, ApiError> {
    require_session(&channel)?;
    let preview = state
        .market
        .kit_import_preview(&user.id, &project_id)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(preview)))
}

/// C28 §5: create catalogue instances with the file's credentials and add
/// them to the project kit.
async fn import(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(project_id): Path<String>,
    body: Result<Json<KitImportRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<KitImportResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .market
        .kit_import(&user.id, &project_id, req)
        .await
        .map_err(market_api_error)?;
    Ok(Json(ApiResponse::ok(result)))
}
