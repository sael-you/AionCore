// `ApiError` is the intended error type at this HTTP boundary (handlers map
// the crate-owned `BoardError` to it here).
#![allow(clippy::disallowed_types)]

//! Board user router (SPEC §5): `/api/projects/{project_id}/board…`,
//! `/api/board/…`, `/api/conversations/{id}/board-events`. Mounted under the
//! normal auth middleware by `aionui-app`.
//!
//! Every handler resolves its actor from [`AuthChannel`] first: a
//! runtime-token request (an agent process riding the auth middleware's
//! helper channel) gets `403 board_use_agent_router`; a request without an
//! `AuthChannel` fails closed with 500. The `x-aionui-board-actor:
//! supervisor` header makes the request the `system` actor in Local mode
//! and is `403 board_system_forbidden` otherwise.

use aionui_api_types::{
    ApiResponse, BOARD_ACTOR_HEADER, BOARD_ACTOR_SUPERVISOR, BoardAssignRequest, BoardAssignResponse,
    BoardBrainstormRequest, BoardCardDetail, BoardCardDto, BoardCardListQuery, BoardCardPage, BoardColumnsResponse,
    BoardCommentRequest, BoardDependencyDto, BoardEmptyRequest, BoardEventDto, BoardEventPage, BoardEventsQuery,
    BoardGateRequest, BoardKeeperResponse, BoardLinkDto, BoardLinkRequest, BoardPacketDto, BoardReviewRequest,
    BoardReviewResult, BoardSettingsDto, BoardView, BoardViewQuery, CreateBoardCardRequest, MoveBoardCardRequest,
    UpdateBoardCardRequest, UpdateBoardColumnsRequest, UpdateBoardSettingsRequest,
};
use aionui_auth::{AuthChannel, CurrentUser};
use aionui_common::ApiError;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde_json::json;
use tracing::{error, warn};

use crate::actor::BoardActor;
use crate::error::{BoardError, BoardErrorCode};
use crate::service_cards::MoveCard;
use crate::state::BoardRouterState;

type ApiResult<T> = Result<Json<ApiResponse<T>>, ApiError>;

/// Build the board user router. Authentication is applied by the caller.
pub fn board_routes(state: BoardRouterState) -> Router {
    Router::new()
        .route("/api/projects/{project_id}/board", get(get_board))
        .route("/api/projects/{project_id}/board/settings", put(put_settings))
        .route("/api/projects/{project_id}/board/columns", put(put_columns))
        .route("/api/projects/{project_id}/board/reactivate", post(reactivate))
        .route("/api/projects/{project_id}/board/keeper", post(keeper))
        .route(
            "/api/projects/{project_id}/board/cards",
            get(list_cards).post(create_card),
        )
        .route("/api/projects/{project_id}/board/events", get(project_events))
        .route("/api/board/cards/{card_id}", get(get_card).patch(patch_card))
        .route("/api/board/cards/{card_id}/move", post(move_card))
        .route("/api/board/cards/{card_id}/gate", post(gate))
        .route("/api/board/cards/{card_id}/reviews/{department}", put(put_review))
        .route("/api/board/cards/{card_id}/assign", post(assign))
        .route("/api/board/cards/{card_id}/brainstorm", post(brainstorm))
        .route("/api/board/cards/{card_id}/links", post(add_link))
        .route(
            "/api/board/cards/{card_id}/links/{conversation_id}",
            delete(remove_link),
        )
        .route("/api/board/cards/{card_id}/archive", post(archive_card))
        .route("/api/board/cards/{card_id}/restore", post(restore_card))
        .route(
            "/api/board/cards/{card_id}/comments",
            get(list_comments).post(post_comment),
        )
        .route("/api/board/cards/{card_id}/events", get(card_events))
        .route("/api/board/cards/{card_id}/packet", get(packet))
        .route(
            "/api/board/cards/{card_id}/deps/{other_id}",
            put(put_dependency).delete(delete_dependency),
        )
        .route("/api/conversations/{id}/board-events", get(conversation_events))
        .with_state(state)
}

/// Map a board error to the coded HTTP error; warn on board 4xx (R10).
pub(crate) fn api_error(error: BoardError) -> ApiError {
    match &error {
        BoardError::Rule { code, .. } => {
            let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            warn!(code = code.as_str(), status = status.as_u16(), "board request rejected");
            ApiError::coded(status, code.as_str(), error.public_message(), error.details().cloned())
        }
        BoardError::Internal(cause) => {
            error!(error = %cause, "board request failed");
            ApiError::Internal("board operation failed".into())
        }
    }
}

