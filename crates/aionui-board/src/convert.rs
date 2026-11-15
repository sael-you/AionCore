//! Row ↔ DTO conversion. Enum columns are guarded by SQL CHECKs; a value the
//! binary does not know (a row written by a newer build) falls back to the
//! column default instead of failing the whole read.

use aionui_api_types::{
    AgentMove, AssigneeKind, BoardActorKind, BoardCardDto, BoardCardSummaryDto, BoardColumnDto, BoardDependencyDto,
    BoardEventDto, BoardLinkDto, BoardLinkStatus, BoardQuestionDto, BoardReviewDto, BoardSettingsDto, BoardTemplate,
    CardPriority, CardSize, CardTier, CardType, ColumnCategory, ColumnKind, ColumnResolution, LinkRole,
};
use aionui_db::{
    BoardCardRow, BoardColumnRow, BoardEventRow, BoardLinkViewRow, BoardQuestionRow, BoardReviewRow, BoardSettingsRow,
};
use serde_json::{Value, json};

/// `TASK-<n>`.
pub fn reference(number: i64) -> String {
    format!("TASK-{number}")
}

fn json_or(raw: &str, fallback: Value) -> Value {
    serde_json::from_str(raw).unwrap_or(fallback)
}

fn string_list(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

pub fn settings_dto(project_id: &str, row: Option<&BoardSettingsRow>) -> BoardSettingsDto {
    match row {
        Some(row) => BoardSettingsDto {
            project_id: row.project_id.clone(),
            exists: true,
            enabled: row.enabled,
            template: BoardTemplate::parse(&row.template).unwrap_or(BoardTemplate::Studio),
            next_number: row.next_number,
            version: row.version,
            settings: json_or(&row.settings, json!({})),
            archived_at: row.archived_at,
            keeper_conversation_id: row.keeper_conversation_id.clone(),
            created_at: Some(row.created_at),
            updated_at: Some(row.updated_at),
        },
        None => BoardSettingsDto {
            project_id: project_id.to_owned(),
            exists: false,
            enabled: false,
            template: BoardTemplate::Studio,
            next_number: 1,
            version: 0,
            settings: json!({}),
            archived_at: None,
            keeper_conversation_id: None,
            created_at: None,
            updated_at: None,
        },
    }
}

pub fn column_dto(row: &BoardColumnRow, card_count: i64) -> BoardColumnDto {
    BoardColumnDto {
        key: row.key.clone(),
        name: row.name.clone(),
        category: ColumnCategory::parse(&row.category).unwrap_or(ColumnCategory::Backlog),
        kind: ColumnKind::parse(&row.kind).unwrap_or(ColumnKind::Normal),
        resolution: row.resolution.as_deref().and_then(ColumnResolution::parse),
        agent_move: AgentMove::parse(&row.agent_move).unwrap_or(AgentMove::Deny),
        wip_limit: row.wip_limit,
        exit_rule: row.exit_rule.as_deref().map(|raw| json_or(raw, Value::Null)),
        gate: row.gate.as_deref().map(|raw| json_or(raw, Value::Null)),
        position: row.position,
        archived: row.archived,
        card_count,
    }
}

/// One dependency target as seen from the waiting card.
pub fn dependency_dto(target: &BoardCardRow, columns: &[BoardColumnRow]) -> BoardDependencyDto {
    let resolution = columns
        .iter()
        .find(|column| column.key == target.column_key)
        .and_then(|column| column.resolution.as_deref());
    BoardDependencyDto {
        card_id: target.id.clone(),
        number: target.number,
        reference: reference(target.number),
        title: target.title.clone(),
        column_key: target.column_key.clone(),
        resolved: resolution == Some(ColumnResolution::Completed.as_str()),
        dropped: resolution == Some(ColumnResolution::Dropped.as_str()),
    }
}

/// Full card with its resolved dependencies.
pub fn card_dto(row: &BoardCardRow, dependencies: Vec<BoardDependencyDto>) -> BoardCardDto {
    let open_dependency_count = dependencies.iter().filter(|dep| !dep.resolved).count() as i64;
    BoardCardDto {
        id: row.id.clone(),
        project_id: row.project_id.clone(),
        number: row.number,
        reference: reference(row.number),
        title: row.title.clone(),
        description: row.description.clone(),
        card_type: CardType::parse(&row.card_type).unwrap_or(CardType::Feature),
        priority: CardPriority::parse(&row.priority).unwrap_or(CardPriority::Medium),
        size: row.size.as_deref().and_then(CardSize::parse),
        tier: row.tier.as_deref().and_then(CardTier::parse),
        column_key: row.column_key.clone(),
        position: row.position,
        labels: string_list(&row.labels),
        skills: string_list(&row.skills),
        routing: string_list(&row.routing),
        creator_kind: BoardActorKind::parse(&row.creator_kind).unwrap_or(BoardActorKind::User),
        creator_ref: row.creator_ref.clone(),
        assignee_kind: row.assignee_kind.as_deref().and_then(AssigneeKind::parse),
        assignee_ref: row.assignee_ref.clone(),
        blocked: row.blocked,
        blocked_reason: row.blocked_reason.clone(),
        blocked_by: row.blocked_by.as_deref().and_then(BoardActorKind::parse),
        change_id: row.change_id.clone(),
        metadata: json_or(&row.metadata, json!({})),
        dependencies,
        open_dependency_count,
        version: row.version,
        archived_at: row.archived_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Board-list form of a card. `blocked` is the effective R5 state (manual
/// flag or an unresolved dependency).
pub fn card_summary(
    row: &BoardCardRow,
    blocked: bool,
    open_dependency_count: i64,
    needs_decision: bool,
) -> BoardCardSummaryDto {
    let card = card_dto(row, Vec::new());
    BoardCardSummaryDto {
        id: card.id,
        number: card.number,
        reference: card.reference,
        title: card.title,
        card_type: card.card_type,
        priority: card.priority,
        size: card.size,
        tier: card.tier,
        column_key: card.column_key,
        position: card.position,
        labels: card.labels,
        skills: card.skills,
        creator_kind: card.creator_kind,
        creator_ref: card.creator_ref,
        assignee_kind: card.assignee_kind,
        assignee_ref: card.assignee_ref,
        assignee_name: None,
        blocked,
        open_dependency_count,
        needs_decision,
        version: card.version,
        archived_at: card.archived_at,
        updated_at: card.updated_at,
    }
}

pub fn event_dto(row: &BoardEventRow) -> BoardEventDto {
    BoardEventDto {
        seq: row.seq,
        prev_seq: row.prev_seq,
        id: row.id.clone(),
        card_id: row.card_id.clone(),
        project_id: row.project_id.clone(),
        actor_kind: BoardActorKind::parse(&row.actor_kind).unwrap_or(BoardActorKind::System),
        actor_ref: row.actor_ref.clone(),
        conversation_id: row.conversation_id.clone(),
        kind: row.kind.clone(),
        payload: json_or(&row.payload, json!({})),
        created_at: row.created_at,
    }
}

pub fn review_dto(row: &BoardReviewRow) -> BoardReviewDto {
    BoardReviewDto {
        id: row.id.clone(),
        department: row.department.clone(),
        reviewer_ref: row.reviewer_ref.clone(),
        conversation_id: row.conversation_id.clone(),
        verdict: row.verdict.clone(),
        summary: row.summary.clone(),
        findings: json_or(&row.findings, json!([])),
        recommendation: row.recommendation.clone(),
        assumptions: json_or(&row.assumptions, json!([])),
        confidence: row.confidence,
        tokens_used: row.tokens_used,
        version: row.version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

pub fn question_dto(row: &BoardQuestionRow) -> BoardQuestionDto {
    BoardQuestionDto {
        id: row.id.clone(),
        from_department: row.from_department.clone(),
        to_department: row.to_department.clone(),
        text: row.text.clone(),
        answered_by_event: row.answered_by_event.clone(),
        open: row.answered_by_event.is_none(),
        version: row.version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

pub fn link_dto(row: &BoardLinkViewRow) -> BoardLinkDto {
    let status = if !row.conversation_exists {
        BoardLinkStatus::Removed
    } else if row.conversation_archived_at.is_some() {
        BoardLinkStatus::Archived
    } else {
        BoardLinkStatus::Active
    };
    BoardLinkDto {
        card_id: row.card_id.clone(),
        conversation_id: row.conversation_id.clone(),
        role: LinkRole::parse(&row.role).unwrap_or(LinkRole::Other),
        department: row.department.clone(),
        created_at: row.created_at,
        status,
        conversation_name: row.conversation_name.clone(),
        conversation_status: if row.conversation_exists {
            row.conversation_status.clone()
        } else {
            Some("removed".to_owned())
        },
    }
}
