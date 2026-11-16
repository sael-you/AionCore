//! Board agent router `/api/runtime/board/…` (SPEC §2, §5) — the helper
//! CLI's endpoint.
//!
//! Mounted OUTSIDE the auth and CSRF middleware (like
//! `aionui-conversation/src/runtime_routes.rs`): it validates the
//! `x-aionui-{user-id,conversation-id,runtime-token}` headers itself
//! (`RuntimeTokenService::validate`, scope `ConversationHelper`) in every
//! identity mode, so the token-bound conversation is the caller and the body
//! cannot name another. There are no gate, force, settings, columns,
//! assign, archive or delete endpoints here — those rules hold by
//! construction. Cards are addressed by number within the caller's project.
//! The project's Board Keeper conversation is an agent with one extra
//! privilege: it may move cards between non-gate columns (D40).

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{
    AgentBlockBoardCardRequest, AgentLinkBoardCardRequest, AgentMoveBoardCardRequest, AgentShowBoardCardQuery,
    AgentUpdateBoardCardRequest, BoardCardListQuery, BoardCliEnvelope, BoardCliError, BoardCommentRequest,
    BoardEmptyRequest, BoardEventsQuery, BoardReviewRequest, CreateBoardCardRequest,
};
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::actor::{AgentCaller, BoardActor};
use crate::error::{BoardError, BoardErrorCode};
use crate::service::BoardService;
use crate::service_cards::MoveCard;
use crate::state::BoardRuntimeRouterState;
use crate::validation::card_number;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";

type Reply = (StatusCode, Json<BoardCliEnvelope<Value>>);
/// A failed step: the reply to send, boxed (it is large).
type Fail = Box<Reply>;