fn invalid_body(reason: String) -> ApiError {
    api_error(
        BoardError::new(BoardErrorCode::Invalid, "the request needs a valid JSON body")
            .with_details(json!({ "field": "body", "reason": reason })),
    )
}

/// Mutations require a JSON body (`{}` allowed) — R9.
fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value)
        .map_err(|rejection| invalid_body(rejection.body_text()))
}

/// DELETE bodies are optional (browsers preflight DELETE, so a JSON body adds
/// no CSRF protection there); a body that is present must be `{}`.
fn delete_body(bytes: &axum::body::Bytes) -> Result<(), ApiError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    serde_json::from_slice::<BoardEmptyRequest>(bytes)
        .map(|_| ())
        .map_err(|error| invalid_body(error.to_string()))
}

fn query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query.map(|Query(value)| value).map_err(|rejection| {
        api_error(
            BoardError::new(BoardErrorCode::Invalid, "invalid query parameters")
                .with_details(json!({ "field": "query", "reason": rejection.body_text() })),
        )
    })
}

/// Resolve the board actor of a user-router request (SPEC §2).
pub(crate) fn resolve_actor(
    user: &CurrentUser,
    channel: Option<&AuthChannel>,
    headers: &HeaderMap,
) -> Result<BoardActor, ApiError> {
    let Some(channel) = channel else {
        error!("board user router reached without an AuthChannel; refusing");
        return Err(ApiError::Internal("authentication channel missing".into()));
    };
    if matches!(channel, AuthChannel::RuntimeToken { .. }) {
        return Err(api_error(BoardError::new(
            BoardErrorCode::UseAgentRouter,
            "agents use /api/runtime/board (the aioncore board CLI)",
        )));
    }
    match headers.get(BOARD_ACTOR_HEADER) {
        None => Ok(BoardActor::user(user.id.clone())),
        Some(value) if *channel == AuthChannel::Local && value.as_bytes() == BOARD_ACTOR_SUPERVISOR.as_bytes() => {
            Ok(BoardActor::system(user.id.clone()))
        }
        Some(_) if *channel != AuthChannel::Local => Err(api_error(BoardError::new(
            BoardErrorCode::SystemForbidden,
            "the supervisor actor is only available in local mode",
        ))),
        Some(_) => Err(api_error(BoardError::invalid(
            BOARD_ACTOR_HEADER,
            format!("{BOARD_ACTOR_HEADER} accepts only `{BOARD_ACTOR_SUPERVISOR}`"),
        ))),
    }
}

macro_rules! actor {
    ($user:expr, $channel:expr, $headers:expr) => {
        resolve_actor(&$user, $channel.as_ref().map(|Extension(channel)| channel), &$headers)?
    };
}

type Channel = Option<Extension<AuthChannel>>;

async fn get_board(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    params: Result<Query<BoardViewQuery>, QueryRejection>,
) -> ApiResult<BoardView> {
    let actor = actor!(user, channel, headers);
    let params = query(params)?;
    let view = state
        .service
        .board_view(&actor, &project_id, params.include_archived)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(view)))
}

async fn put_settings(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    request: Result<Json<UpdateBoardSettingsRequest>, JsonRejection>,
) -> ApiResult<BoardSettingsDto> {
    let actor = actor!(user, channel, headers);
    let settings = state
        .service
        .update_settings(&actor, &project_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(settings)))
}

async fn put_columns(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    request: Result<Json<UpdateBoardColumnsRequest>, JsonRejection>,
) -> ApiResult<BoardColumnsResponse> {
    let actor = actor!(user, channel, headers);
    let columns = state
        .service
        .update_columns(&actor, &project_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(columns)))
}

async fn reactivate(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    request: Result<Json<BoardEmptyRequest>, JsonRejection>,
) -> ApiResult<BoardSettingsDto> {
    let actor = actor!(user, channel, headers);
    body(request)?;
    let settings = state.service.reactivate(&actor, &project_id).await.map_err(api_error)?;
    Ok(Json(ApiResponse::ok(settings)))
}

async fn keeper(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    request: Result<Json<BoardEmptyRequest>, JsonRejection>,
) -> ApiResult<BoardKeeperResponse> {
    let actor = actor!(user, channel, headers);
    body(request)?;
    let keeper = state
        .service
        .ensure_keeper(&actor, &project_id)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(keeper)))
}

