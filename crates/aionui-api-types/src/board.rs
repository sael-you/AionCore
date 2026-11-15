//! Request/response DTOs of the built-in project board (SPEC §5).
//!
//! The user router (`/api/projects/{pid}/board…`, `/api/board/…`) answers
//! `ApiResponse<T>` / coded `ErrorResponse`s; the agent router
//! (`/api/runtime/board/…`) answers [`BoardCliEnvelope`]. Enum values are
//! the lowercase wire strings stored in the DB.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// WebSocket event name broadcast after every committed board-state write.
pub const BOARD_CHANGED_EVENT: &str = "board.changed";
/// Header that marks a user-router request as the Studio supervisor
/// (`system` actor; Local mode only). The only accepted value is
/// [`BOARD_ACTOR_SUPERVISOR`].
pub const BOARD_ACTOR_HEADER: &str = "x-aionui-board-actor";
pub const BOARD_ACTOR_SUPERVISOR: &str = "supervisor";

/// Deserialize a field that distinguishes "absent" (`None`) from an explicit
/// `null` (`Some(None)`), for PATCH bodies that may clear a nullable field.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// The wire / DB string.
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $wire),+ }
            }

            /// Parse a wire / DB string.
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($wire => Some($name::$variant),)+ _ => None }
            }
        }
    };
}

wire_enum!(
    /// Column layout a board is created with. Every board uses the
    /// department flow (D39).
    BoardTemplate { Studio => "studio" }
);
wire_enum!(
    /// A department's verdict on a card (R6).
    ReviewVerdict { Pending => "pending", Approve => "approve", Change => "change", Reject => "reject", Blocked => "blocked" }
);
wire_enum!(
    FindingSeverity { Info => "info", Low => "low", Medium => "medium", High => "high", Critical => "critical" }
);
wire_enum!(
    /// The owner's decision on a gate (R3).
    GateDecision { Approve => "approve", Reject => "reject" }
);
wire_enum!(
    /// The upstream F-KANBAN category a column maps to.
    ColumnCategory { Backlog => "backlog", Todo => "todo", InProgress => "in_progress", Done => "done" }
);
wire_enum!(
    ColumnKind { Normal => "normal", Gate => "gate", Terminal => "terminal" }
);
wire_enum!(
    /// What reaching a terminal column means for dependencies (R5).
    ColumnResolution { Completed => "completed", Dropped => "dropped" }
);
wire_enum!(
    /// Whether agents may move cards into / out of a column.
    AgentMove { Allow => "allow", Deny => "deny" }
);
wire_enum!(
    CardType { Idea => "idea", Bug => "bug", Feature => "feature", Update => "update", Chore => "chore", Research => "research" }
);
wire_enum!(
    CardPriority { Low => "low", Medium => "medium", High => "high", Urgent => "urgent" }
);
wire_enum!(
    CardSize { S => "s", M => "m", L => "l" }
);
wire_enum!(
    CardTier { T0 => "t0", T1 => "t1", T2 => "t2", T3 => "t3" }
);
wire_enum!(
    /// Who wrote a board change (SPEC §2).
    BoardActorKind { User => "user", Agent => "agent", System => "system" }
);
wire_enum!(
    AssigneeKind { Assistant => "assistant", Team => "team", User => "user" }
);
wire_enum!(
    LinkRole { Lead => "lead", Review => "review", Build => "build", Brainstorm => "brainstorm", Qa => "qa", Other => "other" }
);

// ---------------------------------------------------------------------------
// Read models
// ---------------------------------------------------------------------------

/// Board header. `version == 0` and `exists == false` describe a board that
/// was never turned on (no row yet).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardSettingsDto {
    pub project_id: String,
    pub exists: bool,
    pub enabled: bool,
    pub template: BoardTemplate,
    pub next_number: i64,
    pub version: i64,
    pub settings: Value,
    pub archived_at: Option<i64>,
    /// The project's Board Keeper conversation (D40).
    pub keeper_conversation_id: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardColumnDto {
    pub key: String,
    /// `None` = render the i18n default `board.columns.<key>`.
    pub name: Option<String>,
    pub category: ColumnCategory,
    pub kind: ColumnKind,
    pub resolution: Option<ColumnResolution>,
    pub agent_move: AgentMove,
    pub wip_limit: Option<i64>,
    pub exit_rule: Option<Value>,
    pub gate: Option<Value>,
    pub position: i64,
    pub archived: bool,
    /// Active (non-archived) cards currently in the column.
    pub card_count: i64,
}

