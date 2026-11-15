//! Assign and brainstorm (SPEC §6): open (or reuse) a department
//! conversation for a card, link it, set the assignee, deliver the packet.
//!
//! Order and failures:
//! 0. Reserve the `idempotency_key` (`pending`, a `board_assignment_id`, a
//!    fresh `claim_token`). Same key + same request hash on a `done` key →
//!    the stored response; on a `pending` key → take over only when the
//!    lease is older than 30 s (else `409 board_assignment_in_progress`);
//!    different hash → `409 board_idempotency_conflict`. Every takeover
//!    issues a new token; every later phase checks it in its own transaction
//!    and a claimant that lost it stops before any further side effect.
//! 1. Validate (reads).
//! 2. Find-or-create the conversation by `extra.board_assignment_id` (the 901
//!    unique index makes a second one impossible).
//! 3. Board transaction: link, assignee, one `card_assigned` event. If it
//!    fails, a conversation created by this attempt is deleted. A parent
//!    fallback is then written as its own `link_parent_fallback` event.
//! 4. Send the packet once (insert-or-return-existing on the conversation's
//!    first user message; `packet_message_id` stored). A send failure writes
//!    `packet_send_failed` (with the key and request for Resend), releases the
//!    lease and answers `packet_sent: false`; a retry that delivers it writes
//!    `packet_sent`.

use std::collections::HashSet;

use aionui_api_types::{
    BoardAssignRequest, BoardAssignResponse, BoardBrainstormRequest, BoardLinkDto, LinkRole, ParentFallback,
};
use aionui_common::{generate_id, now_ms};
use aionui_db::{BoardCardRow, BoardIdempotencyRow, BoardLinkRow, BoardLinkViewRow};
use serde_json::json;
use tracing::{Instrument, info, warn};

use crate::actor::BoardActor;
use crate::convert::{link_dto, reference};
use crate::error::{BoardError, BoardErrorCode, BoardResult};
use crate::events::{CARD_ASSIGNED, LINK_PARENT_FALLBACK, PACKET_SEND_FAILED, PACKET_SENT};
use crate::packet::build_packet;
use crate::ports::{AssignmentConversation, HostError};
use crate::service::{BoardService, Committed, require_owner_or_system, write_span};
use crate::service_gate::{IDEMPOTENCY_KEY_MAX, request_hash};
use crate::service_reviews::already_linked;
use crate::validation::{DEPARTMENT_MAX, required_text};

/// A pending reservation older than this may be taken over.
pub const ASSIGN_LEASE_MS: i64 = 30_000;
/// Hierarchy limits of the conversation tree (`aionui-conversation`).
const MAX_PARENT_DEPTH: i64 = 3;
const MAX_CHILDREN: i64 = 12;

/// What assigns to the card.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Assignee {
    Assistant(String),
    Team(String),
}

/// A validated assign request.
#[derive(Debug, Clone)]
struct AssignPlan {
    assignee: Assignee,
    role: LinkRole,
    department: Option<String>,
    key: String,
    hash: String,
}

/// The current holder of a reservation.
struct Claim {
    assignment_id: String,
    token: String,
    packet_message_id: Option<String>,
    /// Taken over from an earlier attempt (resume).
    resumed: bool,
}

enum Reservation {
    Claimed(Claim),
    Done(Box<BoardAssignResponse>),
}

fn assignment_in_progress() -> BoardError {
    BoardError::new(
        BoardErrorCode::AssignmentInProgress,
        "another request is completing this assignment; retry shortly",
    )
}

fn team_wrong_project() -> BoardError {
    BoardError::Rule {
        code: BoardErrorCode::WrongProject,
        status: Some(409),
        message: "the team belongs to another project".into(),
        details: None,
    }
}

