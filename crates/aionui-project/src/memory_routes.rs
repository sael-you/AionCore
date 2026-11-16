// `ApiError` is the intended error type at this HTTP boundary (routes map the
// crate-owned `MemoryError` / `ProjectError` to it here).
#![allow(clippy::disallowed_types)]

//! Project memory review routes (card C29, TASK-109) under
//! `/api/projects/{project_id}/memory`. Reads ride the ordinary auth layer;
//! writes are mounted by the app behind a session-only auth state (the agent
//! runtime-token channel is refused) and the shared trusted-Origin guard.
//! Handlers transform only: the logic is in [`MemoryService`] and
//! [`ProjectService::workspace_path`].

use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, MemoryDeleteResponse, MemoryFileResponse, MemoryListResponse, UpdateMemoryFileRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde_json::json;

use crate::memory::{MemoryError, MemoryService};
use crate::service::ProjectService;

#[derive(Clone)]
pub struct MemoryRouterState {
    pub project: Arc<ProjectService>,
    pub memory: Arc<MemoryService>,
}

/// `GET …/memory` and `GET …/memory/{file}`.
pub fn memory_read_routes(state: MemoryRouterState) -> Router {
    Router::new()
        .route("/api/projects/{project_id}/memory", get(list))
        .route("/api/projects/{project_id}/memory/{file}", get(read))
        .with_state(state)
}

/// `PUT` / `DELETE …/memory/{file}` and `POST …/memory/{file}/restore`.
pub fn memory_write_routes(state: MemoryRouterState) -> Router {
    Router::new()
        .route(
            "/api/projects/{project_id}/memory/{file}",
            axum::routing::put(update).delete(remove),
        )
        .route("/api/projects/{project_id}/memory/{file}/restore", post(restore))
        .with_state(state)
}

fn memory_error(err: MemoryError) -> ApiError {
    let (status, code) = match &err {
        MemoryError::InvalidName => (StatusCode::BAD_REQUEST, "MEMORY_INVALID_NAME"),
        MemoryError::NoMemory => (StatusCode::NOT_FOUND, "MEMORY_NOT_FOUND"),
        MemoryError::NotFound => (StatusCode::NOT_FOUND, "MEMORY_FILE_NOT_FOUND"),
        MemoryError::Changed { .. } => (StatusCode::CONFLICT, "MEMORY_CHANGED"),
        MemoryError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "MEMORY_TOO_LARGE"),
        MemoryError::IndexNotDeletable => (StatusCode::BAD_REQUEST, "MEMORY_INDEX_NOT_DELETABLE"),
        MemoryError::NothingToRestore => (StatusCode::NOT_FOUND, "MEMORY_NOTHING_TO_RESTORE"),
        MemoryError::Io(kind) => {
            tracing::warn!(error = %kind, "memory io error");
            return ApiError::Internal("memory files could not be accessed".to_owned());
        }
    };
    let details = match &err {
        MemoryError::Changed { current_modified } => Some(json!({ "current_modified": current_modified })),
        _ => None,
    };
    ApiError::coded(status, code, err.to_string(), details)
}

/// The workspace whose memory is addressed; a project without a local
/// workspace has no memory (404 on file routes, `dir_exists: false` on list).
async fn workspace(
    state: &MemoryRouterState,
    user: &CurrentUser,
    project_id: &str,
) -> Result<Option<PathBuf>, ApiError> {
    Ok(state.project.workspace_path(&user.id, project_id).await?)
}

async fn list(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<MemoryListResponse>>, ApiError> {
    let list = match workspace(&state, &user, &project_id).await? {
        Some(ws) => state.memory.list(&ws).await.map_err(memory_error)?,
        None => MemoryListResponse {
            dir_exists: false,
            index_lines: 0,
            items: Vec::new(),
            lint: Vec::new(),
        },
    };
    Ok(Json(ApiResponse::ok(list)))
}

async fn read(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((project_id, file)): Path<(String, String)>,
) -> Result<Json<ApiResponse<MemoryFileResponse>>, ApiError> {
    let ws = workspace(&state, &user, &project_id)
        .await?
        .ok_or_else(|| memory_error(MemoryError::NoMemory))?;
    let file = state.memory.read(&ws, &file).await.map_err(memory_error)?;
    Ok(Json(ApiResponse::ok(file)))
}

async fn update(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((project_id, file)): Path<(String, String)>,
    body: Result<Json<UpdateMemoryFileRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<MemoryFileResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let ws = workspace(&state, &user, &project_id)
        .await?
        .ok_or_else(|| memory_error(MemoryError::NoMemory))?;
    let file = state
        .memory
        .update(&ws, &file, &req.frontmatter_raw, &req.body, req.expected_modified)
        .await
        .map_err(memory_error)?;
    Ok(Json(ApiResponse::ok(file)))
}

async fn remove(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((project_id, file)): Path<(String, String)>,
) -> Result<Json<ApiResponse<MemoryDeleteResponse>>, ApiError> {
    let ws = workspace(&state, &user, &project_id)
        .await?
        .ok_or_else(|| memory_error(MemoryError::NoMemory))?;
    let deleted = state.memory.delete(&ws, &file).await.map_err(memory_error)?;
    Ok(Json(ApiResponse::ok(deleted)))
}

async fn restore(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((project_id, file)): Path<(String, String)>,
) -> Result<Json<ApiResponse<MemoryFileResponse>>, ApiError> {
    let ws = workspace(&state, &user, &project_id)
        .await?
        .ok_or_else(|| memory_error(MemoryError::NoMemory))?;
    let file = state.memory.restore(&ws, &file).await.map_err(memory_error)?;
    Ok(Json(ApiResponse::ok(file)))
}