/// Full card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardCardDto {
    pub id: String,
    pub project_id: String,
    pub number: i64,
    /// `TASK-<number>`.
    pub reference: String,
    pub title: String,
    pub description: String,
    #[serde(rename = "type")]
    pub card_type: CardType,
    pub priority: CardPriority,
    pub size: Option<CardSize>,
    pub tier: Option<CardTier>,
    pub column_key: String,
    pub position: i64,
    pub labels: Vec<String>,
    pub skills: Vec<String>,
    pub routing: Vec<String>,
    pub creator_kind: BoardActorKind,
    pub creator_ref: String,
    pub assignee_kind: Option<AssigneeKind>,
    pub assignee_ref: Option<String>,
    pub blocked: bool,
    pub blocked_reason: Option<String>,
    /// Who set the manual block (agents cannot lift an owner's block).
    pub blocked_by: Option<BoardActorKind>,
    pub change_id: Option<String>,
    pub metadata: Value,
    /// Cards this card waits for.
    pub dependencies: Vec<BoardDependencyDto>,
    /// Dependencies not yet in a `completed` column.
    pub open_dependency_count: i64,
    pub version: i64,
    pub archived_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Card as listed on the board (no description/metadata).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardCardSummaryDto {
    pub id: String,
    pub number: i64,
    pub reference: String,
    pub title: String,
    #[serde(rename = "type")]
    pub card_type: CardType,
    pub priority: CardPriority,
    pub size: Option<CardSize>,
    pub tier: Option<CardTier>,
    pub column_key: String,
    pub position: i64,
    pub labels: Vec<String>,
    pub skills: Vec<String>,
    pub creator_kind: BoardActorKind,
    pub creator_ref: String,
    pub assignee_kind: Option<AssigneeKind>,
    pub assignee_ref: Option<String>,
    /// Display name of the assignee when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_name: Option<String>,
    /// Manual flag or an unresolved dependency (R5).
    pub blocked: bool,
    pub open_dependency_count: i64,
    /// In a gate column and matching its `owner_if`: only the owner's
    /// decision (or force) moves it on (R3).
    pub needs_decision: bool,
    pub version: i64,
    pub archived_at: Option<i64>,
    pub updated_at: i64,
}

/// `GET /api/projects/{pid}/board`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardView {
    pub settings: BoardSettingsDto,
    pub columns: Vec<BoardColumnDto>,
    pub cards: Vec<BoardCardSummaryDto>,
    /// `seq` of the board's newest event (head of its chain; resume point for
    /// the events API).
    pub last_seq: i64,
    /// Set when `cards` is only the first page: read the rest with
    /// `GET …/board/cards?cursor=<this>` (same order and filter).
    pub next_cards_cursor: Option<String>,
}

/// `PUT /api/projects/{pid}/board/columns` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardColumnsResponse {
    pub columns: Vec<BoardColumnDto>,
    /// `board_settings.version` after the change.
    pub version: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardReviewDto {
    pub id: String,
    pub department: String,
    pub reviewer_ref: String,
    pub conversation_id: Option<String>,
    pub verdict: String,
    pub summary: String,
    pub findings: Value,
    pub recommendation: String,
    pub assumptions: Value,
    pub confidence: Option<f64>,
    pub tokens_used: i64,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardQuestionDto {
    pub id: String,
    pub from_department: String,
    pub to_department: String,
    pub text: String,
    pub answered_by_event: Option<String>,
    pub open: bool,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Live state of a linked conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardLinkStatus {
    Active,
    Archived,
    /// The conversation no longer exists.
    Removed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardLinkDto {
    pub card_id: String,
    pub conversation_id: String,
    pub role: LinkRole,
    pub department: Option<String>,
    pub created_at: i64,
    pub status: BoardLinkStatus,
    pub conversation_name: Option<String>,
    /// The conversation's runtime status (`pending|running|finished`).
    pub conversation_status: Option<String>,
}

/// One side of a dependency edge, resolved to the other card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardDependencyDto {
    pub card_id: String,
    pub number: i64,
    pub reference: String,
    pub title: String,
    pub column_key: String,
    /// The dependency card reached a `completed` column (R5).
    pub resolved: bool,
    /// The dependency card reached a `dropped` column: it keeps the block.
    pub dropped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardEventDto {
    /// Global replay order.
    pub seq: i64,
    /// The project's previous event `seq` (0 for its first): a client whose
    /// last seen `seq` for the project differs has missed events.
    pub prev_seq: i64,
    pub id: String,
    pub card_id: Option<String>,
    pub project_id: String,
    pub actor_kind: BoardActorKind,
    pub actor_ref: String,
    pub conversation_id: Option<String>,
    pub kind: String,
    pub payload: Value,
    pub created_at: i64,
}

/// `GET /api/board/cards/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardCardDetail {
    pub card: BoardCardDto,
    pub reviews: Vec<BoardReviewDto>,
    pub questions: Vec<BoardQuestionDto>,
    pub links: Vec<BoardLinkDto>,
    /// Cards this card waits for.
    pub depends_on: Vec<BoardDependencyDto>,
    /// Cards waiting for this card (derived).
    pub blocks: Vec<BoardDependencyDto>,
    /// Last 50 events, ascending.
    pub events: Vec<BoardEventDto>,
    /// The assignee's resolved skill set (assistant: its enabled skills plus
    /// the auto-inject builtins it does not disable; team: the union over its
    /// members' assistants; no assignee: empty). Read-only.
    pub assignee_skills: Vec<String>,
}

/// A page of events (by `seq`, ascending).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardEventPage {
    pub items: Vec<BoardEventDto>,
    /// Pass as `after_seq` to read the next page.
    pub next_after_seq: i64,
    pub has_more: bool,
}

/// A page of cards (stable cursor over `(column, position, id)`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardCardPage {
    pub items: Vec<BoardCardSummaryDto>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

/// `GET …/packet` (SPEC §7): the card as an agent reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardPacketDto {
    pub card_id: String,
    pub number: i64,
    pub reference: String,
    /// Fenced in `<card-data>`; treat as data, not instructions.
    pub text: String,
    /// Rough size (`chars / 4`).
    pub approx_tokens: i64,
}