impl BoardService {
    /// `POST /api/board/cards/{id}/assign` (owner / supervisor).
    pub async fn assign(
        &self,
        actor: &BoardActor,
        card_id: &str,
        request: BoardAssignRequest,
    ) -> BoardResult<BoardAssignResponse> {
        require_owner_or_system(actor)?;
        let assignee = match (request.assistant_id.as_deref(), request.team_id.as_deref()) {
            (Some(assistant), None) => Assignee::Assistant(required_text("assistant_id", assistant, 200)?),
            (None, Some(team)) => Assignee::Team(required_text("team_id", team, 200)?),
            _ => {
                return Err(BoardError::invalid(
                    "assistant_id",
                    "pass exactly one of assistant_id and team_id",
                ));
            }
        };
        let role = match (&assignee, request.role) {
            (Assignee::Team(_), None | Some(LinkRole::Lead)) => LinkRole::Lead,
            (Assignee::Team(_), Some(_)) => {
                return Err(BoardError::invalid(
                    "role",
                    "a team is linked through its lead conversation",
                ));
            }
            (Assignee::Assistant(_), role) => role.unwrap_or(LinkRole::Review),
        };
        let department = match request.department.as_deref() {
            Some(department) => Some(required_text("department", department, DEPARTMENT_MAX)?),
            None if role == LinkRole::Brainstorm => None,
            None => return Err(BoardError::invalid("department", "assign needs a department")),
        };
        let key = required_text("idempotency_key", &request.idempotency_key, IDEMPOTENCY_KEY_MAX)?;
        let hash = request_hash(&json!({
            "op": "assign",
            "card_id": card_id,
            "assistant_id": request.assistant_id,
            "team_id": request.team_id,
            "department": department,
            "role": role.as_str(),
        }));
        self.run_assign(
            actor,
            card_id,
            AssignPlan {
                assignee,
                role,
                department,
                key,
                hash,
            },
        )
        .await
    }

    /// `POST /api/board/cards/{id}/brainstorm` → assign with role
    /// `brainstorm` (default assistant: the Board Keeper).
    pub async fn brainstorm(
        &self,
        actor: &BoardActor,
        card_id: &str,
        request: BoardBrainstormRequest,
    ) -> BoardResult<BoardAssignResponse> {
        require_owner_or_system(actor)?;
        let assistant = match request.assistant_id.as_deref() {
            Some(id) => required_text("assistant_id", id, 200)?,
            None => self
                .host()?
                .ensure_keeper_assistant(actor.user_id())
                .await
                .map_err(BoardError::Internal)?,
        };
        let key = match request.idempotency_key.as_deref() {
            Some(key) => required_text("idempotency_key", key, IDEMPOTENCY_KEY_MAX)?,
            None => generate_id(),
        };
        let hash = request_hash(&json!({ "op": "brainstorm", "card_id": card_id, "assistant_id": assistant }));
        self.run_assign(
            actor,
            card_id,
            AssignPlan {
                assignee: Assignee::Assistant(assistant),
                role: LinkRole::Brainstorm,
                department: None,
                key,
                hash,
            },
        )
        .await
    }

    async fn run_assign(
        &self,
        actor: &BoardActor,
        card_id: &str,
        plan: AssignPlan,
    ) -> BoardResult<BoardAssignResponse> {
        let user_id = actor.user_id();
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        let settings = self
            .store()
            .settings(user_id, &card.project_id)
            .await?
            .ok_or_else(crate::service::not_turned_on)?;
        Self::check_writable(&settings, actor)?;
        let host = self.host()?.clone();

        // (0) reserve
        let claim = match self.reserve(user_id, &card.project_id, &plan).await? {
            Reservation::Done(response) => return Ok(*response),
            Reservation::Claimed(claim) => claim,
        };
        let outcome = self.assign_claimed(actor, &card, &plan, &claim, host.as_ref()).await;
        if outcome.is_err() {
            // Let the next retry take over at once.
            self.release(user_id, &card.project_id, &plan.key, &claim.token).await;
        }
        outcome
    }

