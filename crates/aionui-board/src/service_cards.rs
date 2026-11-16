//! Card writes: create, update, move, archive/restore, block/unblock.
//!
//! Rules enforced here: R1 (agents on a disabled board), R2 (WIP), R3 (gate
//! columns and `deploying` on plain moves), R4 (exit rules), R5 (blocked
//! cards cannot move forward), R7 (agents stay in their project, `agent_move`,
//! the Board Keeper privilege), R7b (user-scoped lookups), R8 (one event +
//! one broadcast per write), R9 (limits, agent rate limits), versions.

use std::collections::HashMap;

use aionui_api_types::{
    AgentAccess, AgentMove, BoardCardDto, ColumnKind, ColumnResolution, CreateBoardCardRequest, UpdateBoardCardRequest,
};
use aionui_common::{generate_id, now_ms};
use aionui_db::{BoardCardRow, BoardColumnRow, BoardPlacement, BoardPlacementOutcome, IBoardTx};
use serde_json::{Value, json};
use tracing::Instrument;

use crate::actor::BoardActor;
use crate::convert::{agent_card_ids, card_creators, card_dto, dependency_dto, reference, set_creator};
use crate::error::{BoardError, BoardErrorCode, BoardResult};
use crate::events::{
    CARD_ARCHIVED, CARD_BLOCKED, CARD_CREATED, CARD_MOVED, CARD_RESTORED, CARD_UNBLOCKED, CARD_UPDATED, DEPLOY_FORCED,
    GATE_CROSSED, GATE_FORCED,
};
use crate::rate_limit::WriteBudget;
use crate::rules::{exit_rule_missing, owner_must_decide};
use crate::service::{BoardService, Committed, completed_columns, require_owner, version_conflict, write_span};
use crate::templates::{DEPLOYED, DEPLOYING, LOCAL_TEST, READY_TO_DEPLOY};
use crate::validation::{
    BLOCK_REASON_MAX, CHANGE_ID_MAX, DESCRIPTION_MAX, METADATA_MAX_BYTES, ROUTING_MAX, TAG_LIST_MAX, TITLE_MAX,
    free_text, json_object, required_text, tag_list,
};

/// A move request in service terms (both routers map onto it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveCard {
    pub to_column: String,
    pub before_id: Option<String>,
    pub after_id: Option<String>,
    /// `None` = no placement guard (agent router default).
    pub expected_column: Option<String>,
    pub force: bool,
    /// Required when forcing a card out of a gate or `deploying`.
    pub reason: Option<String>,
    /// Agent moves across a gate under `agent_access: approval` (TASK-116):
    /// how the owner approved it, recorded as `{by: "owner", via, at}`.
    pub approval_note: Option<String>,
}

pub(crate) fn card_archived() -> BoardError {
    BoardError::new(BoardErrorCode::Invalid, "the card is archived; restore it first")
        .with_details(json!({ "field": "card", "reason": "archived" }))
}

pub(crate) fn card_version_conflict(card: &BoardCardRow) -> BoardError {
    version_conflict(card.version).with_details(json!({
        "current_version": card.version,
        "current_column": card.column_key,
    }))
}

fn json_list(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".into())
}

fn optional_text(field: &str, value: Option<&str>, max: usize) -> BoardResult<Option<String>> {
    match value.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => required_text(field, text, max).map(Some),
        None => Ok(None),
    }
}

pub(crate) fn is_gate(column: &BoardColumnRow) -> bool {
    column.kind == ColumnKind::Gate.as_str()
}

pub(crate) fn gate_rule(column: &BoardColumnRow) -> Value {
    column
        .gate
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_else(|| json!({}))
}

pub(crate) fn exit_rule(column: &BoardColumnRow) -> Option<Value> {
    column
        .exit_rule
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
}

/// Field changes of a PATCH, validated before the transaction opens.
struct CardPatch {
    request: UpdateBoardCardRequest,
    title: Option<String>,
    description: Option<String>,
    labels: Option<String>,
    skills: Option<String>,
    routing: Option<String>,
    metadata: Option<String>,
    change_id: Option<Option<String>>,
    blocked_reason: Option<Option<String>>,
}

impl CardPatch {
    fn validate(request: UpdateBoardCardRequest) -> BoardResult<Self> {
        let list = |field: &str, values: &Option<Vec<String>>, max: usize| {
            values
                .as_deref()
                .map(|l| tag_list(field, l, max).map(|l| json_list(&l)))
                .transpose()
        };
        Ok(Self {
            title: request
                .title
                .as_deref()
                .map(|t| required_text("title", t, TITLE_MAX))
                .transpose()?,
            description: request
                .description
                .as_deref()
                .map(|d| free_text("description", d, DESCRIPTION_MAX))
                .transpose()?,
            labels: list("labels", &request.labels, TAG_LIST_MAX)?,
            skills: list("skills", &request.skills, TAG_LIST_MAX)?,
            routing: list("routing", &request.routing, ROUTING_MAX)?,
            metadata: request
                .metadata
                .as_ref()
                .map(|m| json_object("metadata", m, METADATA_MAX_BYTES).map(|m| m.to_string()))
                .transpose()?,
            change_id: request
                .change_id
                .as_ref()
                .map(|value| optional_text("change_id", value.as_deref(), CHANGE_ID_MAX))
                .transpose()?,
            blocked_reason: request
                .blocked_reason
                .as_ref()
                .map(|value| optional_text("blocked_reason", value.as_deref(), BLOCK_REASON_MAX))
                .transpose()?,
            request,
        })
    }

