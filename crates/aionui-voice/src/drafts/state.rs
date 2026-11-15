//! Draft states and the allowed transitions (§3.3.1). Every persisted
//! transition is a versioned compare-and-set `… WHERE id=? AND version=? AND
//! state IN (from…)`; this table is the single list of legal `from` states.

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DraftState {
    Drafted,
    Ambiguous,
    PendingTranscript,
    Confirmed,
    Committing,
    Sending,
    Queued,
    Sent,
    Failed,
    SavedNotStarted,
    Cancelled,
    Expired,
    Interrupted,
    Superseded,
}

impl DraftState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drafted => "drafted",
            Self::Ambiguous => "ambiguous",
            Self::PendingTranscript => "pending_transcript",
            Self::Confirmed => "confirmed",
            Self::Committing => "committing",
            Self::Sending => "sending",
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::SavedNotStarted => "saved_not_started",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Interrupted => "interrupted",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        ALL.iter().copied().find(|s| s.as_str() == value)
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Sent
                | Self::Failed
                | Self::SavedNotStarted
                | Self::Cancelled
                | Self::Expired
                | Self::Interrupted
                | Self::Superseded
        )
    }

    /// States from which a cancel wins (everything before `sending`, plus queued).
    pub fn cancellable(self) -> bool {
        matches!(
            self,
            Self::Drafted
                | Self::Ambiguous
                | Self::PendingTranscript
                | Self::Confirmed
                | Self::Committing
                | Self::Queued
        )
    }

    /// Legal predecessors of `self`.
    pub fn allowed_from(self) -> &'static [DraftState] {
        use DraftState::*;
        match self {
            Drafted => &[Ambiguous, PendingTranscript],
            Ambiguous => &[Drafted],
            PendingTranscript => &[Drafted],
            Confirmed => &[Drafted, PendingTranscript],
            Committing => &[Confirmed],
            Sending => &[Committing, Queued, Sending],
            Queued => &[Sending],
            Sent | Failed | SavedNotStarted => &[Sending],
            Cancelled => &[Drafted, Ambiguous, PendingTranscript, Confirmed, Committing, Queued],
            Expired => &[Drafted, Ambiguous, PendingTranscript, Queued],
            Interrupted => &[Committing],
            Superseded => &[Drafted, Ambiguous, PendingTranscript],
        }
    }
}

const ALL: [DraftState; 14] = [
    DraftState::Drafted,
    DraftState::Ambiguous,
    DraftState::PendingTranscript,
    DraftState::Confirmed,
    DraftState::Committing,
    DraftState::Sending,
    DraftState::Queued,
    DraftState::Sent,
    DraftState::Failed,
    DraftState::SavedNotStarted,
    DraftState::Cancelled,
    DraftState::Expired,
    DraftState::Interrupted,
    DraftState::Superseded,
];

/// What a draft does when it is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftAction {
    SendMessage,
    CreateConversation,
}

impl DraftAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SendMessage => "send_message",
            Self::CreateConversation => "create_conversation",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "send_message" => Some(Self::SendMessage),
            "create_conversation" => Some(Self::CreateConversation),
            _ => None,
        }
    }
}

/// How the message will reach the target, read back truthfully (§3.3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    NewTurn,
    Steer,
    QueueBusy,
    QueueApproval,
    NewConversation,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewTurn => "new_turn",
            Self::Steer => "steer",
            Self::QueueBusy => "queue_busy",
            Self::QueueApproval => "queue_approval",
            Self::NewConversation => "new_conversation",
        }
    }
}

/// Hash over every frozen field: a tap carries it, so a tap on a stale card
/// can never confirm a different payload.
pub fn payload_hash(action: DraftAction, target_id: Option<&str>, project_id: Option<&str>, text: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [action.as_str(), target_id.unwrap_or(""), project_id.unwrap_or(""), text] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

/// Idempotency key of a draft version (`ConversationService::send_message_idempotent`).
pub fn idempotency_key(draft_id: &str, version: i64) -> String {
    format!("{draft_id}:{version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for state in ALL {
            assert_eq!(DraftState::parse(state.as_str()), Some(state));
        }
        assert_eq!(DraftState::parse("bogus"), None);
    }

    #[test]
    fn cancel_wins_before_sending_only() {
        assert!(DraftState::Committing.cancellable());
        assert!(DraftState::Queued.cancellable());
        assert!(!DraftState::Sending.cancellable());
        assert!(!DraftState::Sent.cancellable());
        assert!(!DraftState::Cancelled.allowed_from().contains(&DraftState::Sending));
    }

    #[test]
    fn only_confirmed_drafts_commit() {
        assert_eq!(DraftState::Committing.allowed_from(), &[DraftState::Confirmed]);
        assert!(!DraftState::Sending.allowed_from().contains(&DraftState::Drafted));
    }

    #[test]
    fn payload_hash_covers_every_field() {
        let base = payload_hash(DraftAction::SendMessage, Some("c1"), None, "hello");
        assert_ne!(base, payload_hash(DraftAction::SendMessage, Some("c2"), None, "hello"));
        assert_ne!(base, payload_hash(DraftAction::SendMessage, Some("c1"), None, "hello!"));
        assert_ne!(
            base,
            payload_hash(DraftAction::CreateConversation, Some("c1"), None, "hello")
        );
        assert_eq!(idempotency_key("d1", 3), "d1:3");
    }
}