    async fn assign_claimed(
        &self,
        actor: &BoardActor,
        card: &BoardCardRow,
        plan: &AssignPlan,
        claim: &Claim,
        host: &dyn crate::ports::BoardHostPort,
    ) -> BoardResult<BoardAssignResponse> {
        let user_id = actor.user_id();
        let store = self.store();
        // (1) validate
        let mut parent_fallback = None;
        let (conversation_id, created_now) = match &plan.assignee {
            Assignee::Team(team_id) => {
                let team = host
                    .team(user_id, team_id)
                    .await
                    .map_err(BoardError::Internal)?
                    .ok_or_else(|| BoardError::not_found("team"))?;
                if store.team_project_id(user_id, team_id).await?.as_deref() != Some(card.project_id.as_str()) {
                    return Err(team_wrong_project());
                }
                let lead = team
                    .lead_conversation_id
                    .ok_or_else(|| BoardError::invalid("team_id", "the team has no lead conversation"))?;
                (lead, false)
            }
            Assignee::Assistant(assistant_id) => {
                if host
                    .assistant_name(user_id, assistant_id)
                    .await
                    .map_err(BoardError::Internal)?
                    .is_none()
                {
                    return Err(BoardError::not_found("assistant"));
                }
                let workspace = host
                    .project_workspace(user_id, &card.project_id)
                    .await
                    .map_err(BoardError::Internal)?
                    .ok_or_else(|| BoardError::invalid("project", "the project has no workspace folder"))?;
                // (2) find-or-create
                match store.conversation_by_assignment(user_id, &claim.assignment_id).await? {
                    Some(existing) => (existing, false),
                    None => {
                        let (parent, fallback) = self.parent_for(user_id, &card.id).await?;
                        parent_fallback = fallback;
                        let (skills, _) = self.split_skills(host, user_id, card).await?;
                        self.ensure_claim(user_id, &card.project_id, &plan.key, &claim.token)
                            .await?;
                        let spec = AssignmentConversation {
                            assistant_id: assistant_id.clone(),
                            name: format!(
                                "{} · {}",
                                reference(card.number),
                                plan.department.as_deref().unwrap_or(plan.role.as_str())
                            ),
                            workspace,
                            parent_conversation_id: parent,
                            assignment_id: claim.assignment_id.clone(),
                            skills,
                        };
                        match host.create_assignment_conversation(user_id, &spec).await {
                            Ok(id) => (id, true),
                            Err(HostError::Duplicate) => (
                                store
                                    .conversation_by_assignment(user_id, &claim.assignment_id)
                                    .await?
                                    .ok_or_else(|| BoardError::Internal("assignment conversation vanished".into()))?,
                                false,
                            ),
                            Err(HostError::AssistantUnavailable(_)) => return Err(BoardError::not_found("assistant")),
                            Err(HostError::Failed(reason)) => return Err(BoardError::Internal(reason)),
                        }
                    }
                }
            }
        };

        // (3) board transaction
        let linked = match self
            .link_assignment(actor, card, plan, claim, &conversation_id, parent_fallback)
            .await
        {
            Ok(linked) => linked,
            Err(error) => {
                // Only the claim holder may undo its side effects: a claimant
                // that lost the lease leaves the conversation to the new one.
                if created_now
                    && self
                        .claim_held(user_id, &card.project_id, &plan.key, &claim.token)
                        .await
                {
                    self.discard_conversation(user_id, &conversation_id).await;
                }
                return Err(error);
            }
        };

        if let Some(reason) = parent_fallback {
            // Visible on the card, never silent (§6): its own write.
            self.card_event(
                actor,
                card,
                LINK_PARENT_FALLBACK,
                json!({ "conversation_id": conversation_id, "reason": reason }),
            )
            .await?;
        }

        // (4) packet
        let card_row = store
            .card(user_id, &card.id)
            .await?
            .ok_or_else(|| BoardError::not_found("card"))?;
        let mut response = BoardAssignResponse {
            card: self.card_dto_of(&card_row).await?,
            link: linked,
            conversation_id: conversation_id.clone(),
            assignment_id: claim.assignment_id.clone(),
            team_id: match &plan.assignee {
                Assignee::Team(team) => Some(team.clone()),
                Assignee::Assistant(_) => None,
            },
            parent_fallback,
            packet_sent: claim.packet_message_id.is_some(),
        };
        if claim.packet_message_id.is_none() {
            let (_, missing) = self.split_skills(host, user_id, &card_row).await?;
            let columns = store.columns(user_id, &card.project_id).await?;
            let detail = self.detail_of(user_id, card_row.clone()).await?;
            let packet = build_packet(&detail, &columns, plan.department.as_deref(), &missing);
            let text = format!(
                "You were assigned {} on the project board as {} ({}). Work on it as described below; read it again \
                 with `aioncore board show {} --packet`{}.\n\n{}\n[board-assignment {}]",
                reference(card.number),
                plan.department.as_deref().unwrap_or("brainstorm"),
                plan.role.as_str(),
                reference(card.number),
                match plan.department.as_deref() {
                    Some(department) if plan.role == LinkRole::Review => format!(
                        " and record your verdict with `aioncore board review {} {department}`",
                        reference(card.number)
                    ),
                    _ => String::new(),
                },
                packet.text,
                claim.assignment_id
            );
            self.ensure_claim(user_id, &card.project_id, &plan.key, &claim.token)
                .await?;
            let sent = match &plan.assignee {
                Assignee::Assistant(_) => match store.first_user_message(user_id, &conversation_id).await? {
                    // An earlier attempt delivered it: return that message.
                    Some(existing) => Ok(existing),
                    None => host.send_message(user_id, &conversation_id, &text).await,
                },
                Assignee::Team(team) => host
                    .send_team_message(user_id, team, &text, &claim.assignment_id)
                    .await
                    .map(|_| format!("team:{}", claim.assignment_id)),
            };
            match sent {
                Ok(message_id) => {
                    response.packet_sent = true;
                    self.finish(user_id, &card.project_id, plan, claim, &message_id, &response)
                        .await?;
                    if claim.resumed {
                        // A retry delivered it: clears an earlier
                        // `packet_send_failed` on the card.
                        self.card_event(actor, card, PACKET_SENT, json!({ "conversation_id": conversation_id }))
                            .await?;
                    }
                }
                Err(error) => {
                    warn!(card_id = %card.id, error = %error, "board assign: packet send failed");
                    self.packet_failed(actor, card, plan, claim, &conversation_id, &error)
                        .await?;
                }
            }
        } else {
            self.finish(
                user_id,
                &card.project_id,
                plan,
                claim,
                claim.packet_message_id.as_deref().unwrap_or_default(),
                &response,
            )
            .await?;
        }
        info!(
            card_id = %card.id,
            conversation_id = %conversation_id,
            packet_sent = response.packet_sent,
            "board card assigned"
        );
        Ok(response)
    }