    /// Apply to `card`; returns the names of the fields that changed.
    fn apply(self, card: &mut BoardCardRow) -> Vec<&'static str> {
        let mut changed = Vec::new();
        let mut set = |name: &'static str, slot: &mut String, value: Option<String>| {
            if let Some(value) = value
                && *slot != value
            {
                *slot = value;
                changed.push(name);
            }
        };
        set("title", &mut card.title, self.title);
        set("description", &mut card.description, self.description);
        set(
            "type",
            &mut card.card_type,
            self.request.card_type.map(|t| t.as_str().to_owned()),
        );
        set(
            "priority",
            &mut card.priority,
            self.request.priority.map(|p| p.as_str().to_owned()),
        );
        set("labels", &mut card.labels, self.labels);
        set("skills", &mut card.skills, self.skills);
        set("routing", &mut card.routing, self.routing);
        set("metadata", &mut card.metadata, self.metadata);
        let mut set_opt = |name: &'static str, slot: &mut Option<String>, value: Option<Option<String>>| {
            if let Some(value) = value
                && *slot != value
            {
                *slot = value;
                changed.push(name);
            }
        };
        set_opt(
            "size",
            &mut card.size,
            self.request.size.map(|s| s.map(|s| s.as_str().to_owned())),
        );
        set_opt(
            "tier",
            &mut card.tier,
            self.request.tier.map(|t| t.map(|t| t.as_str().to_owned())),
        );
        set_opt("change_id", &mut card.change_id, self.change_id);
        set_opt("blocked_reason", &mut card.blocked_reason, self.blocked_reason);
        // One assignee at a time: a conversation replaces an assistant/team.
        if let Some(conversation) = self.request.assignee_conversation.clone()
            && card.assignee_conversation_id.as_deref() != Some(conversation.as_str())
        {
            card.assignee_conversation_id = Some(conversation);
            card.assignee_kind = None;
            card.assignee_ref = None;
            changed.push("assignee");
        }
        if let Some(blocked) = self.request.blocked
            && card.blocked != blocked
        {
            card.blocked = blocked;
            if !blocked {
                card.blocked_reason = None;
            }
            changed.push("blocked");
        }
        changed
    }
}

impl BoardService {
    /// Load a card by id for `actor` (user-scoped, R7b) and enforce the agent
    /// project scope (R7). Unknown or foreign ids are 404.
    pub(crate) async fn scoped_card(&self, actor: &BoardActor, card_id: &str) -> BoardResult<BoardCardRow> {
        let card = self
            .store()
            .card(actor.user_id(), card_id)
            .await?
            .ok_or_else(|| BoardError::not_found("card"))?;
        if let Some(agent) = actor.as_agent()
            && agent.project_id != card.project_id
        {
            return Err(BoardError::new(
                BoardErrorCode::WrongProject,
                "agents may only change cards of their own project",
            ));
        }
        Ok(card)
    }

    /// Re-read the card inside the transaction and run the board-state checks.
    pub(crate) async fn tx_card(tx: &mut dyn IBoardTx, actor: &BoardActor, card_id: &str) -> BoardResult<BoardCardRow> {
        let card = tx
            .card(actor.user_id(), card_id)
            .await?
            .ok_or_else(|| BoardError::not_found("card"))?;
        let settings = Self::tx_settings(tx, actor, &card.project_id).await?;
        Self::check_writable(&settings, actor)?;
        Ok(card)
    }

    /// The full card (with resolved dependencies) as read inside `tx`.
    pub(crate) async fn tx_card_dto(tx: &mut dyn IBoardTx, card: &BoardCardRow) -> BoardResult<BoardCardDto> {
        let columns = tx.columns(&card.user_id, &card.project_id).await?;
        let ids: Vec<String> = tx
            .dependencies(&card.user_id, &card.id)
            .await?
            .into_iter()
            .map(|dep| dep.depends_on_card_id)
            .collect();
        let targets = tx.cards_by_ids(&card.user_id, &ids).await?;
        let mut dto = card_dto(card, targets.iter().map(|t| dependency_dto(t, &columns)).collect());
        let creators = card_creators(tx.card_creators(&card.user_id, &agent_card_ids([card])).await?);
        set_creator(
            &card.id,
            &mut dto.creator_conversation_id,
            &mut dto.creator_name,
            &creators,
        );
        Ok(dto)
    }