async fn gate(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<BoardGateRequest>, JsonRejection>,
) -> ApiResult<BoardCardDto> {
    let actor = actor!(user, channel, headers);
    let card = state
        .service
        .decide_gate(&actor, &card_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(card)))
}

async fn put_review(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path((card_id, department)): Path<(String, String)>,
    request: Result<Json<BoardReviewRequest>, JsonRejection>,
) -> ApiResult<BoardReviewResult> {
    let actor = actor!(user, channel, headers);
    let review = state
        .service
        .submit_review(&actor, &card_id, &department, body(request)?)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(review)))
}

async fn assign(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<BoardAssignRequest>, JsonRejection>,
) -> ApiResult<BoardAssignResponse> {
    let actor = actor!(user, channel, headers);
    let response = state
        .service
        .assign(&actor, &card_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn brainstorm(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<BoardBrainstormRequest>, JsonRejection>,
) -> ApiResult<BoardAssignResponse> {
    let actor = actor!(user, channel, headers);
    let response = state
        .service
        .brainstorm(&actor, &card_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn add_link(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<BoardLinkRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<BoardLinkDto>>), ApiError> {
    let actor = actor!(user, channel, headers);
    let link = state
        .service
        .add_link(&actor, &card_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(link))))
}

async fn remove_link(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path((card_id, conversation_id)): Path<(String, String)>,
    request: axum::body::Bytes,
) -> ApiResult<serde_json::Value> {
    let actor = actor!(user, channel, headers);
    delete_body(&request)?;
    state
        .service
        .remove_link(&actor, &card_id, &conversation_id)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(json!({ "removed": true }))))
}

async fn list_cards(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    params: Result<Query<BoardCardListQuery>, QueryRejection>,
) -> ApiResult<BoardCardPage> {
    let actor = actor!(user, channel, headers);
    let page = state
        .service
        .list_project_cards(&actor, &project_id, &query(params)?)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(page)))
}

async fn create_card(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    request: Result<Json<CreateBoardCardRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<BoardCardDto>>), ApiError> {
    let actor = actor!(user, channel, headers);
    let card = state
        .service
        .create_card(&actor, &project_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(card))))
}

async fn project_events(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    params: Result<Query<BoardEventsQuery>, QueryRejection>,
) -> ApiResult<BoardEventPage> {
    let actor = actor!(user, channel, headers);
    let params = query(params)?;
    let page = state
        .service
        .project_events(&actor, &project_id, params.after_seq, params.limit)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(page)))
}

async fn get_card(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
) -> ApiResult<BoardCardDetail> {
    let actor = actor!(user, channel, headers);
    let detail = state.service.card_detail(&actor, &card_id).await.map_err(api_error)?;
    Ok(Json(ApiResponse::ok(detail)))
}

async fn patch_card(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<UpdateBoardCardRequest>, JsonRejection>,
) -> ApiResult<BoardCardDto> {
    let actor = actor!(user, channel, headers);
    let card = state
        .service
        .update_card(&actor, &card_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(card)))
}

async fn move_card(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<MoveBoardCardRequest>, JsonRejection>,
) -> ApiResult<BoardCardDto> {
    let actor = actor!(user, channel, headers);
    let request = body(request)?;
    let card = state
        .service
        .move_card(
            &actor,
            &card_id,
            MoveCard {
                to_column: request.to_column,
                before_id: request.before_id,
                after_id: request.after_id,
                expected_column: Some(request.expected_column),
                force: request.force,
                reason: request.reason,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(card)))
}

async fn archive_card(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<BoardEmptyRequest>, JsonRejection>,
) -> ApiResult<BoardCardDto> {
    let actor = actor!(user, channel, headers);
    body(request)?;
    let card = state
        .service
        .set_card_archived(&actor, &card_id, true)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(card)))
}

async fn restore_card(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<BoardEmptyRequest>, JsonRejection>,
) -> ApiResult<BoardCardDto> {
    let actor = actor!(user, channel, headers);
    body(request)?;
    let card = state
        .service
        .set_card_archived(&actor, &card_id, false)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(card)))
}

