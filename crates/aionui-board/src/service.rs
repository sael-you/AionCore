//! Board business rules (SPEC §4) — board-level operations: view, settings,
//! columns, Board Keeper, project archive/delete ports. Card operations live
//! in `service_cards.rs`, gates in `service_gate.rs`, reviews/links in
//! `service_reviews.rs`, assign in `service_assign.rs`, reads in
//! `service_relations.rs`, agent caller resolution in `service_agent.rs`.
//!
//! Every write is one `BEGIN IMMEDIATE` transaction that runs its checks,
//! writes exactly one `board_events` row and commits; the `board.changed`
//! broadcast follows the commit (R8). Each write runs in a `board_write`
//! tracing span carrying `card_id`, `actor_kind`, `actor_ref`, `kind` (R10).
//! Host calls (conversation create/delete, messages) never run inside a
//! transaction.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use aionui_api_types::{
    AgentMove, BoardCardDto, BoardColumnInput, BoardColumnsResponse, BoardEventDto, BoardKeeperResponse,
    BoardReviewDto, BoardSettingsDto, BoardTemplate, BoardView, ColumnKind, ColumnResolution,
    UpdateBoardColumnsRequest, UpdateBoardSettingsRequest,
};
use aionui_common::{generate_id, now_ms};
use aionui_db::{
    BoardCardRow, BoardColumnRow, BoardDepRow, BoardProjectRow, BoardSettingsRow, IBoardStore, IBoardTx, NewBoardEvent,
};
use aionui_realtime::EventBroadcaster;
use serde_json::{Value, json};
use tracing::{Instrument, Span, info, info_span, warn};

use crate::actor::BoardActor;
use crate::convert::{column_dto, event_dto, settings_dto};
use crate::error::{BoardError, BoardErrorCode, BoardResult};
use crate::events::{
    BOARD_ARCHIVED, BOARD_CREATED, BOARD_UNARCHIVED, BoardEventEmitter, COLUMNS_CHANGED, SETTINGS_CHANGED,
};
use crate::ports::{BoardHostPort, KeeperConversation};
use crate::rate_limit::{AgentRateLimiter, WriteBudget};
use crate::templates::{ColumnTemplate, template_columns};
use crate::validation::{COLUMN_NAME_MAX, COLUMNS_MAX, METADATA_MAX_BYTES, RULE_MAX_BYTES, column_key, json_object};

/// The board service. Cheap to clone.
#[derive(Clone)]
pub struct BoardService {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) store: Arc<dyn IBoardStore>,
    pub(crate) emitter: BoardEventEmitter,
    pub(crate) host: OnceLock<Arc<dyn BoardHostPort>>,
    pub(crate) rate_limiter: AgentRateLimiter,
    /// Cards the board GET returns before paging (`next_cards_cursor`).
    pub(crate) view_page: std::sync::atomic::AtomicUsize,
}

/// Default first page of the board GET.
pub const BOARD_VIEW_PAGE: usize = 500;

/// What a committed write publishes.
pub(crate) struct Committed {
    pub(crate) event: BoardEventDto,
    pub(crate) card: Option<BoardCardDto>,
    pub(crate) review: Option<BoardReviewDto>,
}

impl Committed {
    pub(crate) fn event(event: BoardEventDto) -> Self {
        Self {
            event,
            card: None,
            review: None,
        }
    }

    pub(crate) fn card(event: BoardEventDto, card: BoardCardDto) -> Self {
        Self {
            event,
            card: Some(card),
            review: None,
        }
    }
}

/// Open the `board_write` span of one write (R10).
pub(crate) fn write_span(kind: &'static str, actor: &BoardActor, card_id: Option<&str>) -> Span {
    info_span!(
        "board_write",
        kind,
        card_id = card_id.unwrap_or(""),
        actor_kind = actor.kind().as_str(),
        actor_ref = actor.actor_ref(),
    )
}

