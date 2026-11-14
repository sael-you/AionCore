//! Reviews (R6), the questions verdicts open, and owner/supervisor links.
//!
//! One current verdict per department (history in `review_updated`
//! events); every update bumps the review's own version, never the card's.
//! On the agent router the department must equal the caller's link
//! department (`403 board_department_mismatch`). Links created here (owner or
//! supervisor) may carry a department and the roles `lead|review`.

use aionui_api_types::{BoardLinkDto, BoardLinkRequest, BoardReviewRequest, BoardReviewResult, LinkRole};
use aionui_common::{generate_id, now_ms};
use aionui_db::{BoardLinkRow, BoardLinkViewRow, BoardQuestionRow, BoardReviewRow};
use serde_json::json;
use tracing::Instrument;

use crate::actor::BoardActor;
use crate::convert::{link_dto, question_dto, review_dto};
use crate::error::{BoardError, BoardErrorCode, BoardResult};
use crate::events::{LINK_ADDED, LINK_REMOVED, REVIEW_UPDATED};
use crate::rate_limit::WriteBudget;
use crate::service::{BoardService, Committed, require_owner_or_system, write_span};
use crate::validation::{COMMENT_MAX, DEPARTMENT_MAX, DESCRIPTION_MAX, free_text, required_text};

pub const FINDINGS_MAX: usize = 20;
pub const FINDING_TEXT_MAX: usize = 500;
pub const ASSUMPTIONS_MAX: usize = 20;
pub const VERDICT_QUESTIONS_MAX: usize = 10;

/// A validated verdict body.
struct Verdict {
    request: BoardReviewRequest,
    summary: String,
    recommendation: String,
}

fn validate_verdict(request: BoardReviewRequest) -> BoardResult<Verdict> {
    let summary = free_text("summary", &request.summary, COMMENT_MAX)?;
    let recommendation = free_text("recommendation", &request.recommendation, COMMENT_MAX)?;
    if request.findings.len() > FINDINGS_MAX {
        return Err(BoardError::invalid(
            "findings",
            format!("at most {FINDINGS_MAX} findings"),
        ));
    }
    for finding in &request.findings {
        required_text("findings.claim", &finding.claim, FINDING_TEXT_MAX)?;
        free_text("findings.evidence", &finding.evidence, FINDING_TEXT_MAX)?;
    }
    if request.assumptions.len() > ASSUMPTIONS_MAX {
        return Err(BoardError::invalid(
            "assumptions",
            format!("at most {ASSUMPTIONS_MAX} assumptions"),
        ));
    }
    for assumption in &request.assumptions {
        free_text("assumptions", assumption, FINDING_TEXT_MAX)?;
    }
    if request.questions.len() > VERDICT_QUESTIONS_MAX {
        return Err(BoardError::invalid(
            "questions",
            format!("at most {VERDICT_QUESTIONS_MAX} questions"),
        ));
    }
    for question in &request.questions {
        required_text("questions.to", &question.to, DEPARTMENT_MAX)?;
        required_text("questions.text", &question.text, DESCRIPTION_MAX.min(2_000))?;
    }
    if let Some(confidence) = request.confidence
        && !(0.0..=1.0).contains(&confidence)
    {
        return Err(BoardError::invalid("confidence", "confidence is between 0 and 1"));
    }
    if request.tokens_used.is_some_and(|tokens| tokens < 0) {
        return Err(BoardError::invalid("tokens_used", "tokens_used is not negative"));
    }
    Ok(Verdict {
        summary,
        recommendation,
        request,
    })
}