async fn list_comments(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    params: Result<Query<BoardEventsQuery>, QueryRejection>,
) -> ApiResult<BoardEventPage> {
    let actor = actor!(user, channel, headers);
    let params = query(params)?;
    let page = state
        .service
        .card_comments(&actor, &card_id, params.after_seq, params.limit)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(page)))
}

async fn post_comment(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    request: Result<Json<BoardCommentRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<BoardEventDto>>), ApiError> {
    let actor = actor!(user, channel, headers);
    let event = state
        .service
        .comment(&actor, &card_id, body(request)?)
        .await
        .map_err(api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(event))))
}

async fn card_events(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
    params: Result<Query<BoardEventsQuery>, QueryRejection>,
) -> ApiResult<BoardEventPage> {
    let actor = actor!(user, channel, headers);
    let params = query(params)?;
    let page = state
        .service
        .card_events(&actor, &card_id, params.after_seq, params.limit)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(page)))
}

async fn packet(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(card_id): Path<String>,
) -> ApiResult<BoardPacketDto> {
    let actor = actor!(user, channel, headers);
    let packet = state.service.packet(&actor, &card_id).await.map_err(api_error)?;
    Ok(Json(ApiResponse::ok(packet)))
}

async fn put_dependency(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path((card_id, other_id)): Path<(String, String)>,
    request: Result<Json<BoardEmptyRequest>, JsonRejection>,
) -> ApiResult<Vec<BoardDependencyDto>> {
    let actor = actor!(user, channel, headers);
    body(request)?;
    let deps = state
        .service
        .set_dependency(&actor, &card_id, &other_id, true)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(deps)))
}

async fn delete_dependency(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path((card_id, other_id)): Path<(String, String)>,
    request: axum::body::Bytes,
) -> ApiResult<Vec<BoardDependencyDto>> {
    let actor = actor!(user, channel, headers);
    delete_body(&request)?;
    let deps = state
        .service
        .set_dependency(&actor, &card_id, &other_id, false)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(deps)))
}

async fn conversation_events(
    State(state): State<BoardRouterState>,
    Extension(user): Extension<CurrentUser>,
    channel: Channel,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
    params: Result<Query<BoardEventsQuery>, QueryRejection>,
) -> ApiResult<BoardEventPage> {
    let actor = actor!(user, channel, headers);
    let params = query(params)?;
    let page = state
        .service
        .conversation_events(actor.user_id(), &conversation_id, params.after_seq, params.limit)
        .await
        .map_err(api_error)?;
    Ok(Json(ApiResponse::ok(page)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{UserStatus, UserType};
    use axum::http::HeaderValue;

    fn user() -> CurrentUser {
        CurrentUser {
            id: "u1".into(),
            username: "u1".into(),
            user_type: UserType::Local,
            status: UserStatus::Active,
        }
    }

    fn supervisor_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(BOARD_ACTOR_HEADER, HeaderValue::from_static("supervisor"));
        headers
    }

    #[test]
    fn a_missing_auth_channel_fails_closed() {
        let error = resolve_actor(&user(), None, &HeaderMap::new()).unwrap_err();
        assert_eq!(error.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn runtime_token_callers_are_sent_to_the_agent_router() {
        let channel = AuthChannel::RuntimeToken {
            conversation_id: "c1".into(),
        };
        let error = resolve_actor(&user(), Some(&channel), &HeaderMap::new()).unwrap_err();
        assert_eq!(error.status_code(), StatusCode::FORBIDDEN);
        assert_eq!(error.error_code(), "board_use_agent_router");
    }

    #[test]
    fn the_supervisor_header_is_local_mode_only() {
        let local = resolve_actor(&user(), Some(&AuthChannel::Local), &supervisor_headers()).unwrap();
        assert_eq!(local, BoardActor::system("u1"));
        let session = resolve_actor(&user(), Some(&AuthChannel::Session), &supervisor_headers()).unwrap_err();
        assert_eq!(session.error_code(), "board_system_forbidden");
        let mut bogus = HeaderMap::new();
        bogus.insert(BOARD_ACTOR_HEADER, HeaderValue::from_static("root"));
        let error = resolve_actor(&user(), Some(&AuthChannel::Local), &bogus).unwrap_err();
        assert_eq!(error.error_code(), "board_invalid");
    }

    #[test]
    fn plain_requests_are_the_owner() {
        let actor = resolve_actor(&user(), Some(&AuthChannel::Session), &HeaderMap::new()).unwrap();
        assert_eq!(actor, BoardActor::user("u1"));
    }
}
