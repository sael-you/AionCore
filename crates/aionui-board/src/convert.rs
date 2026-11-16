//! Row ↔ DTO conversion. Enum columns are guarded by SQL CHECKs; a value the
//! binary does not know (a row written by a newer build) falls back to the
//! column default instead of failing the whole read.

use std::collections::HashMap;

use aionui_api_types::{
    AgentMove, AssigneeKind, BoardActorKind, BoardCardDto, BoardCardSummaryDto, BoardColumnDto, BoardDependencyDto,
    BoardEventDto, BoardLinkDto, BoardLinkStatus, BoardQuestionDto, BoardReviewDto, BoardSettingsDto, BoardTemplate,
    CardPriority, CardSize, CardTier, CardType, ColumnCategory, ColumnKind, ColumnResolution, LinkRole,
};
use aionui_db::{
    BoardCardCreator, BoardCardRow, BoardColumnRow, BoardEventRow, BoardLinkViewRow, BoardQuestionRow, BoardReviewRow,
    BoardSettingsRow,
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

/// `project_name` decides the default `agent_access` (TASK-116).
pub fn settings_dto(project_id: &str, row: Option<&BoardSettingsRow>, project_name: &str) -> BoardSettingsDto {
    match row {
        Some(row) => {
            let settings = json_or(&row.settings, json!({}));
            BoardSettingsDto {
                agent_access: crate::access::effective(&settings, project_name),
                project_id: row.project_id.clone(),
                exists: true,
                enabled: row.enabled,
                template: BoardTemplate::parse(&row.template).unwrap_or(BoardTemplate::Studio),
                next_number: row.next_number,
                version: row.version,
                settings,
                archived_at: row.archived_at,
                keeper_conversation_id: row.keeper_conversation_id.clone(),
                created_at: Some(row.created_at),
                updated_at: Some(row.updated_at),
            }
        }
        None => BoardSettingsDto {
            project_id: project_id.to_owned(),
            exists: false,
            enabled: false,
            template: BoardTemplate::Studio,
            next_number: 1,
            version: 0,
            settings: json!({}),
            agent_access: crate::access::default_for(project_name),
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
        creator_conversation_id: None,
        creator_name: None,
        // A conversation assignee (909) wins over the 901 columns.
        assignee_kind: match &row.assignee_conversation_id {
            Some(_) => Some(AssigneeKind::Conversation),
            None => row.assignee_kind.as_deref().and_then(AssigneeKind::parse),
        },
        assignee_ref: row
            .assignee_conversation_id
            .clone()
            .or_else(|| row.assignee_ref.clone()),
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

/// Creator conversations by card id (see [`IBoardStore::card_creators`]).
///
/// [`IBoardStore::card_creators`]: aionui_db::IBoardStore::card_creators
pub type CardCreators = HashMap<String, BoardCardCreator>;

/// Index [`BoardCardCreator`] rows by card id (the first `card_created` wins).
pub fn card_creators(rows: Vec<BoardCardCreator>) -> CardCreators {
    let mut creators = CardCreators::new();
    for row in rows {
        creators.entry(row.card_id.clone()).or_insert(row);
    }
    creators
}

/// Ids of the agent-created cards among `rows` (the only ones with a
/// creator conversation).
pub fn agent_card_ids<'a>(rows: impl IntoIterator<Item = &'a BoardCardRow>) -> Vec<String> {
    rows.into_iter()
        .filter(|row| row.creator_kind == BoardActorKind::Agent.as_str())
        .map(|row| row.id.clone())
        .collect()
}

/// Fill `creator_conversation_id` / `creator_name` from the card's creator
/// conversation, when known.
pub fn set_creator(
    card_id: &str,
    conversation_id: &mut Option<String>,
    name: &mut Option<String>,
    creators: &CardCreators,
) {
    if let Some(creator) = creators.get(card_id) {
        *conversation_id = Some(creator.conversation_id.clone());
        *name = creator
            .conversation_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
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
        creator_conversation_id: card.creator_conversation_id,
        creator_name: card.creator_name,
        assignee_kind: card.assignee_kind,
        assignee_ref: card.assignee_ref,
        assignee_name: None,
        blocked,
        open_dependency_count,
        needs_decision,
        version: card.version,
        archived_at: card.archived_at,
        updated_at: card.updated_at,
        metadata: summary_metadata(&card.metadata),
    }
}

/// `metadata` keys the board list carries (the Board Butler's progress line).
const SUMMARY_METADATA_KEYS: [&str; 3] = ["progress", "status_note", "updated_at"];

/// The non-null [`SUMMARY_METADATA_KEYS`] in `metadata`; `None` when there is
/// none, so the field is left out of the board JSON.
fn summary_metadata(metadata: &Value) -> Option<Value> {
    let source = metadata.as_object()?;
    let picked: serde_json::Map<String, Value> = SUMMARY_METADATA_KEYS
        .iter()
        .filter_map(|key| {
            source
                .get(*key)
                .filter(|value| !value.is_null())
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect();
    (!picked.is_empty()).then_some(Value::Object(picked))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(metadata: &str) -> BoardCardRow {
        BoardCardRow {
            id: "c1".into(),
            project_id: "p1".into(),
            user_id: "u1".into(),
            number: 7,
            title: "Card".into(),
            description: "long description".into(),
            card_type: "task".into(),
            priority: "medium".into(),
            size: None,
            tier: None,
            column_key: "todo".into(),
            position: 0,
            labels: "[]".into(),
            skills: "[]".into(),
            routing: "[]".into(),
            creator_kind: "user".into(),
            creator_ref: "u1".into(),
            assignee_kind: None,
            assignee_ref: None,
            assignee_conversation_id: None,
            blocked: false,
            blocked_reason: None,
            blocked_by: None,
            change_id: None,
            metadata: metadata.into(),
            version: 1,
            archived_at: None,
            created_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn summary_carries_only_the_progress_keys_of_metadata() {
        let raw = json!({
            "progress": 40,
            "status_note": "gates running",
            "updated_at": 1_700_000_000_000_i64,
            "packet": {"large": "x".repeat(64)},
            "secret_note": "keep out",
        });
        let summary = card_summary(&row(&raw.to_string()), false, 0, false);
        assert_eq!(
            summary.metadata,
            Some(json!({"progress": 40, "status_note": "gates running", "updated_at": 1_700_000_000_000_i64}))
        );
        let wire = serde_json::to_value(&summary).unwrap();
        assert_eq!(wire["metadata"].as_object().unwrap().len(), 3);
        assert!(wire["metadata"].get("packet").is_none());
        assert!(wire["metadata"].get("secret_note").is_none());
    }

    #[test]
    fn summary_keeps_a_partial_progress_set() {
        let summary = card_summary(&row(r#"{"progress":100,"other":1}"#), false, 0, false);
        assert_eq!(summary.metadata, Some(json!({"progress": 100})));
    }

    #[test]
    fn summary_omits_metadata_without_progress_keys() {
        for raw in ["{}", r#"{"other":1}"#, r#"{"progress":null}"#, "not json", "[1,2]"] {
            let summary = card_summary(&row(raw), false, 0, false);
            assert_eq!(summary.metadata, None, "metadata {raw}");
            let wire = serde_json::to_value(&summary).unwrap();
            assert!(wire.get("metadata").is_none(), "metadata {raw} serialized");
        }
    }

    #[test]
    fn summary_without_metadata_still_deserializes() {
        let mut wire = serde_json::to_value(card_summary(&row("{}"), false, 0, false)).unwrap();
        wire.as_object_mut().unwrap().remove("metadata");
        let back: BoardCardSummaryDto = serde_json::from_value(wire).unwrap();
        assert_eq!(back.metadata, None);
    }
}