    /// The full card as read outside a transaction.
    pub(crate) async fn card_dto_of(&self, card: &BoardCardRow) -> BoardResult<BoardCardDto> {
        let store = self.store();
        let columns = store.columns(&card.user_id, &card.project_id).await?;
        let ids: Vec<String> = store
            .dependencies(&card.user_id, &card.id)
            .await?
            .into_iter()
            .map(|dep| dep.depends_on_card_id)
            .collect();
        let targets = store.cards_by_ids(&card.user_id, &ids).await?;
        let mut dto = card_dto(card, targets.iter().map(|t| dependency_dto(t, &columns)).collect());
        let creators = card_creators(store.card_creators(&card.user_id, &agent_card_ids([card])).await?);
        set_creator(
            &card.id,
            &mut dto.creator_conversation_id,
            &mut dto.creator_name,
            &creators,
        );
        Ok(dto)
    }

    /// Write a changed card, append its single event, commit, broadcast.
    pub(crate) async fn write_card(
        &self,
        mut tx: Box<dyn IBoardTx>,
        actor: &BoardActor,
        card: BoardCardRow,
        expected_version: i64,
        kind: &str,
        extra: Value,
    ) -> BoardResult<BoardCardDto> {
        if !tx.update_card(&card, expected_version).await? {
            return Err(card_version_conflict(&card));
        }
        let dto = Self::tx_card_dto(tx.as_mut(), &card).await?;
        let mut payload = json!({ "card": dto, "version": card.version });
        if let (Some(target), Value::Object(fields)) = (payload.as_object_mut(), extra) {
            target.extend(fields);
        }
        add_reordered(&mut payload, tx.as_mut());
        let event = Self::append_event(tx.as_mut(), actor, &card.project_id, Some(&card.id), kind, payload).await?;
        self.commit(tx, actor.user_id(), &Committed::card(event, dto.clone()))
            .await?;
        Ok(dto)
    }

