//! Card relations and reads: detail, paged lists, comments, dependencies,
//! links, event pages, packet.

use std::collections::{HashMap, HashSet};

use aionui_api_types::{
    BoardCardDetail, BoardCardListQuery, BoardCardPage, BoardCommentRequest, BoardDependencyDto, BoardEventDto,
    BoardEventPage, BoardLinkDto, BoardPacketDto, LinkRole,
};
use aionui_common::{generate_id, now_ms};
use aionui_db::{
    BoardCardCursor, BoardCardFilter, BoardCardRow, BoardDepRow, BoardEventQuery, BoardEventScope, BoardLinkViewRow,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
use tracing::Instrument;

use crate::actor::BoardActor;
use crate::convert::{
    agent_card_ids, card_creators, card_dto, dependency_dto, event_dto, link_dto, question_dto, reference, review_dto,
    set_creator,
};
use crate::error::{BoardError, BoardErrorCode, BoardResult};
use crate::events::{COMMENTED, DEP_ADDED, DEP_REMOVED};
use crate::packet::build_packet;
use crate::rate_limit::WriteBudget;
use crate::service::{BoardService, Committed, require_owner, write_span};
use crate::validation::{COMMENT_MAX, DEPARTMENT_MAX, required_text};

pub const EVENTS_PAGE_DEFAULT: i64 = 100;
pub const EVENTS_PAGE_MAX: i64 = 500;
pub const CARDS_PAGE_DEFAULT: i64 = 100;
pub const CARDS_PAGE_MAX: i64 = 500;
const DETAIL_EVENTS: i64 = 50;

fn page_limit(requested: Option<i64>, default: i64, max: i64) -> i64 {
    requested.unwrap_or(default).clamp(1, max)
}

pub(crate) fn encode_cursor(card: &BoardCardRow) -> String {
    URL_SAFE_NO_PAD.encode(json!([card.column_key, card.position, card.id]).to_string())
}

fn decode_cursor(raw: &str) -> BoardResult<BoardCardCursor> {
    let invalid = || BoardError::invalid("cursor", "malformed cursor");
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| invalid())?;
    let (column_key, position, id): (String, i64, String) = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    Ok(BoardCardCursor {
        column_key,
        position,
        id,
    })
}

/// Whether `from` reaches `to` along `card → depends_on` edges.
fn reaches(edges: &[BoardDepRow], from: &str, to: &str) -> bool {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges {
        adjacency
            .entry(edge.card_id.as_str())
            .or_default()
            .push(edge.depends_on_card_id.as_str());
    }
    let mut stack = vec![from];
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if seen.insert(node)
            && let Some(next) = adjacency.get(node)
        {
            stack.extend(next.iter().copied());
        }
    }
    false
}

impl BoardService {
    /// `GET /api/board/cards/{id}`: card, reviews, questions, links (with
    /// conversation status), dependencies both ways, last 50 events.
    pub async fn card_detail(&self, actor: &BoardActor, card_id: &str) -> BoardResult<BoardCardDetail> {
        let card = self.scoped_card(actor, card_id).await?;
        self.detail_of(actor.user_id(), card).await
    }