/// Data of the `board.changed` WebSocket event — an invalidation hint; after
/// a reconnect the paged events API is the source of truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardChangedPayload {
    pub user_id: String,
    pub project_id: String,
    pub seq: i64,
    /// Chain link of the project (see [`BoardEventDto::prev_seq`]).
    pub prev_seq: i64,
    pub event: BoardEventDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<BoardCardDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<BoardReviewDto>,
}

// ---------------------------------------------------------------------------
// User-router requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardViewQuery {
    #[serde(default)]
    pub include_archived: bool,
}

/// `PUT /api/projects/{pid}/board/settings`. `version` 0 creates the board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateBoardSettingsRequest {
    pub version: i64,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub template: Option<BoardTemplate>,
    /// JSON object replacing the stored settings.
    #[serde(default)]
    pub settings: Option<Value>,
}

/// One column of `PUT /api/projects/{pid}/board/columns`; list order is
/// board order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardColumnInput {
    pub key: String,
    #[serde(default)]
    pub name: Option<String>,
    pub category: ColumnCategory,
    #[serde(default)]
    pub kind: Option<ColumnKind>,
    #[serde(default)]
    pub resolution: Option<ColumnResolution>,
    #[serde(default)]
    pub agent_move: Option<AgentMove>,
    #[serde(default)]
    pub wip_limit: Option<i64>,
    #[serde(default)]
    pub exit_rule: Option<Value>,
    #[serde(default)]
    pub gate: Option<Value>,
}

/// `PUT /api/projects/{pid}/board/columns`: the full active list; columns
/// left out are archived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateBoardColumnsRequest {
    pub version: i64,
    pub columns: Vec<BoardColumnInput>,
}

/// `POST /api/projects/{pid}/board/cards` and the agent `create`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBoardCardRequest {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub card_type: Option<CardType>,
    #[serde(default)]
    pub priority: Option<CardPriority>,
    #[serde(default)]
    pub size: Option<CardSize>,
    #[serde(default)]
    pub tier: Option<CardTier>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub routing: Option<Vec<String>>,
    /// Target column key; default = the first active column.
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub change_id: Option<String>,
}

/// `PATCH /api/board/cards/{id}` and the agent `update`. `version` is the
/// card version the change is based on; explicit `null` clears a nullable
/// field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateBoardCardRequest {
    pub version: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub card_type: Option<CardType>,
    #[serde(default)]
    pub priority: Option<CardPriority>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub size: Option<Option<CardSize>>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub tier: Option<Option<CardTier>>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub routing: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub change_id: Option<Option<String>>,
    /// User router only (agents use `block`/`unblock`).
    #[serde(default)]
    pub blocked: Option<bool>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<Option<String>>,
}