impl BoardService {
    pub fn new(store: Arc<dyn IBoardStore>, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                emitter: BoardEventEmitter::new(broadcaster),
                host: OnceLock::new(),
                rate_limiter: AgentRateLimiter::default(),
                view_page: std::sync::atomic::AtomicUsize::new(BOARD_VIEW_PAGE),
            }),
        }
    }

    /// Change how many cards the board GET returns before paging.
    pub fn set_view_page_size(&self, cards: usize) {
        self.inner
            .view_page
            .store(cards.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// Install the host port (set-once; later calls are ignored).
    pub fn set_host_port(&self, port: Arc<dyn BoardHostPort>) {
        let _ = self.inner.host.set(port);
    }

    pub(crate) fn store(&self) -> &dyn IBoardStore {
        self.inner.store.as_ref()
    }

    pub(crate) fn host(&self) -> BoardResult<&Arc<dyn BoardHostPort>> {
        self.inner
            .host
            .get()
            .ok_or_else(|| BoardError::Internal("board host port not wired".into()))
    }

    /// Append the single event of a write inside its transaction.
    pub(crate) async fn append_event(
        tx: &mut dyn IBoardTx,
        actor: &BoardActor,
        project_id: &str,
        card_id: Option<&str>,
        kind: &str,
        payload: Value,
    ) -> BoardResult<BoardEventDto> {
        Self::append_event_with_id(tx, generate_id(), actor, project_id, card_id, kind, payload).await
    }

    pub(crate) async fn append_event_with_id(
        tx: &mut dyn IBoardTx,
        id: String,
        actor: &BoardActor,
        project_id: &str,
        card_id: Option<&str>,
        kind: &str,
        payload: Value,
    ) -> BoardResult<BoardEventDto> {
        let event = NewBoardEvent {
            id,
            card_id: card_id.map(str::to_owned),
            project_id: project_id.to_owned(),
            user_id: actor.user_id().to_owned(),
            actor_kind: actor.kind().as_str().to_owned(),
            actor_ref: actor.actor_ref().to_owned(),
            conversation_id: actor.conversation_id().map(str::to_owned),
            kind: kind.to_owned(),
            payload: payload.to_string(),
            created_at: now_ms(),
        };
        let placed = tx.insert_event(&event).await?;
        Ok(event_dto(&aionui_db::BoardEventRow {
            seq: placed.seq,
            prev_seq: placed.prev_seq,
            id: event.id,
            card_id: event.card_id,
            project_id: event.project_id,
            user_id: event.user_id,
            actor_kind: event.actor_kind,
            actor_ref: event.actor_ref,
            conversation_id: event.conversation_id,
            kind: event.kind,
            payload: event.payload,
            created_at: event.created_at,
        }))
    }

    /// Commit and broadcast exactly one `board.changed` (R8).
    pub(crate) async fn commit(&self, tx: Box<dyn IBoardTx>, user_id: &str, committed: &Committed) -> BoardResult<()> {
        tx.commit().await?;
        info!(
            seq = committed.event.seq,
            kind = %committed.event.kind,
            project_id = %committed.event.project_id,
            "board write committed"
        );
        self.inner.emitter.board_changed(
            user_id,
            &committed.event,
            committed.card.as_ref(),
            committed.review.as_ref(),
        );
        Ok(())
    }

    /// Charge an agent write against its per-conversation budget (R9).
    pub(crate) fn charge(&self, actor: &BoardActor, budget: WriteBudget) -> BoardResult<()> {
        let Some(agent) = actor.as_agent() else {
            return Ok(());
        };
        if self.inner.rate_limiter.try_acquire(&agent.conversation_id, budget) {
            Ok(())
        } else {
            Err(BoardError::new(
                BoardErrorCode::RateLimited,
                "too many board writes from this conversation; retry in a minute",
            ))
        }
    }

    /// The owner's standard project (R7): missing/foreign → 404, temp → 409
    /// `board_no_project` (temp projects have no board).
    pub(crate) async fn user_project(&self, user_id: &str, project_id: &str) -> BoardResult<BoardProjectRow> {
        match self.store().project(user_id, project_id).await? {
            Some(project) if project.kind == "standard" => Ok(project),
            Some(_) => Err(BoardError::new(
                BoardErrorCode::NoProject,
                "temporary projects have no board",
            )),
            None => Err(BoardError::not_found("project")),
        }
    }

    /// Refuse writes on an archived board (409 owner / 403 agent) and agent
    /// access to a disabled board (R1).
    pub(crate) fn check_writable(settings: &BoardSettingsRow, actor: &BoardActor) -> BoardResult<()> {
        if settings.archived_at.is_some() {
            return Err(BoardError::archived(actor.as_agent().is_some()));
        }
        if actor.as_agent().is_some() && !settings.enabled {
            return Err(disabled());
        }
        Ok(())
    }

    /// Settings of an existing board inside a write transaction.
    pub(crate) async fn tx_settings(
        tx: &mut dyn IBoardTx,
        actor: &BoardActor,
        project_id: &str,
    ) -> BoardResult<BoardSettingsRow> {
        match tx.settings(actor.user_id(), project_id).await? {
            Some(settings) => Ok(settings),
            None if actor.as_agent().is_some() => Err(disabled()),
            None => Err(not_turned_on()),
        }
    }

    /// Clear `archived_at` once an active conversation or team is bound to the
    /// project again (SPEC §6). Runs at the start of a WRITE (never on reads):
    /// its own write with one `board_unarchived` event, then the caller's
    /// write proceeds.
    pub(crate) async fn refresh_archive(&self, actor: &BoardActor, project_id: &str) -> BoardResult<()> {
        let user_id = actor.user_id();
        let Some(settings) = self.store().settings(user_id, project_id).await? else {
            return Ok(());
        };
        if settings.archived_at.is_none() || !self.store().project_has_active_binding(user_id, project_id).await? {
            return Ok(());
        }
        self.set_archived_state(actor, project_id, false, "active_binding")
            .await
    }

    async fn set_archived_state(
        &self,
        actor: &BoardActor,
        project_id: &str,
        archived: bool,
        reason: &str,
    ) -> BoardResult<()> {
        let kind = if archived { BOARD_ARCHIVED } else { BOARD_UNARCHIVED };
        async {
            let user_id = actor.user_id();
            let mut tx = self.store().begin().await?;
            let Some(mut settings) = tx.settings(user_id, project_id).await? else {
                return Ok(());
            };
            if settings.archived_at.is_some() == archived {
                return Ok(());
            }
            let expected = settings.version;
            settings.archived_at = archived.then(now_ms);
            settings.version += 1;
            settings.updated_at = now_ms();
            if !tx.update_settings(&settings, expected).await? {
                return Err(version_conflict(expected));
            }
            let dto = settings_dto(project_id, Some(&settings));
            let event = Self::append_event(
                tx.as_mut(),
                actor,
                project_id,
                None,
                kind,
                json!({ "settings": dto, "version": settings.version, "reason": reason }),
            )
            .await?;
            self.commit(tx, user_id, &Committed::event(event)).await
        }
        .instrument(write_span(kind, actor, None))
        .await
    }

    // -- Read: the board -----------------------------------------------------

    /// `GET /api/projects/{pid}/board`: settings, columns, card summaries and
    /// the board's `last_seq`. A never-enabled board answers `enabled:false,
    /// version:0` with no columns. Writes nothing.
    pub async fn board_view(
        &self,
        actor: &BoardActor,
        project_id: &str,
        include_archived: bool,
    ) -> BoardResult<BoardView> {
        let user_id = actor.user_id();
        self.user_project(user_id, project_id).await?;
        let store = self.store();
        // Read the chain head first: the snapshot below is at least that
        // new, so replaying hints after `last_seq` never skips a change.
        let last_seq = store.last_seq(user_id, project_id).await?;
        let settings = store.settings(user_id, project_id).await?;
        let columns = store.columns(user_id, project_id).await?;
        let cards = store.cards(user_id, project_id, include_archived).await?;
        let deps = store.project_dependencies(user_id, project_id).await?;
        let page = self.inner.view_page.load(std::sync::atomic::Ordering::Relaxed);
        let next_cards_cursor = (cards.len() > page).then(|| crate::service_relations::encode_cursor(&cards[page - 1]));
        let summaries = self
            .summaries(user_id, &cards[..cards.len().min(page)], &columns, &deps)
            .await?;
        let mut counts: HashMap<&str, i64> = HashMap::new();
        for card in cards.iter().filter(|card| card.archived_at.is_none()) {
            *counts.entry(card.column_key.as_str()).or_default() += 1;
        }
        Ok(BoardView {
            settings: settings_dto(project_id, settings.as_ref()),
            columns: columns
                .iter()
                .map(|column| column_dto(column, counts.get(column.key.as_str()).copied().unwrap_or(0)))
                .collect(),
            cards: summaries,
            last_seq,
            next_cards_cursor,
        })
    }

    /// Board-list form of `cards`: effective blocked state, open dependency
    /// count and `needs_decision`.
    pub(crate) async fn summaries(
        &self,
        user_id: &str,
        cards: &[BoardCardRow],
        columns: &[BoardColumnRow],
        deps: &[BoardDepRow],
    ) -> BoardResult<Vec<aionui_api_types::BoardCardSummaryDto>> {
        let open = open_dependencies(cards, deps, columns, self, user_id).await?;
        let decisions = self.needs_decision(user_id, cards, columns).await?;
        let creators = crate::convert::card_creators(
            self.store()
                .card_creators(user_id, &crate::convert::agent_card_ids(cards))
                .await?,
        );
        Ok(cards
            .iter()
            .map(|card| {
                let open = open.get(&card.id).copied().unwrap_or(0);
                let mut summary =
                    crate::convert::card_summary(card, card.blocked || open > 0, open, decisions.contains(&card.id));
                crate::convert::set_creator(
                    &card.id,
                    &mut summary.creator_conversation_id,
                    &mut summary.creator_name,
                    &creators,
                );
                summary
            })
            .collect())
    }

    /// Ids of active cards waiting in a gate column whose `owner_if` they
    /// match (R3), evaluated on their current reviews and questions.
    async fn needs_decision(
        &self,
        user_id: &str,
        cards: &[BoardCardRow],
        columns: &[BoardColumnRow],
    ) -> BoardResult<HashSet<String>> {
        let gates: HashMap<&str, Value> = columns
            .iter()
            .filter(|column| column.kind == ColumnKind::Gate.as_str() && !column.archived)
            .map(|column| {
                let rule = column
                    .gate
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or_else(|| json!({}));
                (column.key.as_str(), rule)
            })
            .collect();
        let mut waiting = HashSet::new();
        for card in cards.iter().filter(|card| card.archived_at.is_none()) {
            let Some(rule) = gates.get(card.column_key.as_str()) else {
                continue;
            };
            let reviews = self.store().reviews(user_id, &card.id).await?;
            let questions = self.store().questions(user_id, &card.id).await?;
            if crate::rules::owner_must_decide(rule, card, &reviews, &questions) {
                waiting.insert(card.id.clone());
            }
        }
        Ok(waiting)
    }

    // -- Write: settings and columns ------------------------------------------

    /// `PUT /api/projects/{pid}/board/settings` (owner only). `version` 0
    /// creates the board with the studio columns. Turning the board on also
    /// creates its Board Keeper conversation when it has none (D40) —
    /// best-effort: a keeper failure is logged and `POST …/board/keeper`
    /// retries it.
    pub async fn update_settings(
        &self,
        actor: &BoardActor,
        project_id: &str,
        request: UpdateBoardSettingsRequest,
    ) -> BoardResult<BoardSettingsDto> {
        require_owner(actor)?;
        let user_id = actor.user_id();
        let project = self.user_project(user_id, project_id).await?;
        let settings_json = match &request.settings {
            Some(value) => Some(json_object("settings", value, METADATA_MAX_BYTES)?),
            None => None,
        };
        self.refresh_archive(actor, project_id).await?;
        let current = self.store().settings(user_id, project_id).await?;
        if let Some(current) = &current {
            Self::check_writable(current, actor)?;
            if current.version != request.version {
                return Err(version_conflict(current.version));
            }
        } else if request.version != 0 {
            return Err(version_conflict(0));
        }
        let enabling = request.enabled == Some(true) && current.as_ref().is_none_or(|s| !s.enabled);
        let keeper = if enabling && self.keeper_missing(user_id, current.as_ref()).await? {
            match self.create_keeper(user_id, &project).await {
                Ok(keeper) => Some(keeper),
                Err(error) => {
                    warn!(project_id = %project_id, error = %error, "board keeper could not be created");
                    None
                }
            }
        } else {
            None
        };
        let kind = if current.is_none() {
            BOARD_CREATED
        } else {
            SETTINGS_CHANGED
        };
        let result = async {
            let mut tx = self.store().begin().await?;
            let now = now_ms();
            let (settings, payload) = match tx.settings(user_id, project_id).await? {
                None => {
                    if request.version != 0 {
                        return Err(version_conflict(0));
                    }
                    let template = request.template.unwrap_or(BoardTemplate::Studio);
                    let row = BoardSettingsRow {
                        project_id: project_id.to_owned(),
                        user_id: user_id.to_owned(),
                        enabled: request.enabled.unwrap_or(false),
                        template: template.as_str().to_owned(),
                        next_number: 1,
                        version: 1,
                        settings: settings_json.clone().unwrap_or_else(|| json!({})).to_string(),
                        archived_at: None,
                        keeper_conversation_id: keeper.clone(),
                        last_event_seq: 0,
                        created_at: now,
                        updated_at: now,
                    };
                    tx.insert_settings(&row).await?;
                    let columns = template_rows(user_id, project_id, &template_columns(template));
                    tx.upsert_columns(&columns).await?;
                    let payload = json!({
                        "settings": settings_dto(project_id, Some(&row)),
                        "columns": columns.iter().map(|c| column_dto(c, 0)).collect::<Vec<_>>(),
                        "version": row.version,
                    });
                    (row, payload)
                }
                Some(mut row) => {
                    Self::check_writable(&row, actor)?;
                    if row.version != request.version {
                        return Err(version_conflict(row.version));
                    }
                    let expected = row.version;
                    if let Some(enabled) = request.enabled {
                        row.enabled = enabled;
                    }
                    if let Some(value) = &settings_json {
                        row.settings = value.to_string();
                    }
                    if keeper.is_some() {
                        row.keeper_conversation_id = keeper.clone();
                    }
                    row.version += 1;
                    row.updated_at = now;
                    if !tx.update_settings(&row, expected).await? {
                        return Err(version_conflict(expected));
                    }
                    let payload = json!({ "settings": settings_dto(project_id, Some(&row)), "version": row.version });
                    (row, payload)
                }
            };
            let event = Self::append_event(tx.as_mut(), actor, project_id, None, kind, payload).await?;
            self.commit(tx, user_id, &Committed::event(event)).await?;
            Ok(settings_dto(project_id, Some(&settings)))
        }
        .instrument(write_span(kind, actor, None))
        .await;
        if result.is_err()
            && let Some(keeper) = &keeper
        {
            self.discard_conversation(user_id, keeper).await;
        }
        result
    }

    /// `PUT /api/projects/{pid}/board/columns` (owner only): the full active
    /// list in board order, matched by `key`; existing columns left out are
    /// archived.
    pub async fn update_columns(
        &self,
        actor: &BoardActor,
        project_id: &str,
        request: UpdateBoardColumnsRequest,
    ) -> BoardResult<BoardColumnsResponse> {
        require_owner(actor)?;
        let user_id = actor.user_id();
        self.user_project(user_id, project_id).await?;
        let inputs = validate_columns(&request.columns)?;
        self.refresh_archive(actor, project_id).await?;
        async {
            let mut tx = self.store().begin().await?;
            let mut settings = Self::tx_settings(tx.as_mut(), actor, project_id).await?;
            Self::check_writable(&settings, actor)?;
            if settings.version != request.version {
                return Err(version_conflict(settings.version));
            }
            let existing = tx.columns(user_id, project_id).await?;
            let keep: HashSet<&str> = inputs.iter().map(|c| c.key.as_str()).collect();
            let mut rows = Vec::with_capacity(existing.len() + inputs.len());
            for (index, input) in inputs.iter().enumerate() {
                let id = existing
                    .iter()
                    .find(|c| c.key == input.key)
                    .map(|c| c.id.clone())
                    .unwrap_or_else(generate_id);
                rows.push(BoardColumnRow {
                    id,
                    project_id: project_id.to_owned(),
                    user_id: user_id.to_owned(),
                    position: index as i64,
                    ..input.clone()
                });
            }
            let targets = gate_targets(&rows);
            let dropped = existing.iter().filter(|c| !keep.contains(c.key.as_str()));
            for (next_position, column) in (inputs.len() as i64..).zip(dropped) {
                if !column.archived && tx.count_column_cards(user_id, project_id, &column.key, None).await? > 0 {
                    return Err(BoardError::new(
                        BoardErrorCode::Invalid,
                        format!("column `{}` still has cards; move them first", column.key),
                    )
                    .with_details(json!({ "field": "columns", "key": column.key, "reason": "has_cards" })));
                }
                if targets.contains(&column.key) {
                    return Err(BoardError::new(
                        BoardErrorCode::Invalid,
                        format!("column `{}` is the target of a gate", column.key),
                    )
                    .with_details(json!({ "field": "columns", "key": column.key, "reason": "gate_target" })));
                }
                rows.push(BoardColumnRow {
                    archived: true,
                    position: next_position,
                    ..column.clone()
                });
            }
            tx.upsert_columns(&rows).await?;
            let expected = settings.version;
            settings.version += 1;
            settings.updated_at = now_ms();
            if !tx.update_settings(&settings, expected).await? {
                return Err(version_conflict(expected));
            }
            let mut dtos = Vec::with_capacity(rows.len());
            for row in &rows {
                let count = tx.count_column_cards(user_id, project_id, &row.key, None).await?;
                dtos.push(column_dto(row, count));
            }
            let event = Self::append_event(
                tx.as_mut(),
                actor,
                project_id,
                None,
                COLUMNS_CHANGED,
                json!({ "columns": dtos, "version": settings.version }),
            )
            .await?;
            self.commit(tx, user_id, &Committed::event(event)).await?;
            Ok(BoardColumnsResponse {
                columns: dtos,
                version: settings.version,
            })
        }
        .instrument(write_span(COLUMNS_CHANGED, actor, None))
        .await
    }

    /// The owner's "Reactivate board" on an archived project's board.
    pub async fn reactivate(&self, actor: &BoardActor, project_id: &str) -> BoardResult<BoardSettingsDto> {
        require_owner(actor)?;
        let user_id = actor.user_id();
        self.user_project(user_id, project_id).await?;
        if self.store().settings(user_id, project_id).await?.is_none() {
            return Err(not_turned_on());
        }
        self.set_archived_state(actor, project_id, false, "reactivated").await?;
        let settings = self.store().settings(user_id, project_id).await?;
        Ok(settings_dto(project_id, settings.as_ref()))
    }

    // -- Board Keeper (D40) --------------------------------------------------

    /// Whether the board has no live keeper conversation.
    async fn keeper_missing(&self, user_id: &str, settings: Option<&BoardSettingsRow>) -> BoardResult<bool> {
        let Some(keeper) = settings.and_then(|s| s.keeper_conversation_id.as_deref()) else {
            return Ok(true);
        };
        Ok(self.store().conversation_info(user_id, keeper).await?.is_none())
    }

    /// Create the keeper conversation `<project> · Board` with the Board
    /// Keeper assistant in the project's workspace folder.
    async fn create_keeper(&self, user_id: &str, project: &BoardProjectRow) -> BoardResult<String> {
        let host = self.host()?;
        let workspace = host
            .project_workspace(user_id, &project.project_id)
            .await
            .map_err(BoardError::Internal)?
            .ok_or_else(|| BoardError::invalid("project", "the project has no workspace folder"))?;
        let assistant_id = host
            .ensure_keeper_assistant(user_id)
            .await
            .map_err(BoardError::Internal)?;
        let conversation_id = host
            .create_keeper_conversation(
                user_id,
                &KeeperConversation {
                    assistant_id,
                    name: format!("{} · Board", project.name),
                    workspace,
                },
            )
            .await
            .map_err(BoardError::Internal)?;
        info!(project_id = %project.project_id, conversation_id = %conversation_id, "board keeper created");
        Ok(conversation_id)
    }

    /// Best-effort removal of a conversation created by a write that then
    /// failed.
    pub(crate) async fn discard_conversation(&self, user_id: &str, conversation_id: &str) {
        if let Ok(host) = self.host()
            && let Err(error) = host.delete_conversation(user_id, conversation_id).await
        {
            warn!(conversation_id = %conversation_id, error = %error, "board: orphan conversation not deleted");
        }
    }

    /// `POST /api/projects/{pid}/board/keeper` (owner only): return the live
    /// keeper conversation, or (re)create it.
    pub async fn ensure_keeper(&self, actor: &BoardActor, project_id: &str) -> BoardResult<BoardKeeperResponse> {
        require_owner(actor)?;
        let user_id = actor.user_id();
        let project = self.user_project(user_id, project_id).await?;
        self.refresh_archive(actor, project_id).await?;
        let current = self
            .store()
            .settings(user_id, project_id)
            .await?
            .ok_or_else(not_turned_on)?;
        Self::check_writable(&current, actor)?;
        if !self.keeper_missing(user_id, Some(&current)).await?
            && let Some(keeper) = current.keeper_conversation_id
        {
            return Ok(BoardKeeperResponse {
                conversation_id: keeper,
                created: false,
            });
        }
        let keeper = self.create_keeper(user_id, &project).await?;
        let result = async {
            let mut tx = self.store().begin().await?;
            let mut settings = Self::tx_settings(tx.as_mut(), actor, project_id).await?;
            Self::check_writable(&settings, actor)?;
            if settings.keeper_conversation_id != current.keeper_conversation_id
                && let Some(winner) = settings.keeper_conversation_id.clone()
            {
                // A concurrent request created the keeper first: keep theirs.
                return Ok(Some(winner));
            }
            let expected = settings.version;
            settings.keeper_conversation_id = Some(keeper.clone());
            settings.version += 1;
            settings.updated_at = now_ms();
            if !tx.update_settings(&settings, expected).await? {
                return Err(version_conflict(expected));
            }
            let payload = json!({ "settings": settings_dto(project_id, Some(&settings)), "version": settings.version });
            let event = Self::append_event(tx.as_mut(), actor, project_id, None, SETTINGS_CHANGED, payload).await?;
            self.commit(tx, user_id, &Committed::event(event)).await?;
            Ok(None)
        }
        .instrument(write_span(SETTINGS_CHANGED, actor, None))
        .await;
        match result {
            Ok(None) => Ok(BoardKeeperResponse {
                conversation_id: keeper,
                created: true,
            }),
            Ok(Some(winner)) => {
                self.discard_conversation(user_id, &keeper).await;
                Ok(BoardKeeperResponse {
                    conversation_id: winner,
                    created: false,
                })
            }
            Err(error) => {
                self.discard_conversation(user_id, &keeper).await;
                Err(error)
            }
        }
    }

    // -- Project lifecycle ports (sidebar) -----------------------------------

    /// Project archive/unarchive: flip `board_settings.archived_at` (a board
    /// that was never created is left alone).
    pub async fn set_board_archived(&self, user_id: &str, project_id: &str, archived: bool) -> BoardResult<()> {
        let reason = if archived {
            "project_archived"
        } else {
            "project_unarchived"
        };
        self.set_archived_state(&BoardActor::user(user_id), project_id, archived, reason)
            .await
    }

    /// Project deletion: hard-delete the whole board. Returns cards removed.
    pub async fn delete_board(&self, user_id: &str, project_id: &str) -> BoardResult<u64> {
        let removed = self.store().delete_board(user_id, project_id).await?;
        info!(project_id = %project_id, cards = removed, "board deleted with its project");
        Ok(removed)
    }

    /// Cards a project deletion would remove (archived included).
    pub async fn count_cards(&self, user_id: &str, project_id: &str) -> BoardResult<i64> {
        Ok(self.store().count_cards(user_id, project_id, true).await?)
    }
}

/// Open dependency count per card id: dependencies whose target is not in a
/// `completed` column (R5).
pub(crate) async fn open_dependencies(
    cards: &[BoardCardRow],
    deps: &[BoardDepRow],
    columns: &[BoardColumnRow],
    service: &BoardService,
    user_id: &str,
) -> BoardResult<HashMap<String, i64>> {
    let mut column_of: HashMap<String, String> = cards
        .iter()
        .map(|card| (card.id.clone(), card.column_key.clone()))
        .collect();
    // Dependency targets may be archived (absent from an active-only list).
    let missing: Vec<String> = deps
        .iter()
        .filter(|dep| !column_of.contains_key(&dep.depends_on_card_id))
        .map(|dep| dep.depends_on_card_id.clone())
        .collect();
    for card in service.store().cards_by_ids(user_id, &missing).await? {
        column_of.insert(card.id, card.column_key);
    }
    let completed = completed_columns(columns);
    let mut open: HashMap<String, i64> = HashMap::new();
    for dep in deps {
        let resolved = column_of
            .get(&dep.depends_on_card_id)
            .is_some_and(|column| completed.contains(column.as_str()));
        if !resolved {
            *open.entry(dep.card_id.clone()).or_default() += 1;
        }
    }
    Ok(open)
}

/// Keys of columns whose resolution is `completed`.
pub(crate) fn completed_columns(columns: &[BoardColumnRow]) -> HashSet<&str> {
    columns
        .iter()
        .filter(|column| column.resolution.as_deref() == Some(ColumnResolution::Completed.as_str()))
        .map(|column| column.key.as_str())
        .collect()
}

pub(crate) fn disabled() -> BoardError {
    BoardError::new(BoardErrorCode::Disabled, "the board is turned off for this project")
}

pub(crate) fn not_turned_on() -> BoardError {
    BoardError::new(BoardErrorCode::NotFound, "the board is not turned on for this project")
}

pub(crate) fn version_conflict(current_version: i64) -> BoardError {
    BoardError::new(
        BoardErrorCode::VersionConflict,
        "the board changed meanwhile; reload and retry",
    )
    .with_details(json!({ "current_version": current_version }))
}

/// Settings, columns, archive, deps, gates, keeper, reviews: owner only (SPEC
/// §2 — the supervisor moves cards, writes events/links and assigns).
pub(crate) fn require_owner(actor: &BoardActor) -> BoardResult<()> {
    match actor {
        BoardActor::User { .. } => Ok(()),
        BoardActor::System { .. } => Err(BoardError::new(
            BoardErrorCode::SystemForbidden,
            "the supervisor may not perform this action",
        )),
        BoardActor::Agent(_) => Err(use_agent_router()),
    }
}

/// Owner or supervisor (links, assign).
pub(crate) fn require_owner_or_system(actor: &BoardActor) -> BoardResult<()> {
    match actor {
        BoardActor::Agent(_) => Err(use_agent_router()),
        _ => Ok(()),
    }
}

fn use_agent_router() -> BoardError {
    BoardError::new(BoardErrorCode::UseAgentRouter, "agents use the agent router")
}

fn template_rows(user_id: &str, project_id: &str, templates: &[ColumnTemplate]) -> Vec<BoardColumnRow> {
    templates
        .iter()
        .enumerate()
        .map(|(index, template)| BoardColumnRow {
            id: generate_id(),
            project_id: project_id.to_owned(),
            user_id: user_id.to_owned(),
            key: template.key.to_owned(),
            name: None,
            category: template.category.as_str().to_owned(),
            kind: template.kind.as_str().to_owned(),
            resolution: template.resolution.map(|r| r.as_str().to_owned()),
            agent_move: template.agent_move.as_str().to_owned(),
            wip_limit: template.wip_limit,
            exit_rule: template.exit_rule.as_ref().map(Value::to_string),
            gate: template.gate.as_ref().map(Value::to_string),
            position: index as i64,
            archived: false,
        })
        .collect()
}

/// Keys named as `on_approve` / `on_reject` by an active gate.
fn gate_targets(rows: &[BoardColumnRow]) -> HashSet<String> {
    rows.iter()
        .filter(|row| !row.archived)
        .filter_map(|row| row.gate.as_deref())
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
        .flat_map(|gate| {
            ["on_approve", "on_reject"]
                .into_iter()
                .filter_map(|key| gate.get(key).and_then(Value::as_str).map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Validate a columns PUT into rows (ids/positions/owner filled in later).
fn validate_columns(inputs: &[BoardColumnInput]) -> BoardResult<Vec<BoardColumnRow>> {
    if inputs.is_empty() || inputs.len() > COLUMNS_MAX {
        return Err(BoardError::invalid(
            "columns",
            format!("a board has between 1 and {COLUMNS_MAX} columns"),
        ));
    }
    let mut seen = HashSet::new();
    let mut rows = Vec::with_capacity(inputs.len());
    for input in inputs {
        let key = column_key(&input.key)?;
        if !seen.insert(key.clone()) {
            return Err(BoardError::invalid(
                "columns.key",
                format!("duplicate column key `{key}`"),
            ));
        }
        let name = match input.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) if name.chars().count() > COLUMN_NAME_MAX => {
                return Err(BoardError::invalid(
                    "columns.name",
                    format!("column names are at most {COLUMN_NAME_MAX} characters"),
                ));
            }
            other => other.map(str::to_owned),
        };
        let kind = input.kind.unwrap_or(ColumnKind::Normal);
        match (kind, input.resolution) {
            (ColumnKind::Terminal, None) => {
                return Err(BoardError::invalid(
                    "columns.resolution",
                    format!("terminal column `{key}` needs a resolution (completed | dropped)"),
                ));
            }
            (ColumnKind::Normal | ColumnKind::Gate, Some(_)) => {
                return Err(BoardError::invalid(
                    "columns.resolution",
                    format!("only terminal columns carry a resolution (`{key}`)"),
                ));
            }
            _ => {}
        }
        if let Some(limit) = input.wip_limit
            && !(1..=999).contains(&limit)
        {
            return Err(BoardError::invalid(
                "columns.wip_limit",
                "wip_limit is between 1 and 999",
            ));
        }
        let exit_rule = match input.exit_rule.as_ref().filter(|v| !v.is_null()) {
            Some(value) => Some(json_object("columns.exit_rule", value, RULE_MAX_BYTES)?.to_string()),
            None => None,
        };
        let gate = match (kind, input.gate.as_ref().filter(|v| !v.is_null())) {
            (ColumnKind::Gate, Some(value)) => Some(json_object("columns.gate", value, RULE_MAX_BYTES)?.to_string()),
            (ColumnKind::Gate, None) => {
                return Err(BoardError::invalid(
                    "columns.gate",
                    format!("gate column `{key}` needs a gate rule"),
                ));
            }
            (_, Some(_)) => {
                return Err(BoardError::invalid(
                    "columns.gate",
                    format!("only gate columns carry a gate rule (`{key}`)"),
                ));
            }
            (_, None) => None,
        };
        rows.push(BoardColumnRow {
            id: String::new(),
            project_id: String::new(),
            user_id: String::new(),
            key,
            name,
            category: input.category.as_str().to_owned(),
            kind: kind.as_str().to_owned(),
            resolution: input.resolution.map(|r| r.as_str().to_owned()),
            agent_move: input.agent_move.unwrap_or(AgentMove::Deny).as_str().to_owned(),
            wip_limit: input.wip_limit,
            exit_rule,
            gate,
            position: 0,
            archived: false,
        });
    }
    // Gate targets must be columns of the new list.
    let keys: HashSet<&str> = rows.iter().map(|row| row.key.as_str()).collect();
    for target in gate_targets(&rows) {
        if !keys.contains(target.as_str()) {
            return Err(BoardError::invalid(
                "columns.gate",
                format!("gate target `{target}` is not a column of the board"),
            ));
        }
    }
    Ok(rows)
}