    /// Step 0: reserve or take over the idempotency key (bookkeeping write,
    /// no event).
    async fn reserve(&self, user_id: &str, project_id: &str, plan: &AssignPlan) -> BoardResult<Reservation> {
        let mut tx = self.store().begin().await?;
        let now = now_ms();
        let token = generate_id();
        let reservation = match tx.idempotency(user_id, project_id, &plan.key).await? {
            None => {
                let assignment_id = generate_id();
                tx.insert_idempotency(&BoardIdempotencyRow {
                    user_id: user_id.to_owned(),
                    project_id: project_id.to_owned(),
                    key: plan.key.clone(),
                    request_hash: plan.hash.clone(),
                    state: "pending".into(),
                    assignment_id: Some(assignment_id.clone()),
                    response: None,
                    claim_token: Some(token.clone()),
                    packet_message_id: None,
                    created_at: now,
                    updated_at: now,
                })
                .await?;
                Reservation::Claimed(Claim {
                    assignment_id,
                    token,
                    packet_message_id: None,
                    resumed: false,
                })
            }
            Some(row) if row.request_hash != plan.hash => {
                return Err(BoardError::new(
                    BoardErrorCode::IdempotencyConflict,
                    "this idempotency_key was used for a different request",
                ));
            }
            Some(row) if row.state == "done" => {
                let response = row.response.unwrap_or_default();
                return Ok(Reservation::Done(Box::new(serde_json::from_str(&response)?)));
            }
            Some(row) => {
                if !tx
                    .take_over_idempotency(user_id, project_id, &plan.key, &token, now - ASSIGN_LEASE_MS, now)
                    .await?
                {
                    return Err(assignment_in_progress());
                }
                Reservation::Claimed(Claim {
                    assignment_id: row.assignment_id.unwrap_or_else(generate_id),
                    token,
                    packet_message_id: row.packet_message_id,
                    resumed: true,
                })
            }
        };
        tx.commit().await?;
        Ok(reservation)
    }

