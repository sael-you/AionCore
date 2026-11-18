// `ApiError` is the intended error type at this HTTP boundary.
#![allow(clippy::disallowed_types)]

//! Project page routes (card C42, TASK-169). Reads ride the ordinary auth
//! layer (owner-scoped by `CurrentUser`); the storage rescan is a write and
//! is mounted by the app behind the session-only auth state + the shared
//! trusted-Origin guard, like the project memory writes.

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, ProjectOverviewResponse, ProjectStorageResponse, ProjectUsageResponse, UsageQuery,
    UsageSummaryResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Router};

use crate::insights::{InsightsError, ProjectInsights};

#[derive(Clone)]
pub struct InsightsRouterState {
    pub insights: Arc<ProjectInsights>,
}

pub fn insights_read_routes(state: InsightsRouterState) -> Router {
    Router::new()
        .route("/api/projects/{project_id}/usage", get(project_usage))
        .route("/api/projects/{project_id}/storage", get(storage))
        .route("/api/projects/{project_id}/overview", get(overview))
        .route("/api/usage/summary", get(usage_summary))
        .with_state(state)
}

pub fn insights_write_routes(state: InsightsRouterState) -> Router {
    Router::new()
        .route("/api/projects/{project_id}/storage/rescan", post(rescan))
        .with_state(state)
}

fn insights_error(err: InsightsError) -> ApiError {
    match err {
        InsightsError::InvalidDays => {
            ApiError::coded(StatusCode::BAD_REQUEST, "usage_invalid_days", err.to_string(), None)
        }
        InsightsError::Project(project) => project.into(),
        InsightsError::Db(db) => {
            tracing::error!(error = %db, "project page: database error");
            ApiError::Internal("database error".to_owned())
        }
        InsightsError::Scan => ApiError::Internal(err.to_string()),
    }
}

async fn project_usage(
    State(state): State<InsightsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<ApiResponse<ProjectUsageResponse>>, ApiError> {
    let usage = state
        .insights
        .project_usage(&user.id, &project_id, query.days)
        .await
        .map_err(insights_error)?;
    Ok(Json(ApiResponse::ok(usage)))
}

async fn usage_summary(
    State(state): State<InsightsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<ApiResponse<UsageSummaryResponse>>, ApiError> {
    let usage = state
        .insights
        .usage_summary(&user.id, query.days)
        .await
        .map_err(insights_error)?;
    Ok(Json(ApiResponse::ok(usage)))
}

async fn storage(
    State(state): State<InsightsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<ProjectStorageResponse>>, ApiError> {
    let scan = state
        .insights
        .storage(&user.id, &project_id, false)
        .await
        .map_err(insights_error)?;
    Ok(Json(ApiResponse::ok(scan)))
}

async fn rescan(
    State(state): State<InsightsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<ProjectStorageResponse>>, ApiError> {
    let scan = state
        .insights
        .storage(&user.id, &project_id, true)
        .await
        .map_err(insights_error)?;
    Ok(Json(ApiResponse::ok(scan)))
}

async fn overview(
    State(state): State<InsightsRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<ProjectOverviewResponse>>, ApiError> {
    let overview = state
        .insights
        .overview(&user.id, &project_id)
        .await
        .map_err(insights_error)?;
    Ok(Json(ApiResponse::ok(overview)))
}

#[cfg(test)]
#[path = "insights_routes_test.rs"]
mod insights_routes_test;