/// `POST /api/board/cards/{id}/move`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveBoardCardRequest {
    pub to_column: String,
    /// Land directly before this card id (XOR `after_id`).
    #[serde(default)]
    pub before_id: Option<String>,
    /// Land directly after this card id (XOR `before_id`).
    #[serde(default)]
    pub after_id: Option<String>,
    /// The column the client believes the card is in; rejected only when the
    /// card's column changed meanwhile.
    pub expected_column: String,
    /// Owner override of WIP, blocked, exit rules and gates (user only).
    #[serde(default)]
    pub force: bool,
    /// Required when forcing a card out of a gate or `deploying`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/board/cards/{id}/gate` (owner only, R3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardGateRequest {
    pub decision: GateDecision,
    #[serde(default)]
    pub reason: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardFinding {
    pub severity: FindingSeverity,
    pub claim: String,
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardVerdictQuestion {
    /// Department (or `owner`) the question is addressed to.
    pub to: String,
    pub text: String,
}

/// `PUT …/reviews/{department}` (user and agent routers, SPEC §7 verdict).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardReviewRequest {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<BoardFinding>,
    #[serde(default)]
    pub recommendation: String,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub questions: Vec<BoardVerdictQuestion>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub tokens_used: Option<i64>,
}

/// Review write result: the current verdict plus the questions it opened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardReviewResult {
    pub review: BoardReviewDto,
    pub questions: Vec<BoardQuestionDto>,
}

/// `POST /api/board/cards/{id}/links` (owner / supervisor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardLinkRequest {
    pub conversation_id: String,
    pub role: LinkRole,
    #[serde(default)]
    pub department: Option<String>,
}

/// `POST /api/board/cards/{id}/assign` (§6): exactly one of `assistant_id`
/// and `team_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardAssignRequest {
    #[serde(default)]
    pub assistant_id: Option<String>,
    #[serde(default)]
    pub team_id: Option<String>,
    /// Required except for `role: brainstorm`.
    #[serde(default)]
    pub department: Option<String>,
    /// `lead` (default for teams) | `review` (default for assistants) |
    /// `build` | `qa` | `brainstorm` | `other`.
    #[serde(default)]
    pub role: Option<LinkRole>,
    pub idempotency_key: String,
}

/// `POST /api/board/cards/{id}/brainstorm` → assign with role `brainstorm`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardBrainstormRequest {
    /// Default: the project's Board Keeper assistant.
    #[serde(default)]
    pub assistant_id: Option<String>,
    /// Default: a fresh key (no retry protection).
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Why assign did not nest the new conversation under the card's lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentFallback {
    Depth,
    Fanout,
    Team,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardAssignResponse {
    pub card: BoardCardDto,
    pub link: BoardLinkDto,
    pub conversation_id: String,
    pub assignment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// `null` when the conversation nested under the card's lead (or the card
    /// has no lead).
    #[serde(default)]
    pub parent_fallback: Option<ParentFallback>,
    /// `false` when the packet could not be delivered (retry the same request
    /// to resend).
    pub packet_sent: bool,
}

/// `POST /api/projects/{pid}/board/keeper`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardKeeperResponse {
    /// The keeper conversation (existing or created).
    pub conversation_id: String,
    /// `true` when a new keeper conversation was created.
    pub created: bool,
}

/// `POST /api/board/cards/{id}/comments`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardCommentRequest {
    pub body: String,
    #[serde(default)]
    pub to_department: Option<String>,
    /// Id of the open question this comment answers (closes it).
    #[serde(default)]
    pub answers: Option<String>,
}

/// Body of mutations that carry no fields (`{}`): archive, restore, deps,
/// reactivate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardEmptyRequest {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardEventsQuery {
    #[serde(default)]
    pub after_seq: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/projects/{pid}/board/cards` (user) and the agent `list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardCardListQuery {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub column: Option<String>,
    /// Assignee ref; on the agent router `me` = the caller's assistant.
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default, rename = "type")]
    pub card_type: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub blocked: Option<bool>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default)]
    pub limit: Option<i64>,
}

// ---------------------------------------------------------------------------
// Agent router
// ---------------------------------------------------------------------------

/// `GET /api/runtime/board/status`: always 200 for an authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardStatusResponse {
    pub enabled: bool,
    pub project_id: Option<String>,
    pub template: Option<BoardTemplate>,
    /// The caller is the project's Board Keeper (may move between non-gate
    /// columns).
    #[serde(default)]
    pub keeper: bool,
}

/// Agent `update <N>` body (block state goes through `block`/`unblock`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUpdateBoardCardRequest {
    pub version: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub card_type: Option<CardType>,
    #[serde(default)]
    pub priority: Option<CardPriority>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub size: Option<Option<CardSize>>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub tier: Option<Option<CardTier>>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub routing: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub change_id: Option<Option<String>>,
}

