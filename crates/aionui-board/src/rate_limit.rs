//! Per-conversation write budget of the agent router (R9): 20 creates and
//! 120 other writes per rolling minute → `429 board_rate_limited`.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const CREATES_PER_MINUTE: usize = 20;
pub const WRITES_PER_MINUTE: usize = 120;
const WINDOW: Duration = Duration::from_secs(60);

/// Which budget a write draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteBudget {
    Create,
    Write,
}

impl WriteBudget {
    fn limit(self) -> usize {
        match self {
            Self::Create => CREATES_PER_MINUTE,
            Self::Write => WRITES_PER_MINUTE,
        }
    }
}

/// Sliding-window limiter keyed by `(conversation, budget)`.
#[derive(Debug, Default)]
pub struct AgentRateLimiter {
    windows: Mutex<HashMap<(String, WriteBudget), VecDeque<Instant>>>,
}

impl AgentRateLimiter {
    /// Record one attempt at `now`; `false` when the budget is exhausted
    /// (the attempt is not recorded then).
    pub fn try_acquire_at(&self, conversation_id: &str, budget: WriteBudget, now: Instant) -> bool {
        let mut windows = self.windows.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Drop idle windows so the map does not grow with dead conversations.
        windows.retain(|_, hits| hits.back().is_some_and(|last| now.duration_since(*last) < WINDOW));
        let hits = windows.entry((conversation_id.to_owned(), budget)).or_default();
        while hits.front().is_some_and(|first| now.duration_since(*first) >= WINDOW) {
            hits.pop_front();
        }
        if hits.len() >= budget.limit() {
            return false;
        }
        hits.push_back(now);
        true
    }

    pub fn try_acquire(&self, conversation_id: &str, budget: WriteBudget) -> bool {
        self.try_acquire_at(conversation_id, budget, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_are_capped_per_conversation_and_window() {
        let limiter = AgentRateLimiter::default();
        let start = Instant::now();
        for _ in 0..CREATES_PER_MINUTE {
            assert!(limiter.try_acquire_at("c1", WriteBudget::Create, start));
        }
        assert!(!limiter.try_acquire_at("c1", WriteBudget::Create, start));
        assert!(
            limiter.try_acquire_at("c2", WriteBudget::Create, start),
            "per conversation"
        );
        assert!(
            limiter.try_acquire_at("c1", WriteBudget::Write, start),
            "separate budgets"
        );
        assert!(
            limiter.try_acquire_at("c1", WriteBudget::Create, start + WINDOW),
            "window slides"
        );
    }

    #[test]
    fn writes_allow_one_hundred_twenty_per_minute() {
        let limiter = AgentRateLimiter::default();
        let start = Instant::now();
        for i in 0..WRITES_PER_MINUTE {
            assert!(limiter.try_acquire_at("c1", WriteBudget::Write, start + Duration::from_millis(i as u64)));
        }
        assert!(!limiter.try_acquire_at("c1", WriteBudget::Write, start + Duration::from_secs(1)));
    }
}
