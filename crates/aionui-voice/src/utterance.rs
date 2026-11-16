//! Conversation mode: one spoken thought = one message (TASK-112).
//!
//! The provider's end-of-turn detection (`semantic_vad`, eagerness `low`, the
//! most tolerant setting the Realtime API documents) still cuts thinking pauses
//! into separate finals ("So." · "When I'm not." · "And for."), and each posted
//! final started its own agent turn. A final is therefore held for a short
//! window; finals that arrive meanwhile are appended, and the merged text is
//! posted once. Appending to an already posted message is impossible (it has
//! started a turn), so holding before the post is the simplest correct option.
//! Controls ("stop the task", "stop voice") are never held.

use std::time::Duration;

use tokio::time::Instant;

/// How long a final waits for the next one before it is posted.
pub const MERGE_WINDOW: Duration = Duration::from_secs(5);
/// A held message is posted at the latest this long after its first final,
/// even while the user keeps speaking (or VAD hears only noise).
pub const MERGE_CAP: Duration = Duration::from_secs(20);

/// Finals held for one conversation, not posted yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingUtterance {
    pub conversation_id: String,
    text: String,
    first_at: Instant,
    last_at: Instant,
    user_speaking: bool,
    fragments: usize,
}

impl PendingUtterance {
    pub(crate) fn new(conversation_id: &str, text: &str, now: Instant) -> Self {
        Self {
            conversation_id: conversation_id.to_owned(),
            text: text.trim().to_owned(),
            first_at: now,
            last_at: now,
            user_speaking: false,
            fragments: 1,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Append the next final of the same conversation.
    pub(crate) fn append(&mut self, text: &str, now: Instant) {
        let text = text.trim();
        if !text.is_empty() {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(text);
        }
        self.last_at = now;
        self.user_speaking = false;
        self.fragments += 1;
    }

    /// The user speaks again: the post waits for that final (up to the cap).
    pub(crate) fn speech_started(&mut self) {
        self.user_speaking = true;
    }

    /// Speech ended without a final yet: the window restarts from here.
    pub(crate) fn speech_stopped(&mut self, now: Instant) {
        self.user_speaking = false;
        self.last_at = now;
    }

    pub(crate) fn user_speaking(&self) -> bool {
        self.user_speaking
    }

    /// Number of finals merged so far (diagnostics).
    pub(crate) fn fragments(&self) -> usize {
        self.fragments
    }

    /// When to post.
    pub(crate) fn deadline(&self, window: Duration, cap: Duration) -> Instant {
        let capped = self.first_at + cap;
        if self.user_speaking {
            capped
        } else {
            (self.last_at + window).min(capped)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = MERGE_WINDOW;
    const C: Duration = MERGE_CAP;

    #[test]
    fn fragments_are_joined_and_each_restarts_the_window() {
        let t0 = Instant::now();
        let mut p = PendingUtterance::new("c1", " So. ", t0);
        assert_eq!(p.deadline(W, C), t0 + W);
        p.append("When I'm not.", t0 + Duration::from_secs(2));
        p.append("And for the tests, use bun.", t0 + Duration::from_secs(4));
        assert_eq!(p.text(), "So. When I'm not. And for the tests, use bun.");
        assert_eq!(p.deadline(W, C), t0 + Duration::from_secs(4) + W);
    }

    #[test]
    fn speaking_again_holds_the_post_until_the_cap() {
        let t0 = Instant::now();
        let mut p = PendingUtterance::new("c1", "So.", t0);
        p.speech_started();
        assert_eq!(p.deadline(W, C), t0 + C);
        // Speech without a final (noise): the window restarts at its end.
        p.speech_stopped(t0 + Duration::from_secs(3));
        assert_eq!(p.deadline(W, C), t0 + Duration::from_secs(3) + W);
    }

    #[test]
    fn the_defaults_give_the_owner_time() {
        // Owner, 2026-11-16: "give a bit more time" (≈ 5 s to go on).
        assert_eq!(MERGE_WINDOW, Duration::from_secs(5));
        assert_eq!(MERGE_CAP, Duration::from_secs(20));
        let defaults = crate::lifecycle::VoiceTimings::default();
        assert_eq!(defaults.utterance_window, MERGE_WINDOW);
        assert!(defaults.cascade_answer_silence >= Duration::from_millis(1500));
    }

    #[test]
    fn the_cap_bounds_a_long_chain_of_fragments() {
        let t0 = Instant::now();
        let mut p = PendingUtterance::new("c1", "a", t0);
        p.append("b", t0 + Duration::from_secs(19));
        assert_eq!(p.deadline(W, C), t0 + C);
    }
}