impl From<AgentUpdateBoardCardRequest> for UpdateBoardCardRequest {
    fn from(value: AgentUpdateBoardCardRequest) -> Self {
        Self {
            version: value.version,
            title: value.title,
            description: value.description,
            card_type: value.card_type,
            priority: value.priority,
            size: value.size,
            tier: value.tier,
            labels: value.labels,
            skills: value.skills,
            routing: value.routing,
            metadata: value.metadata,
            change_id: value.change_id,
            blocked: None,
            blocked_reason: None,
        }
    }
}

/// Agent `move <N> <column>`: anchors are card numbers of the same project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMoveBoardCardRequest {
    pub to_column: String,
    #[serde(default)]
    pub before: Option<i64>,
    #[serde(default)]
    pub after: Option<i64>,
    /// Optional placement guard; default = the card's current column.
    #[serde(default)]
    pub expected_column: Option<String>,
}

/// Agent `block <N>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBlockBoardCardRequest {
    pub reason: String,
}

/// Agent `review <N> <department>`: [`BoardReviewRequest`] on stdin.
/// Agent `link <N> [--role]`: links the caller's own conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLinkBoardCardRequest {
    /// `other` (default) | `build` | `qa`.
    #[serde(default)]
    pub role: Option<LinkRole>,
}

/// Agent `show <N>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentShowBoardCardQuery {
    /// Return the packet instead of the detail.
    #[serde(default)]
    pub packet: bool,
}

pub const BOARD_TOOLS_SCHEMA_VERSION: u32 = 1;

/// Error object of the agent-router envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardCliError {
    /// A `board_*` code (SPEC §5) or `runtime_auth_failed` /
    /// `schema_validation_failed` / `transport_unavailable`.
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardCliMeta {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// The agent-facing envelope: `success`, `data` or `error{code}`, `meta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardCliEnvelope<T> {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BoardCliError>,
    pub meta: BoardCliMeta,
}

impl<T> BoardCliEnvelope<T> {
    pub fn success(data: T, command: impl Into<String>) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta: BoardCliMeta {
                schema_version: BOARD_TOOLS_SCHEMA_VERSION,
                command: Some(command.into()),
            },
        }
    }

    pub fn failure(error: BoardCliError, command: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error),
            meta: BoardCliMeta {
                schema_version: BOARD_TOOLS_SCHEMA_VERSION,
                command: Some(command.into()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_enums_round_trip_their_db_strings() {
        for value in ColumnCategory::ALL {
            assert_eq!(ColumnCategory::parse(value.as_str()), Some(*value));
            assert_eq!(serde_json::to_value(value).unwrap(), json!(value.as_str()));
        }
        assert_eq!(CardType::parse("research"), Some(CardType::Research));
        assert_eq!(CardTier::parse("t4"), None);
    }

    #[test]
    fn patch_distinguishes_absent_from_explicit_null() {
        let absent: UpdateBoardCardRequest = serde_json::from_value(json!({ "version": 1 })).unwrap();
        assert_eq!(absent.size, None);
        let cleared: UpdateBoardCardRequest = serde_json::from_value(json!({ "version": 1, "size": null })).unwrap();
        assert_eq!(cleared.size, Some(None));
        let set: UpdateBoardCardRequest = serde_json::from_value(json!({ "version": 1, "size": "m" })).unwrap();
        assert_eq!(set.size, Some(Some(CardSize::M)));
    }

    #[test]
    fn requests_reject_unknown_fields() {
        assert!(serde_json::from_value::<CreateBoardCardRequest>(json!({ "title": "x", "number": 3 })).is_err());
        assert!(serde_json::from_value::<BoardEmptyRequest>(json!({ "x": 1 })).is_err());
        assert!(serde_json::from_value::<BoardEmptyRequest>(json!({})).is_ok());
        assert!(
            serde_json::from_value::<AgentUpdateBoardCardRequest>(json!({ "version": 1, "blocked": true })).is_err(),
            "agents block through the block command"
        );
    }

    #[test]
    fn envelope_failure_carries_the_code_and_command() {
        let envelope = BoardCliEnvelope::<Value>::failure(
            BoardCliError {
                code: "board_disabled".into(),
                message: "off".into(),
                details: None,
            },
            "board list",
        );
        let value = serde_json::to_value(envelope).unwrap();
        assert_eq!(value["success"], json!(false));
        assert_eq!(value["error"]["code"], json!("board_disabled"));
        assert_eq!(value["meta"]["command"], json!("board list"));
        assert!(value.get("data").is_none());
    }
}