    /// Renew the lease while still holding the claim; a lost claim stops the
    /// caller before any further side effect.
    async fn ensure_claim(&self, user_id: &str, project_id: &str, key: &str, token: &str) -> BoardResult<()> {
        let mut tx = self.store().begin().await?;
        let Some(mut row) = tx.idempotency(user_id, project_id, key).await? else {
            return Err(assignment_in_progress());
        };
        row.updated_at = now_ms();
        if !tx.update_idempotency(&row, token).await? {
            return Err(assignment_in_progress());
        }
        tx.commit().await?;
        Ok(())
    }

    /// Whether `token` still holds the reservation (read in a transaction).
    async fn claim_held(&self, user_id: &str, project_id: &str, key: &str, token: &str) -> bool {
        let held: BoardResult<bool> = async {
            let mut tx = self.store().begin().await?;
            let held = tx
                .idempotency(user_id, project_id, key)
                .await?
                .is_some_and(|row| row.claim_token.as_deref() == Some(token));
            tx.commit().await?;
            Ok(held)
        }
        .await;
        held.unwrap_or(false)
    }

    /// Best-effort: mark the lease stale so a retry can take over at once.
    async fn release(&self, user_id: &str, project_id: &str, key: &str, token: &str) {
        let result: BoardResult<()> = async {
            let mut tx = self.store().begin().await?;
            if let Some(mut row) = tx.idempotency(user_id, project_id, key).await?
                && row.state == "pending"
            {
                row.updated_at = 0;
                tx.update_idempotency(&row, token).await?;
            }
            tx.commit().await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            warn!(error = %error, "board assign: lease not released");
        }
    }