pub fn board_runtime_routes(state: BoardRuntimeRouterState) -> Router {
    Router::new()
        .route("/api/runtime/board/status", get(status))
        .route("/api/runtime/board/cards", get(list).post(create))
        .route("/api/runtime/board/cards/{number}", get(show).patch(update))
        .route("/api/runtime/board/cards/{number}/move", post(move_card))
        .route("/api/runtime/board/cards/{number}/reviews/{department}", put(review))
        .route("/api/runtime/board/cards/{number}/comments", post(comment))
        .route("/api/runtime/board/cards/{number}/block", post(block))
        .route("/api/runtime/board/cards/{number}/unblock", post(unblock))
        .route("/api/runtime/board/cards/{number}/links", post(link))
        .route("/api/runtime/board/cards/{number}/packet", get(packet))
        .route("/api/runtime/board/events", get(events))
        .with_state(state)
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// `(user_id, conversation_id)` of a valid conversation-helper token.
fn authenticate(state: &BoardRuntimeRouterState, headers: &HeaderMap) -> Option<(String, String)> {
    let user_id = required_header(headers, HEADER_USER_ID)?;
    let conversation_id = required_header(headers, HEADER_CONVERSATION_ID)?;
    let token = required_header(headers, HEADER_RUNTIME_TOKEN)?;
    state
        .runtime_token_service
        .validate(
            Some(&token),
            &user_id,
            &conversation_id,
            RuntimeTokenScope::ConversationHelper,
            // Every conversation's helper token is issued with this constant.
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .ok()?;
    Some((user_id, conversation_id))
}

fn failure(status: StatusCode, code: &str, message: String, details: Option<Value>, command: &str) -> Reply {
    (
        status,
        Json(BoardCliEnvelope::failure(
            BoardCliError {
                code: code.to_owned(),
                message,
                details,
            },
            command,
        )),
    )
}

fn board_failure(error: BoardError, command: &str) -> Reply {
    match &error {
        BoardError::Rule { code, .. } => {
            warn!(code = code.as_str(), command, "board agent request rejected");
            let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            failure(
                status,
                code.as_str(),
                error.public_message(),
                error.details().cloned(),
                command,
            )
        }
        BoardError::Internal(cause) => {
            error!(error = %cause, command, "board agent request failed");
            failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transport_unavailable",
                "board operation failed".into(),
                None,
                command,
            )
        }
    }
}

fn success<T: Serialize>(status: StatusCode, data: T, command: &str) -> Reply {
    match serde_json::to_value(data) {
        Ok(value) => (status, Json(BoardCliEnvelope::success(value, command))),
        Err(error) => board_failure(BoardError::from(error), command),
    }
}

fn unauthenticated(command: &str) -> Reply {
    // No header values are logged — the token must never reach the logs.
    warn!(
        error_code = "runtime_auth_failed",
        command, "board agent request refused"
    );
    failure(
        StatusCode::UNAUTHORIZED,
        "runtime_auth_failed",
        "runtime auth failed".into(),
        None,
        command,
    )
}

fn parse_body<T>(body: Result<Json<T>, JsonRejection>, command: &str) -> Result<T, Fail> {
    body.map(|Json(value)| value).map_err(|rejection| {
        Box::new(board_failure(
            BoardError::new(BoardErrorCode::Invalid, "the request needs a valid JSON body")
                .with_details(json!({ "field": "body", "reason": rejection.body_text() })),
            command,
        ))
    })
}

fn parse_query<T>(query: Result<Query<T>, QueryRejection>, command: &str) -> Result<T, Fail> {
    query.map(|Query(value)| value).map_err(|rejection| {
        Box::new(board_failure(
            BoardError::new(BoardErrorCode::Invalid, "invalid query parameters")
                .with_details(json!({ "field": "query", "reason": rejection.body_text() })),
            command,
        ))
    })
}

/// Authenticate, resolve the caller's project (R7) and require an enabled
/// board (R1).
async fn agent(state: &BoardRuntimeRouterState, headers: &HeaderMap, command: &str) -> Result<BoardActor, Fail> {
    let (user_id, conversation_id) = authenticate(state, headers).ok_or_else(|| Box::new(unauthenticated(command)))?;
    let caller = state
        .service
        .resolve_agent(&user_id, &conversation_id)
        .await
        .map_err(|error| Box::new(board_failure(error, command)))?;
    let actor = BoardActor::Agent(caller);
    state
        .service
        .require_enabled(&actor)
        .await
        .map_err(|error| Box::new(board_failure(error, command)))?;
    Ok(actor)
}

fn caller(actor: &BoardActor) -> &AgentCaller {
    actor.as_agent().expect("agent router actors are agents")
}

/// The card id behind `{number}` in the caller's project.
async fn card_id(service: &BoardService, actor: &BoardActor, raw: &str, command: &str) -> Result<String, Fail> {
    let number = card_number(raw).map_err(|error| Box::new(board_failure(error, command)))?;
    service
        .agent_card(caller(actor), number)
        .await
        .map(|card| card.id)
        .map_err(|error| Box::new(board_failure(error, command)))
}

macro_rules! try_reply {
    ($expr:expr) => {
        match $expr {
            Ok(value) => value,
            Err(reply) => return *reply,
        }
    };
}

async fn status(State(state): State<BoardRuntimeRouterState>, headers: HeaderMap) -> Reply {
    const COMMAND: &str = "board status";
    let Some((user_id, conversation_id)) = authenticate(&state, &headers) else {
        return unauthenticated(COMMAND);
    };
    // Not logged as a warning: status always answers (SPEC §7).
    match state.service.agent_status(&user_id, &conversation_id).await {
        Ok(status) => success(StatusCode::OK, status, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn list(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    params: Result<Query<BoardCardListQuery>, QueryRejection>,
) -> Reply {
    const COMMAND: &str = "board list";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let params = try_reply!(parse_query(params, COMMAND));
    let caller = caller(&actor);
    match state
        .service
        .list_cards(
            &caller.user_id,
            &caller.project_id,
            &params,
            caller.assistant_id.as_deref(),
        )
        .await
    {
        Ok(page) => success(StatusCode::OK, page, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn create(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    body: Result<Json<CreateBoardCardRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "board create";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let request = try_reply!(parse_body(body, COMMAND));
    let project_id = caller(&actor).project_id.clone();
    match state.service.create_card(&actor, &project_id, request).await {
        Ok(card) => success(StatusCode::CREATED, card, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn show(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    Path(number): Path<String>,
    params: Result<Query<AgentShowBoardCardQuery>, QueryRejection>,
) -> Reply {
    const COMMAND: &str = "board show";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let params = try_reply!(parse_query(params, COMMAND));
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    if params.packet {
        return match state.service.packet(&actor, &id).await {
            Ok(packet) => success(StatusCode::OK, packet, COMMAND),
            Err(error) => board_failure(error, COMMAND),
        };
    }
    match state.service.card_detail(&actor, &id).await {
        Ok(detail) => success(StatusCode::OK, detail, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn update(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    Path(number): Path<String>,
    body: Result<Json<AgentUpdateBoardCardRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "board update";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let request = try_reply!(parse_body(body, COMMAND));
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    match state.service.update_card(&actor, &id, request.into()).await {
        Ok(card) => success(StatusCode::OK, card, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn move_card(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    Path(number): Path<String>,
    body: Result<Json<AgentMoveBoardCardRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "board move";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let request = try_reply!(parse_body(body, COMMAND));
    if request.before.is_some() && request.after.is_some() {
        return board_failure(BoardError::invalid("before", "pass before or after, not both"), COMMAND);
    }
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    let mut anchors = [None, None];
    for (slot, anchor) in anchors.iter_mut().zip([request.before, request.after]) {
        if let Some(anchor) = anchor {
            *slot = Some(try_reply!(
                card_id(&state.service, &actor, &anchor.to_string(), COMMAND).await
            ));
        }
    }
    let [before_id, after_id] = anchors;
    let request = MoveCard {
        to_column: request.to_column,
        before_id,
        after_id,
        expected_column: request.expected_column,
        force: false,
        reason: None,
        approval_note: request.approval_note,
    };
    match state.service.move_card(&actor, &id, request).await {
        Ok(card) => success(StatusCode::OK, card, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn review(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    Path((number, department)): Path<(String, String)>,
    body: Result<Json<BoardReviewRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "board review";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let request = try_reply!(parse_body(body, COMMAND));
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    match state.service.submit_review(&actor, &id, &department, request).await {
        Ok(review) => success(StatusCode::OK, review, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn comment(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    Path(number): Path<String>,
    body: Result<Json<BoardCommentRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "board comment";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let request = try_reply!(parse_body(body, COMMAND));
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    match state.service.comment(&actor, &id, request).await {
        Ok(event) => success(StatusCode::CREATED, event, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn block(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    Path(number): Path<String>,
    body: Result<Json<AgentBlockBoardCardRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "board block";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let request = try_reply!(parse_body(body, COMMAND));
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    match state.service.set_blocked(&actor, &id, Some(&request.reason)).await {
        Ok(card) => success(StatusCode::OK, card, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn unblock(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    Path(number): Path<String>,
    body: Result<Json<BoardEmptyRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "board unblock";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    try_reply!(parse_body(body, COMMAND));
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    match state.service.set_blocked(&actor, &id, None).await {
        Ok(card) => success(StatusCode::OK, card, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn link(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    Path(number): Path<String>,
    body: Result<Json<AgentLinkBoardCardRequest>, JsonRejection>,
) -> Reply {
    const COMMAND: &str = "board link";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let request = try_reply!(parse_body(body, COMMAND));
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    match state.service.link_caller(&actor, &id, request.role).await {
        Ok(link) => success(StatusCode::CREATED, link, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn packet(State(state): State<BoardRuntimeRouterState>, headers: HeaderMap, Path(number): Path<String>) -> Reply {
    const COMMAND: &str = "board show";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let id = try_reply!(card_id(&state.service, &actor, &number, COMMAND).await);
    match state.service.packet(&actor, &id).await {
        Ok(packet) => success(StatusCode::OK, packet, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}

async fn events(
    State(state): State<BoardRuntimeRouterState>,
    headers: HeaderMap,
    params: Result<Query<BoardEventsQuery>, QueryRejection>,
) -> Reply {
    const COMMAND: &str = "board events";
    let actor = try_reply!(agent(&state, &headers, COMMAND).await);
    let params = try_reply!(parse_query(params, COMMAND));
    let project_id = caller(&actor).project_id.clone();
    match state
        .service
        .project_events(&actor, &project_id, params.after_seq, params.limit)
        .await
    {
        Ok(page) => success(StatusCode::OK, page, COMMAND),
        Err(error) => board_failure(error, COMMAND),
    }
}