    pub(crate) async fn detail_of(&self, user_id: &str, card: BoardCardRow) -> BoardResult<BoardCardDetail> {
        let store = self.store();
        let columns = store.columns(user_id, &card.project_id).await?;
        let depends_on = store.dependencies(user_id, &card.id).await?;
        let dependents = store.dependents(user_id, &card.id).await?;
        let ids: Vec<String> = depends_on
            .iter()
            .map(|dep| dep.depends_on_card_id.clone())
            .chain(dependents.iter().map(|dep| dep.card_id.clone()))
            .collect();
        let related: HashMap<String, BoardCardRow> = store
            .cards_by_ids(user_id, &ids)
            .await?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        let resolve = |id: &str| related.get(id).map(|row| dependency_dto(row, &columns));
        let assignee_skills = match (
            card.assignee_kind.as_deref(),
            card.assignee_ref.as_deref(),
            self.inner.host.get(),
        ) {
            (Some(kind @ ("assistant" | "team")), Some(assignee), Some(host)) => host
                .assignee_skills(user_id, kind, assignee)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(card_id = %card.id, error = %error, "board: assignee skills unavailable");
                    Vec::new()
                }),
            _ => Vec::new(),
        };
        Ok(BoardCardDetail {
            assignee_skills,
            reviews: store.reviews(user_id, &card.id).await?.iter().map(review_dto).collect(),
            questions: store
                .questions(user_id, &card.id)
                .await?
                .iter()
                .map(question_dto)
                .collect(),
            links: store.links(user_id, &card.id).await?.iter().map(link_dto).collect(),
            depends_on: depends_on
                .iter()
                .filter_map(|dep| resolve(&dep.depends_on_card_id))
                .collect(),
            blocks: dependents.iter().filter_map(|dep| resolve(&dep.card_id)).collect(),
            events: store
                .recent_card_events(user_id, &card.id, DETAIL_EVENTS)
                .await?
                .iter()
                .map(event_dto)
                .collect(),
            card: {
                let mut dto = card_dto(
                    &card,
                    depends_on
                        .iter()
                        .filter_map(|dep| resolve(&dep.depends_on_card_id))
                        .collect(),
                );
                let creators = card_creators(store.card_creators(user_id, &agent_card_ids([&card])).await?);
                set_creator(
                    &card.id,
                    &mut dto.creator_conversation_id,
                    &mut dto.creator_name,
                    &creators,
                );
                dto
            },
        })
    }

    /// Paged card list (`GET /api/projects/{pid}/board/cards`, agent `list`).
    /// `assignee_me` resolves `assignee=me` for agents.
    pub async fn list_cards(
        &self,
        user_id: &str,
        project_id: &str,
        query: &BoardCardListQuery,
        assignee_me: Option<&str>,
    ) -> BoardResult<BoardCardPage> {
        let assignee_ref = match query.assignee.as_deref() {
            Some("me") => Some(
                assignee_me
                    .ok_or_else(|| BoardError::invalid("assignee", "`me` is only defined for agent callers"))?
                    .to_owned(),
            ),
            other => other.map(str::to_owned),
        };
        let filter = BoardCardFilter {
            column_key: query.column.clone(),
            assignee_ref,
            card_type: query.card_type.clone(),
            priority: query.priority.clone(),
            label: query.label.clone(),
            skill: query.skill.clone(),
            blocked: query.blocked,
            include_archived: query.include_archived,
        };
        let cursor = query.cursor.as_deref().map(decode_cursor).transpose()?;
        let limit = page_limit(query.limit, CARDS_PAGE_DEFAULT, CARDS_PAGE_MAX);
        let mut rows = self
            .store()
            .cards_page(user_id, project_id, &filter, cursor.as_ref(), limit + 1)
            .await?;
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        let store = self.store();
        let columns = store.columns(user_id, project_id).await?;
        let deps = store.project_dependencies(user_id, project_id).await?;
        let items = self.summaries(user_id, &rows, &columns, &deps).await?;
        Ok(BoardCardPage {
            next_cursor: if has_more { rows.last().map(encode_cursor) } else { None },
            items,
            has_more,
        })
    }

    /// User-router card list: checks project ownership first.
    pub async fn list_project_cards(
        &self,
        actor: &BoardActor,
        project_id: &str,
        query: &BoardCardListQuery,
    ) -> BoardResult<BoardCardPage> {
        self.user_project(actor.user_id(), project_id).await?;
        self.list_cards(actor.user_id(), project_id, query, None).await
    }

    async fn event_page(
        &self,
        user_id: &str,
        scope: BoardEventScope<'_>,
        after_seq: Option<i64>,
        limit: Option<i64>,
        kinds: Option<&[&str]>,
    ) -> BoardResult<BoardEventPage> {
        let after_seq = after_seq.unwrap_or(0).max(0);
        let limit = page_limit(limit, EVENTS_PAGE_DEFAULT, EVENTS_PAGE_MAX);
        let mut rows = self
            .store()
            .events(
                user_id,
                &BoardEventQuery {
                    scope,
                    after_seq,
                    limit: limit + 1,
                    kinds,
                },
            )
            .await?;
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        Ok(BoardEventPage {
            next_after_seq: rows.last().map(|row| row.seq).unwrap_or(after_seq),
            items: rows.iter().map(event_dto).collect(),
            has_more,
        })
    }

    pub async fn card_events(
        &self,
        actor: &BoardActor,
        card_id: &str,
        after_seq: Option<i64>,
        limit: Option<i64>,
    ) -> BoardResult<BoardEventPage> {
        self.scoped_card(actor, card_id).await?;
        self.event_page(actor.user_id(), BoardEventScope::Card(card_id), after_seq, limit, None)
            .await
    }

    /// Comments = `commented` events of a card, paged by `seq`.
    pub async fn card_comments(
        &self,
        actor: &BoardActor,
        card_id: &str,
        after_seq: Option<i64>,
        limit: Option<i64>,
    ) -> BoardResult<BoardEventPage> {
        self.scoped_card(actor, card_id).await?;
        self.event_page(
            actor.user_id(),
            BoardEventScope::Card(card_id),
            after_seq,
            limit,
            Some(&[COMMENTED]),
        )
        .await
    }

    /// Project events (user router checks ownership; agents pass their own
    /// project).
    pub async fn project_events(
        &self,
        actor: &BoardActor,
        project_id: &str,
        after_seq: Option<i64>,
        limit: Option<i64>,
    ) -> BoardResult<BoardEventPage> {
        if actor.as_agent().is_none() {
            self.user_project(actor.user_id(), project_id).await?;
        }
        self.event_page(
            actor.user_id(),
            BoardEventScope::Project(project_id),
            after_seq,
            limit,
            None,
        )
        .await
    }

    /// `GET /api/conversations/{cid}/board-events`: events written by that
    /// conversation, for the chat notices (F-KANBAN-06).
    pub async fn conversation_events(
        &self,
        user_id: &str,
        conversation_id: &str,
        after_seq: Option<i64>,
        limit: Option<i64>,
    ) -> BoardResult<BoardEventPage> {
        self.event_page(
            user_id,
            BoardEventScope::Conversation(conversation_id),
            after_seq,
            limit,
            None,
        )
        .await
    }

    /// `POST /api/board/cards/{id}/comments` and the agent `comment`. A
    /// comment with `answers` closes that open question of the card (its
    /// version bumps; the card's does not).
    pub async fn comment(
        &self,
        actor: &BoardActor,
        card_id: &str,
        request: BoardCommentRequest,
    ) -> BoardResult<BoardEventDto> {
        self.charge(actor, WriteBudget::Write)?;
        let body = required_text("body", &request.body, COMMENT_MAX)?;
        let to_department = request
            .to_department
            .as_deref()
            .map(|d| required_text("to_department", d, DEPARTMENT_MAX))
            .transpose()?;
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            if card.archived_at.is_some() {
                return Err(crate::service_cards::card_archived());
            }
            let event_id = generate_id();
            let mut payload = json!({ "body": body, "to_department": to_department, "answers": request.answers });
            if let Some(question_id) = request.answers.as_deref() {
                let mut question = tx
                    .question(user_id, card_id, question_id)
                    .await?
                    .ok_or_else(|| BoardError::not_found("question"))?;
                if question.answered_by_event.is_some() {
                    return Err(BoardError::invalid("answers", "the question is already answered"));
                }
                // Only the owner answers questions to `owner`; an agent answers
                // only questions to the department it is linked for.
                if let Some(agent) = actor.as_agent() {
                    let department = tx
                        .links(user_id, card_id)
                        .await?
                        .into_iter()
                        .find(|link| link.conversation_id == agent.conversation_id)
                        .and_then(|link| link.department);
                    if question.to_department == crate::rules::OWNER_DEPARTMENT
                        || department.as_deref() != Some(question.to_department.as_str())
                    {
                        return Err(BoardError::new(
                            BoardErrorCode::DepartmentMismatch,
                            "agents answer only questions addressed to their own department",
                        )
                        .with_details(
                            json!({ "to_department": question.to_department, "linked_department": department }),
                        ));
                    }
                }
                let expected = question.version;
                question.answered_by_event = Some(event_id.clone());
                question.version += 1;
                question.updated_at = now_ms();
                if !tx.update_question(&question, expected).await? {
                    return Err(BoardError::new(
                        BoardErrorCode::VersionConflict,
                        "the question changed meanwhile",
                    ));
                }
                payload["question"] = json!(question_dto(&question));
            }
            let event = Self::append_event_with_id(
                tx.as_mut(),
                event_id,
                actor,
                &card.project_id,
                Some(card_id),
                COMMENTED,
                payload,
            )
            .await?;
            self.commit(tx, user_id, &Committed::event(event.clone())).await?;
            Ok(event)
        }
        .instrument(write_span(COMMENTED, actor, Some(card_id)))
        .await
    }

    /// `PUT` / `DELETE /api/board/cards/{id}/deps/{other}` (owner only).
    /// Same project only; self-links and cycles → `422 board_dependency_cycle`.
    /// Adding an existing edge or removing a missing one writes nothing.
    pub async fn set_dependency(
        &self,
        actor: &BoardActor,
        card_id: &str,
        depends_on: &str,
        add: bool,
    ) -> BoardResult<Vec<BoardDependencyDto>> {
        require_owner(actor)?;
        if card_id == depends_on {
            return Err(BoardError::new(
                BoardErrorCode::DependencyCycle,
                "a card cannot depend on itself",
            ));
        }
        let card = self.scoped_card(actor, card_id).await?;
        let target = self.scoped_card(actor, depends_on).await?;
        if target.project_id != card.project_id {
            return Err(BoardError::not_found("card"));
        }
        self.refresh_archive(actor, &card.project_id).await?;
        let kind = if add { DEP_ADDED } else { DEP_REMOVED };
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let settings = Self::tx_settings(tx.as_mut(), actor, &card.project_id).await?;
            Self::check_writable(&settings, actor)?;
            let payload = if add {
                let edges = tx.project_dependencies(user_id, &card.project_id).await?;
                if reaches(&edges, depends_on, card_id) {
                    return Err(BoardError::new(
                        BoardErrorCode::DependencyCycle,
                        format!("{} already depends on {}", reference(target.number), reference(card.number)),
                    ));
                }
                let row = BoardDepRow {
                    card_id: card_id.to_owned(),
                    depends_on_card_id: depends_on.to_owned(),
                    project_id: card.project_id.clone(),
                    user_id: user_id.to_owned(),
                    created_at: now_ms(),
                };
                if !tx.insert_dependency(&row).await? {
                    return Ok(None);
                }
                json!({
                    "dep": { "card_id": row.card_id, "depends_on_card_id": row.depends_on_card_id, "created_at": row.created_at },
                    "number": card.number,
                    "depends_on_number": target.number,
                })
            } else {
                if !tx.delete_dependency(user_id, card_id, depends_on).await? {
                    return Ok(None);
                }
                json!({
                    "card_id": card_id,
                    "depends_on_card_id": depends_on,
                    "number": card.number,
                    "depends_on_number": target.number,
                })
            };
            let event = Self::append_event(tx.as_mut(), actor, &card.project_id, Some(card_id), kind, payload).await?;
            self.commit(tx, user_id, &Committed::event(event)).await?;
            Ok(Some(()))
        }
        .instrument(write_span(kind, actor, Some(card_id)))
        .await?;
        Ok(self.detail_of(actor.user_id(), card).await?.depends_on)
    }

    /// Agent `link <N> [--role]`: link the caller's own conversation to a
    /// card with role `other` (default), `build` or `qa`, department NULL
    /// (only assign sets departments and `lead|review` — R6).
    pub async fn link_caller(
        &self,
        actor: &BoardActor,
        card_id: &str,
        role: Option<LinkRole>,
    ) -> BoardResult<BoardLinkDto> {
        let Some(agent) = actor.as_agent() else {
            return Err(BoardError::invalid("role", "only agents link themselves"));
        };
        let role = role.unwrap_or(LinkRole::Other);
        if !matches!(role, LinkRole::Other | LinkRole::Build | LinkRole::Qa) {
            return Err(BoardError::invalid("role", "agents link with role other, build or qa"));
        }
        self.charge(actor, WriteBudget::Write)?;
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        let row = self
            .insert_link(actor, card_id, &agent.conversation_id, role, None)
            .await?;
        Ok(link_dto(&BoardLinkViewRow {
            card_id: row.card_id,
            conversation_id: row.conversation_id,
            project_id: row.project_id,
            user_id: row.user_id,
            role: row.role,
            department: row.department,
            created_at: row.created_at,
            conversation_name: None,
            conversation_status: None,
            conversation_exists: true,
            conversation_archived_at: None,
        }))
    }

    /// `GET …/packet` (SPEC §7). `department` = the caller's link department
    /// (open questions addressed to it are included).
    pub async fn packet(&self, actor: &BoardActor, card_id: &str) -> BoardResult<BoardPacketDto> {
        let card = self.scoped_card(actor, card_id).await?;
        let department = match actor.as_agent() {
            Some(agent) => self
                .store()
                .links(actor.user_id(), &card.id)
                .await?
                .into_iter()
                .find(|link| link.conversation_id == agent.conversation_id)
                .and_then(|link| link.department),
            None => None,
        };
        let columns = self.store().columns(actor.user_id(), &card.project_id).await?;
        let detail = self.detail_of(actor.user_id(), card).await?;
        Ok(build_packet(&detail, &columns, department.as_deref(), &[]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let card = BoardCardRow {
            id: "c1".into(),
            project_id: "p".into(),
            user_id: "u".into(),
            number: 1,
            title: "t".into(),
            description: String::new(),
            card_type: "bug".into(),
            priority: "low".into(),
            size: None,
            tier: None,
            column_key: "todo".into(),
            position: -1024,
            labels: "[]".into(),
            skills: "[]".into(),
            routing: "[]".into(),
            creator_kind: "user".into(),
            creator_ref: "u".into(),
            assignee_kind: None,
            assignee_ref: None,
            blocked: false,
            blocked_reason: None,
            blocked_by: None,
            change_id: None,
            metadata: "{}".into(),
            version: 1,
            archived_at: None,
            created_at: 0,
            updated_at: 0,
        };
        let cursor = decode_cursor(&encode_cursor(&card)).unwrap();
        assert_eq!(cursor.column_key, "todo");
        assert_eq!(cursor.position, -1024);
        assert_eq!(cursor.id, "c1");
        assert!(decode_cursor("not base64!").is_err());
        assert!(decode_cursor(&URL_SAFE_NO_PAD.encode("[1]")).is_err());
    }

    #[test]
    fn cycle_detection_follows_edges() {
        let edge = |a: &str, b: &str| BoardDepRow {
            card_id: a.into(),
            depends_on_card_id: b.into(),
            project_id: "p".into(),
            user_id: "u".into(),
            created_at: 0,
        };
        let edges = vec![edge("a", "b"), edge("b", "c")];
        assert!(reaches(&edges, "a", "c"));
        assert!(!reaches(&edges, "c", "a"));
    }
}
