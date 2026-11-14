//! Gate decisions (R3): `POST /api/board/cards/{id}/gate` — owner only,
//! idempotent by `idempotency_key`.
//!
//! The key, the request hash and the response are stored in
//! `board_idempotency` (state `done`) in the same transaction as the move and
//! its event: a retry with the same key returns the stored card without a
//! second event; the same key with a different request is `409
//! board_idempotency_conflict`. Before S5 the Ready-to-deploy gate answers
//! `403 board_feature_not_enabled` (its only exit is the owner's force).

use std::collections::HashMap;

use aionui_api_types::{BoardCardDto, BoardGateRequest, GateDecision};
use aionui_common::now_ms;
use aionui_db::{BoardColumnRow, BoardIdempotencyRow, BoardPlacement, BoardPlacementOutcome};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::Instrument;

use crate::actor::BoardActor;
use crate::error::{BoardError, BoardErrorCode, BoardResult};
use crate::events::{GATE_APPROVED, GATE_REJECTED};
use crate::rules::exit_rule_missing;
use crate::service::{BoardService, require_owner, write_span};
use crate::service_cards::{card_archived, exit_rule, gate_rule, is_gate, wip_limit};
use crate::templates::READY_TO_DEPLOY;
use crate::validation::{BLOCK_REASON_MAX, free_text, required_text};

pub const IDEMPOTENCY_KEY_MAX: usize = 200;

/// Stable hash of a request (idempotency key reuse detection).
pub(crate) fn request_hash(value: &Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

impl BoardService {
    /// Approve or reject the card waiting in a gate column; the card moves to
    /// the gate's `on_approve` / `on_reject` column.
    pub async fn decide_gate(
        &self,
        actor: &BoardActor,
        card_id: &str,
        request: BoardGateRequest,
    ) -> BoardResult<BoardCardDto> {
        require_owner(actor)?;
        let key = required_text("idempotency_key", &request.idempotency_key, IDEMPOTENCY_KEY_MAX)?;
        let reason = free_text("reason", request.reason.as_deref().unwrap_or(""), BLOCK_REASON_MAX)?;
        let reason = Some(reason.trim().to_owned()).filter(|r| !r.is_empty());
        let hash = request_hash(&json!({
            "op": "gate",
            "card_id": card_id,
            "decision": request.decision.as_str(),
            "reason": reason,
        }));
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        let kind = match request.decision {
            GateDecision::Approve => GATE_APPROVED,
            GateDecision::Reject => GATE_REJECTED,
        };
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            if let Some(stored) = tx.idempotency(user_id, &card.project_id, &key).await? {
                if stored.request_hash != hash || stored.state != "done" {
                    return Err(BoardError::new(
                        BoardErrorCode::IdempotencyConflict,
                        "this idempotency_key was used for a different request",
                    ));
                }
                // Replay: the stored response, no second event.
                let response = stored.response.unwrap_or_default();
                return serde_json::from_str(&response).map_err(BoardError::from);
            }
            let mut card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            if card.archived_at.is_some() {
                return Err(card_archived());
            }
            let columns = tx.columns(user_id, &card.project_id).await?;
            let by_key: HashMap<&str, &BoardColumnRow> = columns.iter().map(|c| (c.key.as_str(), c)).collect();
            let from = by_key
                .get(card.column_key.as_str())
                .copied()
                .filter(|column| is_gate(column))
                .ok_or_else(|| BoardError::invalid("card", "the card is not waiting in a gate column"))?;
            if from.key == READY_TO_DEPLOY {
                return Err(BoardError::new(
                    BoardErrorCode::FeatureNotEnabled,
                    "deploy approvals are not available yet; force the card out with a reason instead",
                ));
            }
            let rule = gate_rule(from);
            let target_key = rule
                .get(match request.decision {
                    GateDecision::Approve => "on_approve",
                    GateDecision::Reject => "on_reject",
                })
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let to = by_key
                .get(target_key.as_str())
                .copied()
                .filter(|column| !column.archived)
                .ok_or_else(|| {
                    BoardError::invalid("gate", format!("the gate's target `{target_key}` is not a column"))
                })?;
            if request.decision == GateDecision::Approve {
                let blockers = self.blockers(tx.as_mut(), &card, &columns).await?;
                if !blockers.is_clear() {
                    return Err(BoardError::new(
                        BoardErrorCode::Blocked,
                        "the card is blocked and cannot move forward",
                    )
                    .with_details(blockers.details()));
                }
                if let Some(exit) = exit_rule(from) {
                    let reviews = tx.reviews(user_id, &card.id).await?;
                    let questions = tx.questions(user_id, &card.id).await?;
                    let missing = exit_rule_missing(&exit, &card, &reviews, &questions);
                    if !missing.is_empty() {
                        return Err(BoardError::new(
                            BoardErrorCode::ExitRule,
                            format!("`{}` exit rule is not met", from.key),
                        )
                        .with_details(json!({ "missing": missing })));
                    }
                }
            }
            if let Some(limit) = to.wip_limit {
                let count = tx
                    .count_column_cards(user_id, &card.project_id, &to.key, Some(&card.id))
                    .await?;
                if count >= limit {
                    return Err(wip_limit(&to.key, limit, count));
                }
            }
            let BoardPlacementOutcome::Placed(position) = tx
                .place(user_id, &card.project_id, &to.key, Some(&card.id), BoardPlacement::Tail)
                .await?
            else {
                return Err(BoardError::Internal("tail placement reported a missing anchor".into()));
            };
            let from_key = std::mem::replace(&mut card.column_key, to.key.clone());
            card.position = position;
            let expected = card.version;
            card.version += 1;
            card.updated_at = now_ms();
            if !tx.update_card(&card, expected).await? {
                return Err(crate::service_cards::card_version_conflict(&card));
            }
            let dto = Self::tx_card_dto(tx.as_mut(), &card).await?;
            let event = Self::append_event(
                tx.as_mut(),
                actor,
                &card.project_id,
                Some(&card.id),
                kind,
                json!({ "card": dto, "version": card.version, "from": from_key, "to": to.key, "reason": reason }),
            )
            .await?;
            let now = now_ms();
            tx.insert_idempotency(&BoardIdempotencyRow {
                user_id: user_id.to_owned(),
                project_id: card.project_id.clone(),
                key: key.clone(),
                request_hash: hash.clone(),
                state: "done".into(),
                assignment_id: None,
                response: Some(serde_json::to_string(&dto)?),
                claim_token: None,
                packet_message_id: None,
                created_at: now,
                updated_at: now,
            })
            .await?;
            self.commit(tx, user_id, &crate::service::Committed::card(event, dto.clone()))
                .await?;
            Ok(dto)
        }
        .instrument(write_span(kind, actor, Some(card_id)))
        .await
    }
}
