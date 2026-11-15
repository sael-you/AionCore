//! Spoken confirmation evidence (§3.3.4) and the 4 s hold with speech freeze
//! (§3.3.5). Pure state machines with injected time so every rule is tested.
//!
//! A draft becomes confirmed by voice only with ALL of:
//! 1. `voice.playbackFinished {readbackId, tEnd}` for that draft's clip;
//! 2. one client `voice.speechOnset {t}` with `t − tEnd ≥ 300 ms`, matched
//!    by a provider `speech_started` within 1.5 s, with no other onset between;
//! 3. that utterance's final transcript is exactly a confirm phrase.
//!
//! Anything else never authorises a send.

use std::time::Duration;

use tokio::time::Instant;

/// Minimum gap between the end of the read-back and the confirming speech.
pub const MIN_GAP_S: f64 = 0.3;
/// The provider must report speech within this long after the client onset.
pub const ONSET_MATCH_WINDOW: Duration = Duration::from_millis(1500);
/// Hold before sending (cancel window).
pub const HOLD: Duration = Duration::from_secs(4);
/// Longest freeze while waiting for an utterance's transcript.
pub const MAX_FREEZE: Duration = Duration::from_secs(5);
/// Time left after a non-cancel utterance during the hold.
pub const MIN_RESUME: Duration = Duration::from_secs(1);

const CONFIRM_PHRASES: &[&str] = &["send", "send it", "yes send", "go ahead"];
const NEGATIONS: &[&str] = &[
    "no", "dont", "don't", "wait", "but", "not", "stop", "cancel", "hold", "never",
];
const CANCEL_PHRASES: &[&str] = &[
    "cancel",
    "cancel it",
    "stop",
    "dont send",
    "don't send",
    "do not send",
    "no",
    "no wait",
    "wait",
    "hold on",
    "never mind",
    "nevermind",
];