impl BoardService {
    /// `PUT …/reviews/{department}` (owner, or an agent linked to the card
    /// with that department).
    pub async fn submit_review(
        &self,
        actor: &BoardActor,
        card_id: &str,
        department: &str,
        request: BoardReviewRequest,
    ) -> BoardResult<BoardReviewResult> {
        if matches!(actor, BoardActor::System { .. }) {
            return Err(BoardError::new(
                BoardErrorCode::SystemForbidden,
                "the supervisor does not write verdicts",
            ));
        }
        self.charge(actor, WriteBudget::Write)?;
        let department = required_text("department", department, DEPARTMENT_MAX)?;
        let verdict = validate_verdict(request)?;
        let card = self.scoped_card(actor, card_id).await?;
        if let Some(agent) = actor.as_agent() {
            let linked = self
                .store()
                .links(actor.user_id(), card_id)
                .await?
                .into_iter()
                .find(|link| link.conversation_id == agent.conversation_id)
                .and_then(|link| link.department);
            if linked.as_deref() != Some(department.as_str()) {
                return Err(BoardError::new(
                    BoardErrorCode::DepartmentMismatch,
                    "agents review only for the department they were assigned",
                )
                .with_details(json!({ "department": department, "linked_department": linked })));
            }
        }
        self.refresh_archive(actor, &card.project_id).await?;
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            if card.archived_at.is_some() {
                return Err(crate::service_cards::card_archived());
            }
            let now = now_ms();
            let existing = tx
                .reviews(user_id, card_id)
                .await?
                .into_iter()
                .find(|review| review.department == department);
            let request = &verdict.request;
            let row = BoardReviewRow {
                id: existing.as_ref().map(|r| r.id.clone()).unwrap_or_else(generate_id),
                card_id: card.id.clone(),
                project_id: card.project_id.clone(),
                user_id: user_id.to_owned(),
                department: department.clone(),
                reviewer_ref: actor.actor_ref().to_owned(),
                conversation_id: actor.conversation_id().map(str::to_owned),
                verdict: request.verdict.as_str().to_owned(),
                summary: verdict.summary.clone(),
                findings: serde_json::to_string(&request.findings)?,
                recommendation: verdict.recommendation.clone(),
                assumptions: serde_json::to_string(&request.assumptions)?,
                confidence: request.confidence,
                tokens_used: request.tokens_used.unwrap_or(0),
                version: existing.as_ref().map(|r| r.version + 1).unwrap_or(1),
                created_at: existing.as_ref().map(|r| r.created_at).unwrap_or(now),
                updated_at: now,
            };
            if !tx.upsert_review(&row, existing.as_ref().map(|r| r.version)).await? {
                return Err(BoardError::new(
                    BoardErrorCode::VersionConflict,
                    "the review changed meanwhile; retry",
                ));
            }
            let mut questions = Vec::with_capacity(request.questions.len());
            for question in &request.questions {
                let row = BoardQuestionRow {
                    id: generate_id(),
                    card_id: card.id.clone(),
                    project_id: card.project_id.clone(),
                    user_id: user_id.to_owned(),
                    from_department: department.clone(),
                    to_department: question.to.trim().to_owned(),
                    text: question.text.trim().to_owned(),
                    answered_by_event: None,
                    version: 1,
                    created_at: now,
                    updated_at: now,
                };
                tx.insert_question(&row).await?;
                questions.push(question_dto(&row));
            }
            let review = review_dto(&row);
            let event = Self::append_event(
                tx.as_mut(),
                actor,
                &card.project_id,
                Some(&card.id),
                REVIEW_UPDATED,
                json!({
                    "department": department,
                    "verdict": row.verdict,
                    "review": review,
                    "version": row.version,
                    "questions": questions,
                }),
            )
            .await?;
            let committed = Committed {
                event,
                card: None,
                review: Some(review.clone()),
            };
            self.commit(tx, user_id, &committed).await?;
            Ok(BoardReviewResult { review, questions })
        }
        .instrument(write_span(REVIEW_UPDATED, actor, Some(card_id)))
        .await
    }

    /// `POST /api/board/cards/{id}/links` (owner / supervisor): link a
    /// conversation of the same user and project (R7b; other scopes → 404).
    pub async fn add_link(
        &self,
        actor: &BoardActor,
        card_id: &str,
        request: BoardLinkRequest,
    ) -> BoardResult<BoardLinkDto> {
        require_owner_or_system(actor)?;
        let department = request
            .department
            .as_deref()
            .map(|d| required_text("department", d, DEPARTMENT_MAX))
            .transpose()?;
        let card = self.scoped_card(actor, card_id).await?;
        let conversation = self
            .store()
            .conversation_info(actor.user_id(), &request.conversation_id)
            .await?
            .filter(|info| info.project_id.as_deref() == Some(card.project_id.as_str()))
            .ok_or_else(|| BoardError::not_found("conversation"))?;
        self.refresh_archive(actor, &card.project_id).await?;
        let row = self
            .insert_link(actor, &card.id, &conversation.conversation_id, request.role, department)
            .await?;
        Ok(link_dto(&BoardLinkViewRow {
            card_id: row.card_id,
            conversation_id: row.conversation_id,
            project_id: row.project_id,
            user_id: row.user_id,
            role: row.role,
            department: row.department,
            created_at: row.created_at,
            conversation_name: Some(conversation.name),
            conversation_status: None,
            conversation_exists: true,
            conversation_archived_at: conversation.archived.then_some(0),
        }))
    }

    /// Insert one link in its own write (one `link_added` event). A second
    /// link of the same conversation, or a second `lead`, is
    /// `409 board_already_linked`.
    pub(crate) async fn insert_link(
        &self,
        actor: &BoardActor,
        card_id: &str,
        conversation_id: &str,
        role: LinkRole,
        department: Option<String>,
    ) -> BoardResult<BoardLinkRow> {
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            let links = tx.links(user_id, card_id).await?;
            if links.iter().any(|link| link.conversation_id == conversation_id) {
                return Err(already_linked("this conversation is already linked to the card"));
            }
            if role == LinkRole::Lead && links.iter().any(|link| link.role == LinkRole::Lead.as_str()) {
                return Err(already_linked("the card already has a lead conversation"));
            }
            let row = BoardLinkRow {
                card_id: card_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
                project_id: card.project_id.clone(),
                user_id: user_id.to_owned(),
                role: role.as_str().to_owned(),
                department,
                created_at: now_ms(),
            };
            tx.insert_link(&row).await?;
            let event = Self::append_event(
                tx.as_mut(),
                actor,
                &card.project_id,
                Some(card_id),
                LINK_ADDED,
                json!({ "link": row }),
            )
            .await?;
            self.commit(tx, user_id, &Committed::event(event)).await?;
            Ok(row)
        }
        .instrument(write_span(LINK_ADDED, actor, Some(card_id)))
        .await
    }

    /// `DELETE /api/board/cards/{id}/links/{cid}` (owner / supervisor): a
    /// hard delete written as a `link_removed` tombstone.
    pub async fn remove_link(&self, actor: &BoardActor, card_id: &str, conversation_id: &str) -> BoardResult<()> {
        require_owner_or_system(actor)?;
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            if !tx.delete_link(user_id, card_id, conversation_id).await? {
                return Err(BoardError::not_found("link"));
            }
            let event = Self::append_event(
                tx.as_mut(),
                actor,
                &card.project_id,
                Some(card_id),
                LINK_REMOVED,
                json!({ "card_id": card_id, "conversation_id": conversation_id }),
            )
            .await?;
            self.commit(tx, user_id, &Committed::event(event)).await
        }
        .instrument(write_span(LINK_REMOVED, actor, Some(card_id)))
        .await
    }
}

pub(crate) fn already_linked(message: &str) -> BoardError {
    BoardError::new(BoardErrorCode::AlreadyLinked, message)
}
