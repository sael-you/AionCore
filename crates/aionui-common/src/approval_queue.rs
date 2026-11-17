//! A wait-for-the-owner queue: a caller parks a question with a payload, the
//! owner answers it later (or nobody does and it times out). Per user: only
//! the asking user's sessions may list or answer. Grants let one "yes" cover
//! a short burst of follow-up actions with the same key.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allowed,
    Denied,
    /// Nobody answered in time (or the question was dropped).
    TimedOut,
}

struct Pending<T> {
    user_id: String,
    payload: T,
    reply: oneshot::Sender<bool>,
}

pub struct ApprovalQueue<T> {
    pending: Mutex<HashMap<String, Pending<T>>>,
    grants: Mutex<HashMap<String, Instant>>,
}

impl<T> Default for ApprovalQueue<T> {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            grants: Mutex::new(HashMap::new()),
        }
    }
}

impl<T: Clone> ApprovalQueue<T> {
    /// Whether a recent "yes" for `grant_key` is still valid.
    pub fn granted(&self, grant_key: &str) -> bool {
        let mut grants = self.grants.lock().expect("grants lock");
        let now = Instant::now();
        grants.retain(|_, until| *until > now);
        grants.contains_key(grant_key)
    }

    pub fn grant(&self, grant_key: &str, ttl: Duration) {
        self.grants
            .lock()
            .expect("grants lock")
            .insert(grant_key.to_owned(), Instant::now() + ttl);
    }

    /// Park the question `id` and wait up to `timeout` for the answer. On a
    /// timeout the question is removed.
    pub async fn ask(&self, id: &str, user_id: &str, payload: T, timeout: Duration) -> Answer {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(
            id.to_owned(),
            Pending {
                user_id: user_id.to_owned(),
                payload,
                reply: tx,
            },
        );
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(true)) => Answer::Allowed,
            Ok(Ok(false)) => Answer::Denied,
            _ => {
                self.pending.lock().expect("pending lock").remove(id);
                Answer::TimedOut
            }
        }
    }

    /// The user's open questions.
    pub fn list(&self, user_id: &str) -> Vec<T> {
        self.pending
            .lock()
            .expect("pending lock")
            .values()
            .filter(|p| p.user_id == user_id)
            .map(|p| p.payload.clone())
            .collect()
    }

    /// Answer question `id` of `user_id`; its payload, or `None` when it is
    /// not pending (answered, expired, unknown or another user's).
    pub fn decide(&self, user_id: &str, id: &str, allow: bool) -> Option<T> {
        let pending = {
            let mut map = self.pending.lock().expect("pending lock");
            match map.get(id) {
                Some(p) if p.user_id == user_id => map.remove(id),
                _ => None,
            }
        }?;
        let _ = pending.reply.send(allow);
        Some(pending.payload)
    }

    /// Drop every open question matching `pred` (they time out for the asker).
    pub fn drop_where(&self, pred: impl Fn(&str, &T) -> bool) -> Vec<T> {
        let mut map = self.pending.lock().expect("pending lock");
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, p)| pred(&p.user_id, &p.payload))
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| map.remove(&id))
            .map(|p| p.payload)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn answers_are_per_user_once_and_time_out() {
        let q: Arc<ApprovalQueue<&'static str>> = Arc::new(ApprovalQueue::default());
        let q2 = q.clone();
        let waiting = tokio::spawn(async move { q2.ask("a1", "u1", "tap", Duration::from_secs(5)).await });
        while q.list("u1").is_empty() {
            tokio::task::yield_now().await;
        }
        assert!(q.list("u2").is_empty());
        assert_eq!(q.decide("u2", "a1", true), None, "another user cannot answer");
        assert_eq!(q.decide("u1", "a1", true), Some("tap"));
        assert_eq!(waiting.await.unwrap(), Answer::Allowed);
        assert_eq!(q.decide("u1", "a1", true), None, "answered once");
        assert_eq!(
            q.ask("a2", "u1", "x", Duration::from_millis(20)).await,
            Answer::TimedOut
        );
        assert!(q.list("u1").is_empty());
        assert!(!q.granted("k"));
        q.grant("k", Duration::from_secs(60));
        assert!(q.granted("k"));
        q.grant("gone", Duration::ZERO);
        assert!(!q.granted("gone"));
    }
}