/// Lowercase, drop punctuation (keep apostrophes), collapse whitespace.
pub fn normalize_utterance(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Exactly a confirm phrase (≤ 3 words), no negation or hesitation.
pub fn is_confirm_phrase(text: &str) -> bool {
    let normalized = normalize_utterance(text);
    let words: Vec<&str> = normalized.split(' ').filter(|w| !w.is_empty()).collect();
    if words.is_empty() || words.len() > 3 || words.iter().any(|w| NEGATIONS.contains(w)) {
        return false;
    }
    CONFIRM_PHRASES.contains(&normalized.as_str())
}

/// A spoken cancel during the read-back or the hold.
pub fn is_cancel_phrase(text: &str) -> bool {
    let normalized = normalize_utterance(text);
    CANCEL_PHRASES.contains(&normalized.as_str())
        || normalized.starts_with("cancel")
        || normalized.starts_with("don't send")
        || normalized.starts_with("dont send")
}

/// Whether text contains a confirm-grammar word (clip texts must not, §3.4).
pub fn contains_confirm_grammar(text: &str) -> bool {
    let normalized = normalize_utterance(text);
    let words: Vec<&str> = normalized.split(' ').collect();
    words.iter().any(|w| matches!(*w, "send" | "yes" | "confirm")) || normalized.contains("go ahead")
}

#[derive(Debug, Clone, PartialEq)]
pub enum OnsetVerdict {
    /// Recorded as the candidate confirming utterance.
    Candidate,
    /// Started less than 300 ms after the read-back: "say send again, or tap".
    TooEarly,
    /// No finished read-back to confirm (or a different clip): ignored.
    NotAwaiting,
    /// A second onset before the first resolved: evidence voided.
    Conflict,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptVerdict {
    /// All evidence present: the draft may move to `confirmed`.
    Confirmed,
    /// The utterance cancels the draft.
    Cancel,
    /// Evidence incomplete or not a confirm phrase: Jarvis asks for a tap.
    Rejected(&'static str),
    /// No confirm attempt in progress.
    Ignored,
}

/// Evidence for one draft version awaiting confirmation.
#[derive(Debug, Clone, Default)]
pub struct ConfirmTracker {
    readback_id: Option<String>,
    playback_end: Option<f64>,
    onset: Option<(f64, Instant)>,
    provider_started: bool,
    voided: bool,
    /// A clip is playing on the device: confirm matching is paused.
    clip_playing: bool,
}

impl ConfirmTracker {
    /// A new read-back clip was issued for the draft (resets all evidence).
    pub fn await_readback(&mut self, readback_id: &str) {
        *self = Self {
            readback_id: Some(readback_id.to_owned()),
            clip_playing: true,
            ..Self::default()
        };
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn awaiting(&self) -> Option<&str> {
        self.readback_id.as_deref()
    }

    /// Another clip (follow-up, say) started on the device: pause matching.
    pub fn clip_started(&mut self) {
        self.clip_playing = true;
        self.onset = None;
    }

    pub fn playback_finished(&mut self, readback_id: &str, t_end: f64) -> bool {
        if self.readback_id.as_deref() != Some(readback_id) || !t_end.is_finite() {
            return false;
        }
        self.playback_end = Some(t_end);
        self.clip_playing = false;
        true
    }

    pub fn client_onset(&mut self, t: f64, now: Instant) -> OnsetVerdict {
        let Some(t_end) = self.playback_end else {
            return OnsetVerdict::NotAwaiting;
        };
        if self.clip_playing {
            return OnsetVerdict::NotAwaiting;
        }
        if self.onset.is_some() {
            self.voided = true;
            return OnsetVerdict::Conflict;
        }
        if t - t_end < MIN_GAP_S {
            self.voided = true;
            return OnsetVerdict::TooEarly;
        }
        self.onset = Some((t, now));
        OnsetVerdict::Candidate
    }

    /// Provider/ASR `speech_started` (backend clock).
    pub fn provider_speech_started(&mut self, now: Instant) {
        if let Some((_, received)) = self.onset
            && now.saturating_duration_since(received) <= ONSET_MATCH_WINDOW
        {
            self.provider_started = true;
        }
    }

    /// Whether an utterance is being evaluated (→ `pending_transcript`).
    pub fn utterance_pending(&self) -> bool {
        self.onset.is_some()
    }

    /// The final transcript of the utterance (transcription model).
    pub fn final_transcript(&mut self, text: &str) -> TranscriptVerdict {
        if self.readback_id.is_none() {
            return TranscriptVerdict::Ignored;
        }
        if is_cancel_phrase(text) {
            return TranscriptVerdict::Cancel;
        }
        let verdict = if self.playback_end.is_none() {
            if is_confirm_phrase(text) {
                TranscriptVerdict::Rejected("read-back not finished")
            } else {
                TranscriptVerdict::Ignored
            }
        } else if self.voided {
            TranscriptVerdict::Rejected("ambiguous or early speech")
        } else if self.onset.is_none() {
            if is_confirm_phrase(text) {
                TranscriptVerdict::Rejected("no matching speech onset")
            } else {
                TranscriptVerdict::Ignored
            }
        } else if !self.provider_started {
            TranscriptVerdict::Rejected("speech onset not confirmed by the provider")
        } else if !is_confirm_phrase(text) {
            TranscriptVerdict::Rejected("not a confirm phrase")
        } else {
            TranscriptVerdict::Confirmed
        };
        // One utterance per attempt: the next one starts from a clean slate.
        self.onset = None;
        self.provider_started = false;
        self.voided = false;
        verdict
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldEvent {
    /// Keep holding.
    Holding,
    /// The deadline passed: send.
    Due,
}

/// The 4 s hold. Speech freezes the deadline until its transcript arrives
/// (max 5 s); a non-cancel utterance resumes with at least 1 s left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldClock {
    deadline: Instant,
    frozen_at: Option<Instant>,
}

impl HoldClock {
    pub fn start(now: Instant) -> Self {
        Self {
            deadline: now + HOLD,
            frozen_at: None,
        }
    }

    /// Resume a persisted hold (recovery keeps a deadline that is not yet due).
    pub fn with_deadline(deadline: Instant) -> Self {
        Self {
            deadline,
            frozen_at: None,
        }
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen_at.is_some()
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        match self.frozen_at {
            Some(frozen) => self.deadline.saturating_duration_since(frozen),
            None => self.deadline.saturating_duration_since(now),
        }
    }

    pub fn freeze(&mut self, now: Instant) {
        if self.frozen_at.is_none() && now < self.deadline {
            self.frozen_at = Some(now);
        }
    }

    /// The utterance was not a cancel: continue with ≥ 1 s left.
    pub fn resume(&mut self, now: Instant) {
        if let Some(frozen) = self.frozen_at.take() {
            let left = self.deadline.saturating_duration_since(frozen).max(MIN_RESUME);
            self.deadline = now + left;
        }
    }

    pub fn tick(&mut self, now: Instant) -> HoldEvent {
        if let Some(frozen) = self.frozen_at {
            if now.saturating_duration_since(frozen) >= MAX_FREEZE {
                self.resume(now);
            } else {
                return HoldEvent::Holding;
            }
        }
        if now >= self.deadline {
            HoldEvent::Due
        } else {
            HoldEvent::Holding
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(now: Instant) -> ConfirmTracker {
        let mut tracker = ConfirmTracker::default();
        tracker.await_readback("rb1");
        assert!(tracker.playback_finished("rb1", 10.0));
        let _ = now;
        tracker
    }

    #[test]
    fn confirm_grammar() {
        for yes in ["send", "Send it.", "yes, send", "go ahead"] {
            assert!(is_confirm_phrase(yes), "{yes}");
        }
        for no in [
            "no, wait",
            "don't send",
            "send it but change the title",
            "send it now please",
            "sent",
            "",
            "yes",
            "send not yet",
        ] {
            assert!(!is_confirm_phrase(no), "{no}");
        }
        assert!(is_cancel_phrase("Cancel!"));
        assert!(is_cancel_phrase("don't send that"));
        assert!(!is_cancel_phrase("send it"));
        assert!(contains_confirm_grammar("Kanban build says: yes, ship it"));
        assert!(!contains_confirm_grammar("Message for Kanban build: rerun the tests"));
    }

    #[test]
    fn full_evidence_confirms() {
        let now = Instant::now();
        let mut tracker = ready(now);
        assert_eq!(tracker.client_onset(10.5, now), OnsetVerdict::Candidate);
        assert!(tracker.utterance_pending());
        tracker.provider_speech_started(now + Duration::from_millis(400));
        assert_eq!(tracker.final_transcript("send it"), TranscriptVerdict::Confirmed);
    }

    #[test]
    fn send_said_too_early_is_rejected() {
        let now = Instant::now();
        let mut tracker = ready(now);
        assert_eq!(tracker.client_onset(10.2, now), OnsetVerdict::TooEarly);
        tracker.provider_speech_started(now);
        assert!(matches!(
            tracker.final_transcript("send"),
            TranscriptVerdict::Rejected(_)
        ));
    }

    #[test]
    fn missing_pieces_never_confirm() {
        let now = Instant::now();
        // Model or transcript alone: no read-back finished.
        let mut tracker = ConfirmTracker::default();
        tracker.await_readback("rb1");
        assert!(matches!(
            tracker.final_transcript("send"),
            TranscriptVerdict::Rejected(_)
        ));
        // Wrong clip id.
        assert!(!tracker.playback_finished("other", 10.0));
        // No provider speech within 1.5 s.
        let mut tracker = ready(now);
        tracker.client_onset(11.0, now);
        tracker.provider_speech_started(now + Duration::from_secs(2));
        assert!(matches!(
            tracker.final_transcript("send"),
            TranscriptVerdict::Rejected(_)
        ));
        // Two onsets.
        let mut tracker = ready(now);
        tracker.client_onset(11.0, now);
        assert_eq!(tracker.client_onset(11.4, now), OnsetVerdict::Conflict);
        tracker.provider_speech_started(now);
        assert!(matches!(
            tracker.final_transcript("send"),
            TranscriptVerdict::Rejected(_)
        ));
        // Not a confirm phrase.
        let mut tracker = ready(now);
        tracker.client_onset(11.0, now);
        tracker.provider_speech_started(now);
        assert!(matches!(
            tracker.final_transcript("send it to the other one"),
            TranscriptVerdict::Rejected(_)
        ));
        // A clip playing pauses matching.
        let mut tracker = ready(now);
        tracker.clip_started();
        assert_eq!(tracker.client_onset(11.0, now), OnsetVerdict::NotAwaiting);
    }

    #[test]
    fn spoken_cancel_is_recognised_any_time() {
        let mut tracker = ConfirmTracker::default();
        tracker.await_readback("rb1");
        assert_eq!(tracker.final_transcript("cancel"), TranscriptVerdict::Cancel);
    }

    #[test]
    fn hold_freezes_on_speech_and_resumes_with_a_floor() {
        let t0 = Instant::now();
        let mut hold = HoldClock::start(t0);
        assert_eq!(hold.tick(t0 + Duration::from_millis(3700)), HoldEvent::Holding);
        // "cancel" starts at 3.8 s: frozen past the original deadline.
        hold.freeze(t0 + Duration::from_millis(3800));
        assert_eq!(hold.tick(t0 + Duration::from_millis(4500)), HoldEvent::Holding);
        assert!(hold.is_frozen());
        // A non-cancel utterance resumes with at least 1 s.
        hold.resume(t0 + Duration::from_millis(4600));
        assert_eq!(hold.tick(t0 + Duration::from_millis(5000)), HoldEvent::Holding);
        assert_eq!(hold.tick(t0 + Duration::from_millis(5600)), HoldEvent::Due);
    }

    #[test]
    fn freeze_is_capped_at_five_seconds() {
        let t0 = Instant::now();
        let mut hold = HoldClock::start(t0);
        hold.freeze(t0 + Duration::from_secs(1));
        assert_eq!(hold.tick(t0 + Duration::from_millis(5900)), HoldEvent::Holding);
        // 5 s after the freeze: resumes with the 3 s that were left.
        assert_eq!(hold.tick(t0 + Duration::from_secs(6)), HoldEvent::Holding);
        assert_eq!(hold.tick(t0 + Duration::from_secs(9)), HoldEvent::Due);
    }
}