    /// Step 3: link + assignee + one `card_assigned` event, under the claim.
    async fn link_assignment(
        &self,
        actor: &BoardActor,
        card: &BoardCardRow,
        plan: &AssignPlan,
        claim: &Claim,
        conversation_id: &str,
        parent_fallback: Option<ParentFallback>,
    ) -> BoardResult<BoardLinkDto> {
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let Some(mut reservation) = tx.idempotency(user_id, &card.project_id, &plan.key).await? else {
                return Err(assignment_in_progress());
            };
            if reservation.claim_token.as_deref() != Some(claim.token.as_str()) {
                return Err(assignment_in_progress());
            }
            let mut row = Self::tx_card(tx.as_mut(), actor, &card.id).await?;
            if row.archived_at.is_some() {
                return Err(crate::service_cards::card_archived());
            }
            let links = tx.links(user_id, &card.id).await?;
            let (kind_ref, kind) = match &plan.assignee {
                Assignee::Assistant(id) => (id.clone(), "assistant"),
                Assignee::Team(id) => (id.clone(), "team"),
            };
            let existing = links
                .iter()
                .find(|link| link.conversation_id == conversation_id)
                .cloned();
            let link = match existing {
                // Resume: an earlier attempt of THIS reservation finished the
                // link step (it records the conversation in `response`).
                Some(link) if reservation.response.is_some() => {
                    reservation.updated_at = now_ms();
                    tx.update_idempotency(&reservation, &claim.token).await?;
                    tx.commit().await?;
                    return Ok(view(&link));
                }
                Some(_) => return Err(already_linked("this conversation is already linked to the card")),
                None => {
                    if plan.role == LinkRole::Lead && links.iter().any(|link| link.role == LinkRole::Lead.as_str()) {
                        return Err(already_linked("the card already has a lead conversation"));
                    }
                    BoardLinkRow {
                        card_id: card.id.clone(),
                        conversation_id: conversation_id.to_owned(),
                        project_id: card.project_id.clone(),
                        user_id: user_id.to_owned(),
                        role: plan.role.as_str().to_owned(),
                        department: plan.department.clone(),
                        created_at: now_ms(),
                    }
                }
            };
            tx.insert_link(&link).await?;
            let expected = row.version;
            row.assignee_kind = Some(kind.to_owned());
            row.assignee_ref = Some(kind_ref);
            row.version += 1;
            row.updated_at = now_ms();
            if !tx.update_card(&row, expected).await? {
                return Err(crate::service_cards::card_version_conflict(&row));
            }
            let dto = Self::tx_card_dto(tx.as_mut(), &row).await?;
            let mut payload = json!({
                "card": dto,
                "version": row.version,
                "link": link,
                "assignment_id": claim.assignment_id,
                "conversation_id": conversation_id,
            });
            if let Assignee::Team(team) = &plan.assignee {
                payload["team_id"] = json!(team);
            }
            if let Some(fallback) = parent_fallback {
                payload["parent_fallback"] = json!(fallback);
            }
            let event = Self::append_event(
                tx.as_mut(),
                actor,
                &card.project_id,
                Some(&card.id),
                CARD_ASSIGNED,
                payload,
            )
            .await?;
            reservation.updated_at = now_ms();
            reservation.response = Some(json!({ "conversation_id": conversation_id }).to_string());
            if !tx.update_idempotency(&reservation, &claim.token).await? {
                return Err(assignment_in_progress());
            }
            self.commit(tx, user_id, &Committed::card(event, dto)).await?;
            Ok(view(&link))
        }
        .instrument(write_span(CARD_ASSIGNED, actor, Some(&card.id)))
        .await
    }

    /// Step 4 success: store the packet message id and the response (`done`).
    async fn finish(
        &self,
        user_id: &str,
        project_id: &str,
        plan: &AssignPlan,
        claim: &Claim,
        message_id: &str,
        response: &BoardAssignResponse,
    ) -> BoardResult<()> {
        let mut tx = self.store().begin().await?;
        let Some(mut row) = tx.idempotency(user_id, project_id, &plan.key).await? else {
            return Err(assignment_in_progress());
        };
        row.state = "done".into();
        row.packet_message_id = Some(message_id.to_owned());
        row.response = Some(serde_json::to_string(response)?);
        row.updated_at = now_ms();
        if !tx.update_idempotency(&row, &claim.token).await? {
            return Err(assignment_in_progress());
        }
        tx.commit().await?;
        Ok(())
    }

    /// Step 4 failure: one `packet_send_failed` event; the lease is released
    /// so the same request resends.
    async fn packet_failed(
        &self,
        actor: &BoardActor,
        card: &BoardCardRow,
        plan: &AssignPlan,
        claim: &Claim,
        conversation_id: &str,
        error: &str,
    ) -> BoardResult<()> {
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let Some(mut row) = tx.idempotency(user_id, &card.project_id, &plan.key).await? else {
                return Err(assignment_in_progress());
            };
            row.updated_at = 0;
            if !tx.update_idempotency(&row, &claim.token).await? {
                return Err(assignment_in_progress());
            }
            let event = Self::append_event(
                tx.as_mut(),
                actor,
                &card.project_id,
                Some(&card.id),
                PACKET_SEND_FAILED,
                json!({
                    "conversation_id": conversation_id,
                    "assignment_id": claim.assignment_id,
                    "idempotency_key": plan.key,
                    "request": plan.request_view(),
                    "error": error.chars().take(300).collect::<String>(),
                }),
            )
            .await?;
            self.commit(tx, user_id, &Committed::event(event)).await
        }
        .instrument(write_span(PACKET_SEND_FAILED, actor, Some(&card.id)))
        .await
    }

    /// The card's lead conversation as parent when it is eligible (depth < 3,
    /// < 12 children, not a team member); otherwise a root plus the reason.
    async fn parent_for(&self, user_id: &str, card_id: &str) -> BoardResult<(Option<String>, Option<ParentFallback>)> {
        let Some(lead) = self
            .store()
            .links(user_id, card_id)
            .await?
            .into_iter()
            .find(|link| link.role == LinkRole::Lead.as_str() && link.conversation_exists)
        else {
            return Ok((None, None));
        };
        let Some(info) = self.store().conversation_info(user_id, &lead.conversation_id).await? else {
            return Ok((None, None));
        };
        Ok(if info.team_member {
            (None, Some(ParentFallback::Team))
        } else if info.depth >= MAX_PARENT_DEPTH {
            (None, Some(ParentFallback::Depth))
        } else if info.child_count >= MAX_CHILDREN {
            (None, Some(ParentFallback::Fanout))
        } else {
            (Some(info.conversation_id), None)
        })
    }

    /// Tagged skills split into (installed → enabled, missing → packet only).
    async fn split_skills(
        &self,
        host: &dyn crate::ports::BoardHostPort,
        user_id: &str,
        card: &BoardCardRow,
    ) -> BoardResult<(Vec<String>, Vec<String>)> {
        let tagged: Vec<String> = serde_json::from_str(&card.skills).unwrap_or_default();
        if tagged.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let installed: HashSet<String> = host
            .user_skill_names(user_id)
            .await
            .map_err(BoardError::Internal)?
            .into_iter()
            .collect();
        Ok(tagged.into_iter().partition(|skill| installed.contains(skill)))
    }
}