    /// `POST /api/projects/{pid}/board/cards` and the agent `create`. Cards
    /// land at the tail of the first active column unless `column` names
    /// another (agents may only name columns open to them).
    pub async fn create_card(
        &self,
        actor: &BoardActor,
        project_id: &str,
        request: CreateBoardCardRequest,
    ) -> BoardResult<BoardCardDto> {
        if let Some(agent) = actor.as_agent()
            && agent.project_id != project_id
        {
            return Err(BoardError::new(
                BoardErrorCode::WrongProject,
                "agents may only create cards in their own project",
            ));
        }
        self.charge(actor, WriteBudget::Create)?;
        let title = required_text("title", &request.title, TITLE_MAX)?;
        let description = free_text(
            "description",
            request.description.as_deref().unwrap_or(""),
            DESCRIPTION_MAX,
        )?;
        let labels = tag_list("labels", request.labels.as_deref().unwrap_or(&[]), TAG_LIST_MAX)?;
        let skills = tag_list("skills", request.skills.as_deref().unwrap_or(&[]), TAG_LIST_MAX)?;
        let routing = tag_list("routing", request.routing.as_deref().unwrap_or(&[]), ROUTING_MAX)?;
        let metadata = match &request.metadata {
            Some(value) => json_object("metadata", value, METADATA_MAX_BYTES)?,
            None => json!({}),
        };
        let change_id = optional_text("change_id", request.change_id.as_deref(), CHANGE_ID_MAX)?;
        if actor.as_agent().is_none() {
            self.user_project(actor.user_id(), project_id).await?;
        }
        self.refresh_archive(actor, project_id).await?;
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let settings = Self::tx_settings(tx.as_mut(), actor, project_id).await?;
            Self::check_writable(&settings, actor)?;
            let columns = tx.columns(user_id, project_id).await?;
            let active: Vec<&BoardColumnRow> = columns.iter().filter(|c| !c.archived).collect();
            let column = match request.column.as_deref() {
                None => *active
                    .first()
                    .ok_or_else(|| BoardError::invalid("column", "the board has no active column"))?,
                Some(key) => {
                    let column = active
                        .iter()
                        .find(|c| c.key == key)
                        .copied()
                        .ok_or_else(|| BoardError::invalid("column", format!("unknown column `{key}`")))?;
                    // Agents create in the first active column. Naming another
                    // one counts as moving there: only a column open to agents
                    // (the Board Keeper: any non-gate column).
                    if let Some(agent) = actor.as_agent() {
                        let first = active.first().map_or("", |c| c.key.as_str());
                        let open = column.key == first
                            || (column.key != DEPLOYING
                                && !is_gate(column)
                                && (column.agent_move == AgentMove::Allow.as_str() || agent.keeper));
                        if !open {
                            return Err(agent_create_denied(&column.key, first));
                        }
                    }
                    if column.key == DEPLOYING || (is_gate(column) && !actor.is_user()) {
                        return Err(gate_use_decision(&column.key));
                    }
                    check_barrier(actor, &columns, None, column, false)?;
                    column
                }
            };
            if let Some(limit) = column.wip_limit {
                let count = tx.count_column_cards(user_id, project_id, &column.key, None).await?;
                if count >= limit {
                    return Err(wip_limit(&column.key, limit, count));
                }
            }
            let number = tx
                .claim_number(user_id, project_id)
                .await?
                .ok_or_else(|| BoardError::not_found("board"))?;
            let BoardPlacementOutcome::Placed(position) = tx
                .place(user_id, project_id, &column.key, None, BoardPlacement::Tail)
                .await?
            else {
                return Err(BoardError::Internal("tail placement reported a missing anchor".into()));
            };
            let now = now_ms();
            let card = BoardCardRow {
                id: generate_id(),
                project_id: project_id.to_owned(),
                user_id: user_id.to_owned(),
                number,
                title,
                description,
                card_type: request
                    .card_type
                    .unwrap_or(aionui_api_types::CardType::Feature)
                    .as_str()
                    .to_owned(),
                priority: request
                    .priority
                    .unwrap_or(aionui_api_types::CardPriority::Medium)
                    .as_str()
                    .to_owned(),
                size: request.size.map(|s| s.as_str().to_owned()),
                tier: request.tier.map(|t| t.as_str().to_owned()),
                column_key: column.key.clone(),
                position,
                labels: json_list(&labels),
                skills: json_list(&skills),
                routing: json_list(&routing),
                creator_kind: actor.kind().as_str().to_owned(),
                creator_ref: actor.actor_ref().to_owned(),
                assignee_kind: None,
                assignee_ref: None,
                assignee_conversation_id: None,
                blocked: false,
                blocked_reason: None,
                blocked_by: None,
                change_id,
                metadata: metadata.to_string(),
                version: 1,
                archived_at: None,
                created_at: now,
                updated_at: now,
            };
            tx.insert_card(&card).await?;
            let mut dto = card_dto(&card, Vec::new());
            let mut payload = json!({ "card": dto, "version": card.version });
            add_reordered(&mut payload, tx.as_mut());
            let event =
                Self::append_event(tx.as_mut(), actor, project_id, Some(&card.id), CARD_CREATED, payload).await?;
            // The creator conversation is read back from the event just
            // appended (the payload keeps the card as stored).
            let creators = card_creators(tx.card_creators(user_id, &agent_card_ids([&card])).await?);
            set_creator(
                &card.id,
                &mut dto.creator_conversation_id,
                &mut dto.creator_name,
                &creators,
            );
            self.commit(tx, user_id, &Committed::card(event, dto.clone())).await?;
            Ok(dto)
        }
        .instrument(write_span(CARD_CREATED, actor, None))
        .await
    }

    /// `PATCH /api/board/cards/{id}` (also block/unblock for the owner) and
    /// the agent `update`.
    pub async fn update_card(
        &self,
        actor: &BoardActor,
        card_id: &str,
        request: UpdateBoardCardRequest,
    ) -> BoardResult<BoardCardDto> {
        self.charge(actor, WriteBudget::Write)?;
        let version = request.version;
        if let Some(conversation) = request.assignee_conversation.as_deref() {
            self.check_assignee_conversation(actor.user_id(), conversation).await?;
        }
        let patch = CardPatch::validate(request)?;
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        async {
            let mut tx = self.store().begin().await?;
            let mut card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            if card.archived_at.is_some() {
                return Err(card_archived());
            }
            if card.version != version {
                return Err(card_version_conflict(&card));
            }
            let changed = patch.apply(&mut card);
            // The fields owner_if reads cannot be changed by agents while the
            // card waits for a decision (allowed again once it left the gate).
            if actor.as_agent().is_some()
                && changed
                    .iter()
                    .any(|field| matches!(*field, "size" | "tier" | "routing"))
            {
                let columns = tx.columns(actor.user_id(), &card.project_id).await?;
                if columns.iter().any(|c| c.key == card.column_key && is_gate(c)) {
                    return Err(gate_use_decision(&card.column_key)
                        .with_details(json!({ "column": card.column_key, "fields": ["size", "tier", "routing"] })));
                }
            }
            if changed.contains(&"blocked") {
                card.blocked_by = card.blocked.then(|| actor.kind().as_str().to_owned());
            }
            if card.blocked && card.blocked_reason.is_none() && changed.contains(&"blocked") {
                return Err(BoardError::invalid("blocked_reason", "blocking a card needs a reason"));
            }
            card.version += 1;
            card.updated_at = now_ms();
            self.write_card(tx, actor, card, version, CARD_UPDATED, json!({ "changed": changed }))
                .await
        }
        .instrument(write_span(CARD_UPDATED, actor, Some(card_id)))
        .await
    }

    /// A conversation assignee must be one of the user's live conversations.
    async fn check_assignee_conversation(&self, user_id: &str, conversation_id: &str) -> BoardResult<()> {
        let conversation_id = required_text("assignee_conversation", conversation_id, 200)?;
        match self.store().conversation_info(user_id, &conversation_id).await? {
            Some(info) if !info.archived => Ok(()),
            Some(_) => Err(BoardError::invalid(
                "assignee_conversation",
                "the conversation is archived",
            )),
            None => Err(BoardError::invalid(
                "assignee_conversation",
                format!("no conversation `{conversation_id}`"),
            )),
        }
    }

    /// `POST /api/board/cards/{id}/move` and the agent `move`.
    pub async fn move_card(&self, actor: &BoardActor, card_id: &str, request: MoveCard) -> BoardResult<BoardCardDto> {
        if request.before_id.is_some() && request.after_id.is_some() {
            return Err(BoardError::invalid("before_id", "pass before_id or after_id, not both"));
        }
        if request.force {
            match actor {
                BoardActor::User { .. } => {}
                BoardActor::System { .. } => {
                    return Err(BoardError::new(
                        BoardErrorCode::SystemForbidden,
                        "the supervisor may not force a move",
                    ));
                }
                BoardActor::Agent(_) => return Err(BoardError::invalid("force", "agents cannot force a move")),
            }
        }
        if [&request.before_id, &request.after_id]
            .into_iter()
            .flatten()
            .any(|anchor| anchor == card_id)
        {
            return Err(BoardError::invalid(
                "before_id",
                "a card cannot be placed next to itself",
            ));
        }
        let reason = optional_text("reason", request.reason.as_deref(), BLOCK_REASON_MAX)?;
        let approval_note = optional_text(
            "approval_note",
            request.approval_note.as_deref(),
            crate::access::APPROVAL_NOTE_MAX,
        )?;
        self.charge(actor, WriteBudget::Write)?;
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        // TASK-116: how far this agent may move across gates.
        let access = match actor.as_agent() {
            Some(_) => Some(self.agent_access(actor.user_id(), &card.project_id).await?),
            None => None,
        };
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let mut card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            if card.archived_at.is_some() {
                return Err(card_archived());
            }
            if let Some(expected) = &request.expected_column
                && *expected != card.column_key
            {
                return Err(card_version_conflict(&card));
            }
            let columns = tx.columns(user_id, &card.project_id).await?;
            let by_key: HashMap<&str, &BoardColumnRow> = columns.iter().map(|c| (c.key.as_str(), c)).collect();
            let to = by_key
                .get(request.to_column.as_str())
                .filter(|c| !c.archived)
                .copied()
                .ok_or_else(|| BoardError::invalid("to_column", format!("unknown column `{}`", request.to_column)))?;
            let from = by_key.get(card.column_key.as_str()).copied();
            let changes_column = to.key != card.column_key;
            // An agent touching a gate (leaving, entering or passing it) with
            // the board's access: the gate refusals below are lifted and the
            // move is recorded as `gate_crossed`. Without it the old rules
            // stand; under `approval` their refusal says a note is needed.
            let touched = access
                .filter(|_| changes_column)
                .and_then(|access| touched_gate(&columns, from, to).map(|gate| (access, gate.key.clone())));
            let crossing = touched
                .as_ref()
                .filter(|(access, _)| crate::access::permits(*access, approval_note.as_deref()))
                .map(|(access, gate)| (*access, gate.clone()));
            let gates_open = crossing.is_some();
            let refused = |error: BoardError| match &touched {
                Some((AgentAccess::Approval, gate)) if !gates_open => agent_move_denied(gate).with_details(json!({
                    "column": to.key,
                    "gate": gate,
                    "approval_required": true,
                })),
                _ => error,
            };
            check_agent_move(actor, from, to, gates_open).map_err(refused)?;
            let mut kind = CARD_MOVED;
            let mut overrides: Vec<&str> = Vec::new();
            if changes_column {
                // `deploying` is entered only through the Deploy approval of
                // a deploy job (S5), never by a plain move. The supervisor
                // leaves it only to `deployed` or `local_test`; the owner may
                // force a card out with a reason.
                if to.key == DEPLOYING {
                    return Err(gate_use_decision(DEPLOYING));
                }
                if card.column_key == DEPLOYING {
                    if matches!(actor, BoardActor::System { .. }) {
                        if to.key != DEPLOYED && to.key != LOCAL_TEST {
                            return Err(gate_use_decision(DEPLOYING));
                        }
                    } else {
                        require_forced_reason(actor, &request, &reason, DEPLOYING)?;
                        kind = DEPLOY_FORCED;
                        overrides.push("deploying");
                    }
                } else if let Some(from) = from.filter(|from| is_gate(from)) {
                    let reviews = tx.reviews(user_id, &card.id).await?;
                    let questions = tx.questions(user_id, &card.id).await?;
                    if owner_must_decide(&gate_rule(from), &card, &reviews, &questions) && !gates_open {
                        require_forced_reason(actor, &request, &reason, &from.key).map_err(refused)?;
                        kind = GATE_FORCED;
                        overrides.push("gate");
                    }
                }
            }
            if card.column_key != DEPLOYING {
                check_barrier(actor, &columns, from, to, gates_open).map_err(refused)?;
            }
            if gates_open {
                kind = GATE_CROSSED;
            }
            let forward = changes_column && from.is_some_and(|from| to.position > from.position);
            if forward {
                let blockers = self.blockers(tx.as_mut(), &card, &columns).await?;
                if !blockers.is_clear() {
                    if !request.force {
                        return Err(BoardError::new(
                            BoardErrorCode::Blocked,
                            "the card is blocked and cannot move forward",
                        )
                        .with_details(blockers.details()));
                    }
                    overrides.push("blocked");
                }
                if let Some(rule) = from.and_then(exit_rule) {
                    let reviews = tx.reviews(user_id, &card.id).await?;
                    let questions = tx.questions(user_id, &card.id).await?;
                    let missing = exit_rule_missing(&rule, &card, &reviews, &questions);
                    if !missing.is_empty() {
                        if !request.force {
                            return Err(BoardError::new(
                                BoardErrorCode::ExitRule,
                                format!("`{}` exit rule is not met", card.column_key),
                            )
                            .with_details(json!({ "missing": missing })));
                        }
                        overrides.push("exit_rule");
                    }
                }
            }
            if changes_column && let Some(limit) = to.wip_limit {
                let count = tx
                    .count_column_cards(user_id, &card.project_id, &to.key, Some(&card.id))
                    .await?;
                if count >= limit {
                    if !request.force {
                        return Err(wip_limit(&to.key, limit, count));
                    }
                    overrides.push("wip_limit");
                }
            }
            let placement = match (&request.before_id, &request.after_id) {
                (Some(before), _) => BoardPlacement::Before(before),
                (_, Some(after)) => BoardPlacement::After(after),
                _ => BoardPlacement::Tail,
            };
            let position = match tx
                .place(user_id, &card.project_id, &to.key, Some(&card.id), placement)
                .await?
            {
                BoardPlacementOutcome::Placed(position) => position,
                BoardPlacementOutcome::AnchorNotFound => {
                    let field = if request.before_id.is_some() {
                        "before_id"
                    } else {
                        "after_id"
                    };
                    return Err(BoardError::invalid(
                        field,
                        "the anchor card is not in the target column",
                    ));
                }
            };
            let dropped_dependents =
                if changes_column && to.resolution.as_deref() == Some(ColumnResolution::Dropped.as_str()) {
                    let ids: Vec<String> = tx
                        .dependents(user_id, &card.id)
                        .await?
                        .into_iter()
                        .map(|dep| dep.card_id)
                        .collect();
                    tx.cards_by_ids(user_id, &ids)
                        .await?
                        .into_iter()
                        .map(|dependent| json!({ "card_id": dependent.id, "number": dependent.number }))
                        .collect()
                } else {
                    Vec::new()
                };
            if kind == DEPLOY_FORCED
                && let Some(job) = tx.active_deploy_job(user_id, &card.id).await?
            {
                // The override is recorded on the job; the supervisor keeps
                // watching it but never moves this card again (R3).
                let mut detail: Value = serde_json::from_str(&job.detail).unwrap_or_else(|_| json!({}));
                detail["owner_override"] = json!({ "at": now_ms(), "to": to.key, "reason": reason });
                tx.update_deploy_job_detail(user_id, &job.id, &detail.to_string(), now_ms())
                    .await?;
            }
            let from_key = std::mem::replace(&mut card.column_key, to.key.clone());
            card.position = position;
            let expected = card.version;
            card.version += 1;
            card.updated_at = now_ms();
            let mut extra = json!({ "from": from_key, "to": to.key, "forced": !overrides.is_empty() });
            if !overrides.is_empty() {
                extra["overrides"] = json!(overrides);
            }
            if reason.is_some() {
                extra["reason"] = json!(reason);
            }
            if !dropped_dependents.is_empty() {
                // R5: dependents keep their block; the drop is recorded on
                // this move's single event.
                extra["blocker_dropped"] = json!(dropped_dependents);
            }
            if let Some((access, gate)) = &crossing {
                extra["gate"] = json!(gate);
                extra["agent_access"] = json!(access.as_str());
                extra["approval"] = match &approval_note {
                    Some(note) => json!({ "by": "owner", "via": note, "at": now_ms() }),
                    None => Value::Null,
                };
                tracing::info!(card_id = %card.id, gate = %gate, agent_access = access.as_str(),
                    approved = approval_note.is_some(), "board: agent moved a card across a gate");
            }
            self.write_card(tx, actor, card, expected, kind, extra).await
        }
        .instrument(write_span(CARD_MOVED, actor, Some(card_id)))
        .await
    }

    /// Manual block / unblock (agent `block`/`unblock`; the owner uses PATCH).
    pub async fn set_blocked(
        &self,
        actor: &BoardActor,
        card_id: &str,
        reason: Option<&str>,
    ) -> BoardResult<BoardCardDto> {
        self.charge(actor, WriteBudget::Write)?;
        let reason = match reason {
            Some(reason) => Some(required_text("reason", reason, BLOCK_REASON_MAX)?),
            None => None,
        };
        let kind = if reason.is_some() { CARD_BLOCKED } else { CARD_UNBLOCKED };
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        async {
            let mut tx = self.store().begin().await?;
            let mut card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            if card.archived_at.is_some() {
                return Err(card_archived());
            }
            if reason.is_none() && !card.blocked {
                // Already unblocked: nothing changes, nothing is written.
                return Self::tx_card_dto(tx.as_mut(), &card).await;
            }
            if actor.as_agent().is_some() && card.blocked && card.blocked_by.as_deref() == Some("user") {
                return Err(BoardError::new(
                    BoardErrorCode::AgentMoveDenied,
                    "the owner set this block; only the owner lifts or changes it",
                )
                .with_details(json!({ "reason": "owner_block" })));
            }
            card.blocked = reason.is_some();
            card.blocked_reason = reason.clone();
            card.blocked_by = reason.is_some().then(|| actor.kind().as_str().to_owned());
            let expected = card.version;
            card.version += 1;
            card.updated_at = now_ms();
            self.write_card(tx, actor, card, expected, kind, json!({})).await
        }
        .instrument(write_span(kind, actor, Some(card_id)))
        .await
    }

    /// `POST /api/board/cards/{id}/archive` · `/restore` (owner only). A
    /// no-op (already in that state) writes nothing.
    pub async fn set_card_archived(
        &self,
        actor: &BoardActor,
        card_id: &str,
        archived: bool,
    ) -> BoardResult<BoardCardDto> {
        require_owner(actor)?;
        let kind = if archived { CARD_ARCHIVED } else { CARD_RESTORED };
        let card = self.scoped_card(actor, card_id).await?;
        self.refresh_archive(actor, &card.project_id).await?;
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let mut card = Self::tx_card(tx.as_mut(), actor, card_id).await?;
            if card.archived_at.is_some() == archived {
                return Self::tx_card_dto(tx.as_mut(), &card).await;
            }
            if archived {
                card.archived_at = Some(now_ms());
            } else {
                // Restore lands at the tail of its column, or of the first
                // active column when its own was archived meanwhile.
                let columns = tx.columns(user_id, &card.project_id).await?;
                let column = columns
                    .iter()
                    .find(|c| c.key == card.column_key && !c.archived)
                    .or_else(|| columns.iter().find(|c| !c.archived))
                    .ok_or_else(|| BoardError::invalid("column", "the board has no active column"))?
                    .key
                    .clone();
                let BoardPlacementOutcome::Placed(position) = tx
                    .place(user_id, &card.project_id, &column, Some(&card.id), BoardPlacement::Tail)
                    .await?
                else {
                    return Err(BoardError::Internal("tail placement reported a missing anchor".into()));
                };
                card.column_key = column;
                card.position = position;
                card.archived_at = None;
            }
            let expected = card.version;
            card.version += 1;
            card.updated_at = now_ms();
            self.write_card(tx, actor, card, expected, kind, json!({})).await
        }
        .instrument(write_span(kind, actor, Some(card_id)))
        .await
    }

    /// R5 blockers of `card`: the manual flag, unresolved dependencies, and
    /// dependencies whose target was dropped (they keep the block).
    pub(crate) async fn blockers(
        &self,
        tx: &mut dyn IBoardTx,
        card: &BoardCardRow,
        columns: &[BoardColumnRow],
    ) -> BoardResult<Blockers> {
        let deps = tx.dependencies(&card.user_id, &card.id).await?;
        let ids: Vec<String> = deps.into_iter().map(|dep| dep.depends_on_card_id).collect();
        let targets = tx.cards_by_ids(&card.user_id, &ids).await?;
        let completed = completed_columns(columns);
        let dropped: Vec<&str> = columns
            .iter()
            .filter(|c| c.resolution.as_deref() == Some(ColumnResolution::Dropped.as_str()))
            .map(|c| c.key.as_str())
            .collect();
        let mut blockers = Blockers {
            manual: card.blocked.then(|| card.blocked_reason.clone().unwrap_or_default()),
            ..Default::default()
        };
        for target in &targets {
            if completed.contains(target.column_key.as_str()) {
                continue;
            }
            if dropped.contains(&target.column_key.as_str()) {
                blockers.dropped.push(reference(target.number));
            } else {
                blockers.unresolved.push(reference(target.number));
            }
        }
        Ok(blockers)
    }
}

