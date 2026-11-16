//! Board event kinds (rows of `board_events`) and the `board.changed`
//! WebSocket broadcast (R8).
//!
//! Payload conventions (replayable, SPEC §3): card events carry
//! `{"card": <full card>, "version": n}`; settings/columns events carry their
//! after-state and `board_settings.version`; link/dependency events are set
//! operations — an add carries the full row, a remove carries the key.

use std::sync::Arc;

use aionui_api_types::{
    BOARD_CHANGED_EVENT, BoardCardDto, BoardChangedPayload, BoardEventDto, BoardReviewDto, WebSocketMessage,
};
use aionui_realtime::EventBroadcaster;
use tracing::warn;

pub const BOARD_CREATED: &str = "board_created";
pub const SETTINGS_CHANGED: &str = "settings_changed";
pub const COLUMNS_CHANGED: &str = "columns_changed";
pub const BOARD_ARCHIVED: &str = "board_archived";
pub const BOARD_UNARCHIVED: &str = "board_unarchived";
pub const CARD_CREATED: &str = "card_created";
pub const CARD_UPDATED: &str = "card_updated";
pub const CARD_MOVED: &str = "card_moved";
pub const CARD_ARCHIVED: &str = "card_archived";
pub const CARD_RESTORED: &str = "card_restored";
pub const CARD_BLOCKED: &str = "card_blocked";
pub const CARD_UNBLOCKED: &str = "card_unblocked";
pub const COMMENTED: &str = "commented";
pub const DEP_ADDED: &str = "dep_added";
pub const DEP_REMOVED: &str = "dep_removed";
pub const LINK_ADDED: &str = "link_added";
pub const LINK_REMOVED: &str = "link_removed";
pub const GATE_APPROVED: &str = "gate_approved";
pub const GATE_REJECTED: &str = "gate_rejected";
pub const GATE_FORCED: &str = "gate_forced";
/// An agent moved a card into, out of or past a gate under `agent_access`
/// (TASK-116); payload adds `gate`, `agent_access` and `approval`.
pub const GATE_CROSSED: &str = "gate_crossed";
pub const DEPLOY_FORCED: &str = "deploy_forced";
pub const REVIEW_UPDATED: &str = "review_updated";
pub const CARD_ASSIGNED: &str = "card_assigned";
pub const PACKET_SEND_FAILED: &str = "packet_send_failed";
pub const PACKET_SENT: &str = "packet_sent";
pub const LINK_PARENT_FALLBACK: &str = "link_parent_fallback";

/// Publishes committed board writes to the owner's WebSocket connections.
#[derive(Clone)]
pub struct BoardEventEmitter {
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl BoardEventEmitter {
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { broadcaster }
    }

    /// Broadcast one `board.changed` hint. Called after commit only.
    pub fn board_changed(
        &self,
        user_id: &str,
        event: &BoardEventDto,
        card: Option<&BoardCardDto>,
        review: Option<&BoardReviewDto>,
    ) {
        let payload = BoardChangedPayload {
            user_id: user_id.to_owned(),
            project_id: event.project_id.clone(),
            seq: event.seq,
            prev_seq: event.prev_seq,
            event: event.clone(),
            card: card.cloned(),
            review: review.cloned(),
        };
        match serde_json::to_value(&payload) {
            Ok(data) => self
                .broadcaster
                .broadcast(WebSocketMessage::new(BOARD_CHANGED_EVENT, data)),
            Err(error) => warn!(error = %error, "board.changed payload could not be serialized"),
        }
    }
}