impl AssignPlan {
    /// The request as the UI resends it (`packet_send_failed`).
    fn request_view(&self) -> serde_json::Value {
        let mut request = json!({ "department": self.department, "role": self.role.as_str() });
        match &self.assignee {
            Assignee::Assistant(id) => request["assistant_id"] = json!(id),
            Assignee::Team(id) => request["team_id"] = json!(id),
        }
        request
    }
}

impl BoardService {
    /// One card event in its own write (no row change).
    async fn card_event(
        &self,
        actor: &BoardActor,
        card: &BoardCardRow,
        kind: &'static str,
        payload: serde_json::Value,
    ) -> BoardResult<()> {
        async {
            let mut tx = self.store().begin().await?;
            let settings = Self::tx_settings(tx.as_mut(), actor, &card.project_id).await?;
            Self::check_writable(&settings, actor)?;
            let event = Self::append_event(tx.as_mut(), actor, &card.project_id, Some(&card.id), kind, payload).await?;
            self.commit(tx, actor.user_id(), &Committed::event(event)).await
        }
        .instrument(write_span(kind, actor, Some(&card.id)))
        .await
    }
}

fn view(link: &BoardLinkRow) -> BoardLinkDto {
    link_dto(&BoardLinkViewRow {
        card_id: link.card_id.clone(),
        conversation_id: link.conversation_id.clone(),
        project_id: link.project_id.clone(),
        user_id: link.user_id.clone(),
        role: link.role.clone(),
        department: link.department.clone(),
        created_at: link.created_at,
        conversation_name: None,
        conversation_status: None,
        conversation_exists: true,
        conversation_archived_at: None,
    })
}