/// A rebalance renumbered the column: record `(card_id, position)` of every
/// card it moved, so replaying the events keeps the column order.
pub(crate) fn add_reordered(payload: &mut Value, tx: &mut dyn IBoardTx) {
    let reordered = tx.take_rebalanced();
    if !reordered.is_empty() {
        payload["reordered"] = json!(
            reordered
                .into_iter()
                .map(|(card_id, position)| json!({ "card_id": card_id, "position": position }))
                .collect::<Vec<_>>()
        );
    }
}

/// Agents move only through columns open to them; the Board Keeper may move
/// between any two non-gate columns (D40). `deploying` stays closed to both.
/// `gates_open`: the board's `agent_access` lets this agent through gates
/// (TASK-116); `deploying` stays closed regardless.
fn check_agent_move(
    actor: &BoardActor,
    from: Option<&BoardColumnRow>,
    to: &BoardColumnRow,
    gates_open: bool,
) -> BoardResult<()> {
    let Some(agent) = actor.as_agent() else {
        return Ok(());
    };
    let columns = std::iter::once(to).chain(from);
    for column in columns {
        let open = if gates_open && is_gate(column) {
            true
        } else if agent.keeper {
            !is_gate(column) && column.key != DEPLOYING
        } else {
            column.agent_move == AgentMove::Allow.as_str()
        };
        if !open {
            return Err(agent_move_denied(&column.key));
        }
    }
    Ok(())
}

