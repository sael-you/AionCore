//! Cross-crate lifecycle hook traits.
//!
//! Hooks defined here let lower-layer crates (e.g. `aionui-ai-agent`,
//! `aionui-cron`) react to events owned by higher-layer crates (e.g.
//! `aionui-conversation`) without forming a dependency cycle.

use async_trait::async_trait;

/// Notified before a conversation row is deleted via
/// `ConversationService::delete`.
///
/// Implementors are responsible for cleaning up their per-conversation state
/// (kill agent processes, drop cron job state, etc.). Hooks run sequentially
/// in registration order; failures must be logged inside the hook and not
/// propagated.
#[async_trait]
pub trait OnConversationDelete: Send + Sync {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str);
}

/// Why a turn was cancelled.
///
/// Passed to [`OnConversationTurnCancelled`] because "the user pressed stop" and
/// "we are recycling a wedged agent process" want opposite treatment, and a hook
/// cannot tell them apart from the conversation id alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnCancelCause {
    /// A user-initiated stop — the cancel route, or a runtime-facing adapter
    /// standing in for it.
    UserRequested,
    /// `restart_runtime` cancelling the active turn as a precondition for
    /// killing and rebuilding the agent process. NOT a request to abandon work
    /// aimed at this conversation: the user wants the conversation working
    /// again, and the restart leaves it idle — precisely the state a pending
    /// delivery has been waiting for.
    RuntimeRestart,
}

/// Notified when a conversation's turn was actually cancelled via
/// `ConversationService::cancel`.
///
/// Exists so an upper-layer crate (`aionui-session-message`) can drop the
/// pending deliveries aimed at that conversation without
/// `aionui-conversation` depending upwards. Without it, "stop" is a lie: the
/// user cancels A's turn, and a second later the drainer delivers B's queued
/// message to A, which starts a new turn — whack-a-mole the user cannot win.
///
/// Only fired on the branches where a cancel really took effect. A cancel whose
/// `turn_id` did not match the active turn cancelled nothing, and must NOT
/// clear the queue — doing so would silently drop messages, which is the worst
/// failure mode this feature has. Implementors must also honour `cause`: see
/// [`TurnCancelCause::RuntimeRestart`].
///
/// Hooks run sequentially in registration order; failures must be logged
/// inside the hook and not propagated.
#[async_trait]
pub trait OnConversationTurnCancelled: Send + Sync {
    async fn on_turn_cancelled(&self, user_id: &str, conversation_id: &str, turn_id: &str, cause: TurnCancelCause);
}

/// Delivers a finished child conversation's result report to its parent.
///
/// Hierarchical conversations: when a child's turn completes,
/// `aionui-conversation` builds the `[[AION_CHILD_RESULT ...]]` report and hands
/// it to this port, which an upper-layer crate (`aionui-session-message`)
/// implements on top of the cross-session message path — so the feature toggle
/// and rate limiting apply — without `aionui-conversation` depending upwards.
///
/// Delivery is best-effort: an `Err` is logged by the caller and never fails
/// the child's turn completion. The flow is strictly child → parent.
#[async_trait]
pub trait ChildResultDelivery: Send + Sync {
    async fn deliver_child_result(
        &self,
        user_id: &str,
        child_conversation_id: &str,
        parent_conversation_id: &str,
        message: String,
    ) -> Result<(), String>;
}
