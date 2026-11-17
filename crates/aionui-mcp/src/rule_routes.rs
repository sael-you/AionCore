#![allow(clippy::disallowed_types)]

//! `/api/rules/*` routes (card C37, TASK-131). Merged into the MCP market
//! routers, so they get the same stacks: reads behind the ordinary auth
//! middleware; writes (CRUD, import, file hand-over) behind the session-only
//! auth state + the shared strict Origin rule, each handler checking the
//! channel again (runtime-token and `--local` default user refused, 403):
//! an agent can never write the rules it is bound by. The import preview
//! reads repository files, so it sits on the write stack too.

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};

use aionui_api_types::{
    ApiResponse, CreateRuleRequest, RuleEffectiveQuery, RuleEffectiveResponse, RuleImportPreviewResponse,
    RuleImportQuery, RuleImportRequest, RuleImportResponse, RuleManageFilesRequest, RuleManageFilesResponse,
    RuleResponse, RuleSourceResponse, UpdateRuleRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::market_routes::{Channel, McpMarketRouterState, require_session};
use crate::rules::{RuleError, RuleService};

pub fn rule_read_routes(state: McpMarketRouterState) -> Router {
    Router::new()
        .route("/api/rules", get(list))
        .route("/api/rules/effective", get(effective))
        .route("/api/rules/sources", get(sources))
        .route("/api/rules/{id}", get(get_one))
        .with_state(state)
}

pub fn rule_write_routes(state: McpMarketRouterState) -> Router {
    Router::new()
        .route("/api/rules", post(create))
        .route("/api/rules/import/preview", get(import_preview))
        .route("/api/rules/import", post(import))
        .route("/api/rules/files/manage", post(manage_files))
        .route("/api/rules/{id}", put(update).delete(delete_one))
        .with_state(state)
}

pub(crate) fn rule_api_error(err: RuleError) -> ApiError {
    let coded = |status, code, msg| ApiError::coded(status, code, msg, None);
    match err {
        RuleError::Invalid(msg) => coded(StatusCode::BAD_REQUEST, "RULE_INVALID", msg),
        RuleError::ConfirmRequired(msg) => coded(StatusCode::BAD_REQUEST, "RULE_CONFIRM_REQUIRED", msg),
        RuleError::NameTaken(msg) => coded(StatusCode::CONFLICT, "RULE_NAME_TAKEN", msg),
        RuleError::Stale(msg) => coded(StatusCode::CONFLICT, "RULE_STALE", msg),
        RuleError::AlreadyImported(msg) => coded(StatusCode::CONFLICT, "RULE_ALREADY_IMPORTED", msg),
        RuleError::NotFound(msg) => coded(StatusCode::NOT_FOUND, "RULE_NOT_FOUND", msg),
        RuleError::Internal(msg) => ApiError::Internal(msg),
    }
}

fn rules(state: &McpMarketRouterState) -> Result<&RuleService, ApiError> {
    state.market.rules().ok_or_else(|| {
        ApiError::coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "RULES_UNAVAILABLE",
            "the rules service is not configured",
            None,
        )
    })
}

async fn list(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<RuleResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        rules(&state)?.list(&user.id).await.map_err(rule_api_error)?,
    )))
}

async fn get_one(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RuleResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        rules(&state)?.get(&user.id, &id).await.map_err(rule_api_error)?,
    )))
}

async fn effective(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<RuleEffectiveQuery>,
) -> Result<Json<ApiResponse<RuleEffectiveResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        rules(&state)?
            .effective(&user.id, query)
            .await
            .map_err(rule_api_error)?,
    )))
}

async fn sources(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<RuleSourceResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        rules(&state)?.sources(&user.id).await.map_err(rule_api_error)?,
    )))
}

async fn create(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    body: Result<Json<CreateRuleRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<RuleResponse>>), ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let rule = rules(&state)?.create(&user.id, req).await.map_err(rule_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(rule))))
}

async fn update(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
    body: Result<Json<UpdateRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RuleResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let rule = rules(&state)?
        .update(&user.id, &id, req)
        .await
        .map_err(rule_api_error)?;
    Ok(Json(ApiResponse::ok(rule)))
}

async fn delete_one(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    require_session(&channel)?;
    rules(&state)?.delete(&user.id, &id).await.map_err(rule_api_error)?;
    Ok(Json(ApiResponse::success()))
}

async fn import_preview(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    Query(query): Query<RuleImportQuery>,
) -> Result<Json<ApiResponse<RuleImportPreviewResponse>>, ApiError> {
    require_session(&channel)?;
    Ok(Json(ApiResponse::ok(
        rules(&state)?
            .import_preview(&user.id, query)
            .await
            .map_err(rule_api_error)?,
    )))
}

async fn import(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    body: Result<Json<RuleImportRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RuleImportResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        rules(&state)?.import(&user.id, req).await.map_err(rule_api_error)?,
    )))
}

async fn manage_files(
    State(state): State<McpMarketRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    body: Result<Json<RuleManageFilesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RuleManageFilesResponse>>, ApiError> {
    require_session(&channel)?;
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        rules(&state)?
            .manage_files(&user.id, req)
            .await
            .map_err(rule_api_error)?,
    )))
}