/// Every actor but the owner stays in front of the next gate: a move (or a
/// create, which starts before the first column) may land ON the first gate or
/// `deploying` column after its source, never beyond it. The keeper also never
/// reaches the terminal columns after Ready to deploy.
fn check_barrier(
    actor: &BoardActor,
    columns: &[BoardColumnRow],
    from: Option<&BoardColumnRow>,
    to: &BoardColumnRow,
    gates_open: bool,
) -> BoardResult<()> {
    if actor.is_user() {
        return Ok(());
    }
    let from_position = from.map(|column| column.position).unwrap_or(i64::MIN);
    if to.position <= from_position {
        return Ok(());
    }
    let barrier = columns
        .iter()
        .filter(|c| !c.archived && c.position > from_position && ((is_gate(c) && !gates_open) || c.key == DEPLOYING))
        .min_by_key(|c| c.position);
    let blocked_by_barrier = barrier.is_some_and(|barrier| to.position > barrier.position);
    let keeper_terminal = actor.as_agent().is_some_and(|agent| agent.keeper)
        && to.kind == ColumnKind::Terminal.as_str()
        && columns
            .iter()
            .find(|c| c.key == READY_TO_DEPLOY)
            .is_some_and(|ready| to.position > ready.position);
    if !(blocked_by_barrier || keeper_terminal) {
        return Ok(());
    }
    let gate = barrier.map(|b| b.key.as_str()).unwrap_or(READY_TO_DEPLOY);
    Err(match actor {
        BoardActor::Agent(_) => agent_move_denied(&to.key).with_details(json!({ "column": to.key, "gate": gate })),
        _ => gate_use_decision(gate),
    })
}

