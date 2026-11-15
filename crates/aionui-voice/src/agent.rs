//! Agent → user (§3.4): `say` speaks a line on the user's armed device,
//! `ask` speaks a question and returns the user's transcript. An `ask`
//! answer is never an approval. `ask` blocks at most `min(timeout, 50 s)`,
//! then returns `pending`; `wait` continues.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Longest a single `ask`/`wait` call blocks.
pub const MAX_WAIT: Duration = Duration::from_secs(50);
/// `say` lines per minute per conversation.
pub const SAY_PER_MINUTE: u32 = 6;
/// An unanswered ask expires after this long.
pub const ASK_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskStatus {
    Pending,
    Answered(String),
    Cancelled,
    Expired,
}

#[derive(Debug)]
struct AskItem {
    user_id: String,
    conversation_id: String,
    status: AskStatus,
    created: Instant,
    notify: Arc<Notify>,
}

/// Pending asks, answered by the user's next utterance in a live call.
#[derive(Default)]
pub struct AskRegistry {
    items: DashMap<String, AskItem>,
    queue: DashMap<String, VecDeque<String>>,
}

impl AskRegistry {
    pub fn create(&self, user_id: &str, conversation_id: &str) -> String {
        self.expire();
        let id = aionui_common::generate_prefixed_id("ask");
        self.items.insert(
            id.clone(),
            AskItem {
                user_id: user_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
                status: AskStatus::Pending,
                created: Instant::now(),
                notify: Arc::new(Notify::new()),
            },
        );
        self.queue.entry(user_id.to_owned()).or_default().push_back(id.clone());
        id
    }

    fn expire(&self) {
        for mut item in self.items.iter_mut() {
            if item.status == AskStatus::Pending && item.created.elapsed() >= ASK_TTL {
                item.status = AskStatus::Expired;
                item.notify.notify_waiters();
            }
        }
        self.items.retain(|_, item| item.created.elapsed() < ASK_TTL * 2);
    }

    /// Whether the user has an unanswered ask (the model may not act on the
    /// answer utterance).
    pub fn has_pending(&self, user_id: &str) -> bool {
        self.queue
            .get(user_id)
            .is_some_and(|q| q.iter().any(|id| self.is_pending(id)))
    }

    fn is_pending(&self, id: &str) -> bool {
        self.items.get(id).is_some_and(|i| i.status == AskStatus::Pending)
    }

    /// Answer the oldest pending ask of the user with `transcript`.
    pub fn answer_next(&self, user_id: &str, transcript: &str) -> Option<String> {
        let mut queue = self.queue.get_mut(user_id)?;
        while let Some(id) = queue.pop_front() {
            if let Some(mut item) = self.items.get_mut(&id)
                && item.status == AskStatus::Pending
            {
                item.status = AskStatus::Answered(transcript.to_owned());
                item.notify.notify_waiters();
                return Some(id);
            }
        }
        None
    }

    pub fn cancel_user(&self, user_id: &str) {
        for mut item in self.items.iter_mut() {
            if item.user_id == user_id && item.status == AskStatus::Pending {
                item.status = AskStatus::Cancelled;
                item.notify.notify_waiters();
            }
        }
    }

    /// Wait up to `timeout` (capped at [`MAX_WAIT`]) for an answer. `None`
    /// when the ask does not exist for this caller.
    pub async fn wait(&self, user_id: &str, conversation_id: &str, id: &str, timeout: Duration) -> Option<AskStatus> {
        let deadline = Instant::now() + timeout.min(MAX_WAIT);
        loop {
            let notify = {
                let item = self.items.get(id)?;
                if item.user_id != user_id || item.conversation_id != conversation_id {
                    return None;
                }
                if item.status != AskStatus::Pending {
                    return Some(item.status.clone());
                }
                item.notify.clone()
            };
            let notified = notify.notified();
            // Re-check after registering, so an answer in between is not missed.
            if !self.is_pending(id) {
                continue;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Some(AskStatus::Pending);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ask_is_answered_by_the_next_utterance() {
        let registry = Arc::new(AskRegistry::default());
        let id = registry.create("u1", "c1");
        assert!(registry.has_pending("u1"));
        let waiter = {
            let registry = registry.clone();
            let id = id.clone();
            tokio::spawn(async move { registry.wait("u1", "c1", &id, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(registry.answer_next("u1", "the blue one").as_deref(), Some(id.as_str()));
        assert_eq!(waiter.await.unwrap(), Some(AskStatus::Answered("the blue one".into())));
        assert!(!registry.has_pending("u1"));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_times_out_as_pending_and_is_caller_bound() {
        let registry = AskRegistry::default();
        let id = registry.create("u1", "c1");
        assert_eq!(
            registry.wait("u1", "c1", &id, Duration::from_secs(1)).await,
            Some(AskStatus::Pending)
        );
        assert_eq!(
            registry.wait("u2", "c1", &id, Duration::from_secs(1)).await,
            None,
            "other user"
        );
        assert_eq!(
            registry.wait("u1", "c2", &id, Duration::from_secs(1)).await,
            None,
            "other conversation"
        );
        registry.cancel_user("u1");
        assert_eq!(
            registry.wait("u1", "c1", &id, Duration::from_secs(1)).await,
            Some(AskStatus::Cancelled)
        );
    }
}
