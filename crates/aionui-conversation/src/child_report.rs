//! Child → parent result reports (hierarchical conversations).
//!
//! When a child conversation's turn completes, the child's final assistant
//! text is delivered to its parent through the cross-session message path
//! (the [`ChildResultDelivery`] port), wrapped in an `[[AION_CHILD_RESULT]]`
//! block. Delivery is best-effort: it never fails the turn completion.
//!
//! Loop guard: a report only ever flows child → parent, and the parent is read
//! from the child's own row, never from the message. A parent that is itself a
//! child reports to ITS parent when its own turn ends, which walks strictly up
//! a finite, acyclic chain (cycles are rejected at create / reparent time).

use std::sync::Arc;

use aionui_common::{ChildResultDelivery, ErrorChain};
use aionui_db::models::MessageRow;
use aionui_db::{IConversationRepository, MessagePageDirection, MessagePageParams};
use tracing::{debug, warn};

use crate::hierarchy::{ChildResultStatus, format_child_result, reports_to_parent};

/// How many of the newest messages to scan for the final assistant text.
const SCAN_LIMIT: u32 = 200;

/// Body used when the turn produced no assistant text.
const NO_OUTPUT: &str = "(no assistant output)";

/// The last assistant text segment after the last user message, from messages
/// in ascending display order.
pub(crate) fn final_assistant_text(messages: &[MessageRow]) -> Option<String> {
    let after_user = messages
        .iter()
        .rposition(|m| m.position.as_deref() == Some("right") && !m.hidden)
        .map_or(0, |index| index + 1);
    messages[after_user..]
        .iter()
        .rev()
        .filter(|m| m.r#type == "text" && m.position.as_deref() != Some("right") && !m.hidden)
        .find_map(|m| {
            serde_json::from_str::<serde_json::Value>(&m.content)
                .ok()
                .and_then(|v| v.get("content").and_then(|c| c.as_str()).map(str::to_owned))
                .filter(|text| !text.trim().is_empty())
        })
}

/// Build and deliver the report for `conversation_id`, if it has a parent and
/// has not opted out. Every failure is logged at `warn` and swallowed.
pub(crate) async fn report_child_result(
    repo: Arc<dyn IConversationRepository>,
    delivery: Arc<dyn ChildResultDelivery>,
    user_id: String,
    conversation_id: String,
    status: ChildResultStatus,
) {
    let row = match repo.get(&user_id, &conversation_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(error) => {
            warn!(conversation_id = %conversation_id, error = %ErrorChain(&error), "child result: row lookup failed");
            return;
        }
    };
    let Some(parent_id) = row.parent_conversation_id.clone() else {
        return;
    };
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_default();
    if !reports_to_parent(&extra) {
        debug!(conversation_id = %conversation_id, "child result: report_to_parent=false, not reporting");
        return;
    }

    let params = MessagePageParams {
        limit: SCAN_LIMIT,
        direction: MessagePageDirection::InitialLatest,
    };
    let text = match repo.list_messages_page(&user_id, &conversation_id, &params).await {
        Ok(page) => final_assistant_text(&page.items),
        Err(error) => {
            warn!(conversation_id = %conversation_id, error = %ErrorChain(&error), "child result: message lookup failed");
            None
        }
    };
    let message = format_child_result(
        &conversation_id,
        &row.name,
        status,
        text.as_deref().unwrap_or(NO_OUTPUT),
    );
    match delivery
        .deliver_child_result(&user_id, &conversation_id, &parent_id, message)
        .await
    {
        Ok(()) => debug!(
            conversation_id = %conversation_id,
            parent_conversation_id = %parent_id,
            status = status.as_str(),
            "child result delivered to parent"
        ),
        Err(reason) => warn!(
            conversation_id = %conversation_id,
            parent_conversation_id = %parent_id,
            error = %reason,
            "child result delivery failed; turn completion unaffected"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(kind: &str, position: &str, text: &str, hidden: bool) -> MessageRow {
        MessageRow {
            id: format!("{kind}-{position}-{text}"),
            conversation_id: "c".into(),
            msg_id: None,
            r#type: kind.into(),
            content: serde_json::json!({ "content": text }).to_string(),
            position: Some(position.into()),
            status: None,
            hidden,
            created_at: 0,
            backend_turn_id: None,
        }
    }

    #[test]
    fn final_text_is_the_last_assistant_segment_after_the_last_user_message() {
        let messages = vec![
            msg("text", "right", "old prompt", false),
            msg("text", "left", "old answer", false),
            msg("text", "right", "new prompt", false),
            msg("text", "left", "thinking out loud", false),
            msg("tool_group", "left", "ignored", false),
            msg("text", "left", "final answer", false),
        ];
        assert_eq!(final_assistant_text(&messages).as_deref(), Some("final answer"));
    }

    #[test]
    fn a_turn_without_assistant_text_has_no_final_text() {
        let messages = vec![
            msg("text", "left", "previous turn answer", false),
            msg("text", "right", "prompt", false),
        ];
        assert_eq!(final_assistant_text(&messages), None);
        assert_eq!(final_assistant_text(&[]), None);
    }
}