/// The gate an agent's move touches (TASK-116): the gate it leaves, else the
/// gate it enters, else the first gate it passes moving forward.
fn touched_gate<'a>(
    columns: &'a [BoardColumnRow],
    from: Option<&'a BoardColumnRow>,
    to: &'a BoardColumnRow,
) -> Option<&'a BoardColumnRow> {
    if let Some(from) = from.filter(|c| is_gate(c)) {
        return Some(from);
    }
    if is_gate(to) {
        return Some(to);
    }
    let from_position = from.map(|c| c.position).unwrap_or(i64::MIN);
    if to.position <= from_position {
        return None;
    }
    columns
        .iter()
        .filter(|c| !c.archived && is_gate(c) && c.position > from_position && c.position < to.position)
        .min_by_key(|c| c.position)
}

/// Leaving a decided gate or `deploying` needs the owner's force with a
/// non-empty reason.
fn require_forced_reason(
    actor: &BoardActor,
    request: &MoveCard,
    reason: &Option<String>,
    column: &str,
) -> BoardResult<()> {
    if !(actor.is_user() && request.force) {
        return Err(gate_use_decision(column));
    }
    if reason.is_none() {
        return Err(BoardError::invalid(
            "reason",
            "forcing a card out of a gate needs a reason",
        ));
    }
    Ok(())
}

/// Why a card counts as blocked (R5).
#[derive(Debug, Default)]
pub(crate) struct Blockers {
    pub(crate) manual: Option<String>,
    pub(crate) unresolved: Vec<String>,
    pub(crate) dropped: Vec<String>,
}

impl Blockers {
    pub(crate) fn is_clear(&self) -> bool {
        self.manual.is_none() && self.unresolved.is_empty() && self.dropped.is_empty()
    }

    pub(crate) fn details(&self) -> Value {
        json!({
            "blocked_reason": self.manual,
            "unresolved_dependencies": self.unresolved,
            "dropped_dependencies": self.dropped,
        })
    }
}

pub(crate) fn gate_use_decision(column: &str) -> BoardError {
    BoardError::new(
        BoardErrorCode::GateUseDecision,
        format!("cards leave `{column}` through the owner's decision"),
    )
    .with_details(json!({ "column": column }))
}

pub(crate) fn agent_move_denied(column: &str) -> BoardError {
    BoardError::new(
        BoardErrorCode::AgentMoveDenied,
        format!("agents may not move cards into or out of `{column}`"),
    )
    .with_details(json!({ "column": column }))
}

/// An agent named a create column other than the first one (and not open
/// to it). Same code as a denied move; the message states the create rule.
pub(crate) fn agent_create_denied(column: &str, first_column: &str) -> BoardError {
    BoardError::new(
        BoardErrorCode::AgentMoveDenied,
        format!(
            "agents can only create cards in the first column `{first_column}`: leave out `column` \
             (`{column}` is not open to agents; ask the owner or the Board Keeper to move the card)"
        ),
    )
    .with_details(json!({ "column": column, "first_column": first_column }))
}

pub(crate) fn wip_limit(column: &str, limit: i64, count: i64) -> BoardError {
    BoardError::new(
        BoardErrorCode::WipLimit,
        format!("column `{column}` is full ({count}/{limit})"),
    )
    .with_details(json!({ "column": column, "limit": limit, "count": count }))
}
