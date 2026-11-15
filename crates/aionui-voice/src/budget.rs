//! Per-call budget meter (§3.5): reservations before every response,
//! reconciliation against reported usage, unknown usage charged in full, and
//! the daily cap with its 80 % warning.

use std::collections::HashMap;

use aionui_db::VoiceUsageDelta;

use crate::pricing::{
    ModelPricing, ReservationInput, STT_MICROS_PER_SECOND, USER_AUDIO_TOKENS_PER_SECOND, Usage, reservation_micros,
    usage_cost_micros,
};

/// Output cap for voice turns (§3.5); also sent as the session's `max_output_tokens`.
pub const VOICE_MAX_OUTPUT_TOKENS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetRefused {
    pub needed_micros: i64,
    pub remaining_micros: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetThreshold {
    /// First crossing of 80 % of the daily limit.
    Warning,
    /// The limit is reached: hang up.
    Reached,
}

#[derive(Debug, Clone)]
pub struct BudgetMeter {
    pricing: ModelPricing,
    limit_micros: i64,
    /// Spent today by this user's other sessions.
    base_used_micros: i64,
    /// Charged to this session so far (reconciled usage + unknown charges).
    session_cost_micros: i64,
    /// Outstanding reservations by provider response id.
    reservations: HashMap<String, i64>,
    /// Reserved by a backend-issued `response.create` whose id is not known yet.
    pending_own: Vec<i64>,
    context_text_tokens: u64,
    context_audio_tokens: u64,
    audio_ms_since_last: u64,
    warned: bool,
}

impl BudgetMeter {
    /// `prefix_tokens` is the estimated size of the (text) instructions + tools prefix.
    pub fn new(pricing: ModelPricing, limit_micros: i64, base_used_micros: i64, prefix_tokens: u64) -> Self {
        Self {
            pricing,
            limit_micros,
            base_used_micros,
            session_cost_micros: 0,
            reservations: HashMap::new(),
            pending_own: Vec::new(),
            context_text_tokens: prefix_tokens,
            context_audio_tokens: 0,
            audio_ms_since_last: 0,
            warned: false,
        }
    }

    pub fn limit_micros(&self) -> i64 {
        self.limit_micros
    }

    pub fn session_cost_micros(&self) -> i64 {
        self.session_cost_micros
    }

    /// Spent + reserved today, across all of the user's sessions.
    pub fn used_micros(&self) -> i64 {
        self.base_used_micros
            + self.session_cost_micros
            + self.reservations.values().sum::<i64>()
            + self.pending_own.iter().sum::<i64>()
    }

    pub fn has_in_flight(&self) -> bool {
        !self.reservations.is_empty() || !self.pending_own.is_empty()
    }

    /// Reservation the next response would need.
    pub fn next_reservation_micros(&self) -> i64 {
        let audio_tokens = self.audio_ms_since_last * USER_AUDIO_TOKENS_PER_SECOND / 1000;
        reservation_micros(
            &self.pricing,
            &ReservationInput {
                max_output_tokens: VOICE_MAX_OUTPUT_TOKENS,
                context_text_tokens: self.context_text_tokens,
                context_audio_tokens: self.context_audio_tokens,
                audio_tokens_since_last: audio_tokens,
                stt_seconds: self.audio_ms_since_last.div_ceil(1000),
            },
        )
    }

    fn reserve(&self) -> Result<i64, BudgetRefused> {
        let needed = self.next_reservation_micros();
        let remaining = self.limit_micros - self.used_micros();
        if needed > remaining {
            return Err(BudgetRefused {
                needed_micros: needed,
                remaining_micros: remaining.max(0),
            });
        }
        Ok(needed)
    }

    /// Whether one more response fits today (admission check).
    pub fn can_afford_response(&self) -> bool {
        self.reserve().is_ok()
    }

    /// Reserve before a backend-issued `response.create`.
    pub fn reserve_own(&mut self) -> Result<(), BudgetRefused> {
        let amount = self.reserve()?;
        self.pending_own.push(amount);
        Ok(())
    }

    /// Drop the latest own reservation when its `response.create` could not be sent.
    pub fn release_own(&mut self) {
        self.pending_own.pop();
    }

    /// `response.created`: binds a pending own reservation, or reserves for a
    /// provider-initiated (`create_response: true`) response. A refusal means
    /// the response must be cancelled.
    pub fn on_response_created(&mut self, response_id: &str) -> Result<(), BudgetRefused> {
        if self.reservations.contains_key(response_id) {
            return Ok(());
        }
        let amount = match self.pending_own.pop() {
            Some(amount) => amount,
            None => self.reserve()?,
        };
        self.reservations.insert(response_id.to_owned(), amount);
        Ok(())
    }

    /// `response.done`: replace the reservation by the reported usage. Without
    /// usage the reservation is charged in full and counted as unknown.
    pub fn on_response_done(&mut self, response_id: &str, usage: Option<&Usage>) -> VoiceUsageDelta {
        let reserved = self.reservations.remove(response_id);
        let delta = match usage {
            Some(usage) => {
                self.context_text_tokens = usage.context_text();
                self.context_audio_tokens = usage.context_audio();
                VoiceUsageDelta {
                    input_tokens: to_i64(usage.input_tokens),
                    cached_input_tokens: to_i64(usage.cached_tokens()),
                    output_tokens: to_i64(usage.output_tokens),
                    responses: 1,
                    cost_micros: usage_cost_micros(&self.pricing, usage),
                    ..Default::default()
                }
            }
            None => VoiceUsageDelta {
                responses: 1,
                unknown_responses: 1,
                cost_micros: reserved.unwrap_or_else(|| self.next_reservation_micros()),
                ..Default::default()
            },
        };
        self.audio_ms_since_last = 0;
        self.session_cost_micros += delta.cost_micros;
        delta
    }

    /// User speech segment (from VAD start/stop) — feeds the next reservation.
    pub fn on_user_audio(&mut self, duration_ms: u64) {
        self.audio_ms_since_last = self.audio_ms_since_last.saturating_add(duration_ms);
    }

    /// Transcription usage (`{type: duration, seconds}`), charged at once.
    pub fn on_stt_seconds(&mut self, seconds: u64) -> VoiceUsageDelta {
        let cost = to_i64(seconds.saturating_mul(STT_MICROS_PER_SECOND));
        self.session_cost_micros += cost;
        VoiceUsageDelta {
            stt_seconds: to_i64(seconds),
            cost_micros: cost,
            ..Default::default()
        }
    }

    /// Lossy re-attach (§3.6.4): every in-flight response is unknown and is
    /// charged at its full reservation.
    pub fn charge_in_flight_as_unknown(&mut self) -> VoiceUsageDelta {
        let amounts: Vec<i64> = self
            .reservations
            .drain()
            .map(|(_, v)| v)
            .chain(self.pending_own.drain(..))
            .collect();
        self.charge_unknown(amounts)
    }

    /// Restart recovery: nothing is known about the time the backend was down,
    /// so one full reservation is charged as unknown (conservative).
    pub fn charge_restart_gap(&mut self) -> VoiceUsageDelta {
        let amount = self.next_reservation_micros();
        self.charge_unknown(vec![amount])
    }

    fn charge_unknown(&mut self, amounts: Vec<i64>) -> VoiceUsageDelta {
        let count = amounts.len() as i64;
        let cost: i64 = amounts.into_iter().sum();
        self.session_cost_micros += cost;
        VoiceUsageDelta {
            responses: count,
            unknown_responses: count,
            cost_micros: cost,
            ..Default::default()
        }
    }

    /// Threshold crossed since the last call (the warning fires once).
    pub fn threshold(&mut self) -> Option<BudgetThreshold> {
        let used = self.used_micros();
        if used >= self.limit_micros {
            return Some(BudgetThreshold::Reached);
        }
        if !self.warned && used.saturating_mul(10) >= self.limit_micros.saturating_mul(8) {
            self.warned = true;
            return Some(BudgetThreshold::Warning);
        }
        None
    }
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Start of the current UTC day in epoch milliseconds (daily budget window).
pub fn utc_day_start_ms(now_ms: i64) -> i64 {
    now_ms - now_ms.rem_euclid(86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::PRICING_FULL;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            input_text: input,
            output_audio: output,
            ..Default::default()
        }
    }

    #[test]
    fn reservation_is_held_until_done_then_reconciled() {
        let mut meter = BudgetMeter::new(PRICING_FULL, 5_000_000, 0, 1_000);
        let reserve = meter.next_reservation_micros();
        meter.on_response_created("r1").unwrap();
        assert_eq!(meter.used_micros(), reserve);
        assert!(meter.has_in_flight());

        let delta = meter.on_response_done("r1", Some(&usage(1_000, 100)));
        assert_eq!(delta.responses, 1);
        assert_eq!(delta.unknown_responses, 0);
        assert_eq!(meter.used_micros(), delta.cost_micros);
        assert!(!meter.has_in_flight());
        assert!(delta.cost_micros < reserve, "actual usage below the worst case");
    }

    #[test]
    fn response_that_does_not_fit_is_refused() {
        // Limit below one reservation.
        let mut meter = BudgetMeter::new(PRICING_FULL, 10_000, 0, 1_000);
        assert!(!meter.can_afford_response());
        let refused = meter.on_response_created("r1").unwrap_err();
        assert!(refused.needed_micros > refused.remaining_micros);
        assert!(meter.reserve_own().is_err());
        assert_eq!(meter.used_micros(), 0, "a refusal reserves nothing");
    }

    #[test]
    fn other_sessions_today_count_against_the_cap() {
        let meter = BudgetMeter::new(PRICING_FULL, 5_000_000, 4_990_000, 1_000);
        assert!(!meter.can_afford_response());
    }

    #[test]
    fn own_reservation_binds_to_the_created_response() {
        let mut meter = BudgetMeter::new(PRICING_FULL, 5_000_000, 0, 1_000);
        meter.reserve_own().unwrap();
        let used = meter.used_micros();
        meter.on_response_created("r1").unwrap();
        assert_eq!(meter.used_micros(), used, "no double reservation");
    }

    #[test]
    fn unknown_usage_is_charged_at_full_reservation() {
        let mut meter = BudgetMeter::new(PRICING_FULL, 5_000_000, 0, 1_000);
        meter.on_response_created("r1").unwrap();
        meter.on_response_created("r2").unwrap();
        let held = meter.used_micros();
        let delta = meter.charge_in_flight_as_unknown();
        assert_eq!(delta.unknown_responses, 2);
        assert_eq!(delta.cost_micros, held);
        assert_eq!(meter.session_cost_micros(), held);
        assert!(!meter.has_in_flight());

        // A done without usage is unknown too.
        meter.on_response_created("r3").unwrap();
        let delta = meter.on_response_done("r3", None);
        assert_eq!(delta.unknown_responses, 1);
        assert!(delta.cost_micros > 0);
    }

    #[test]
    fn audio_since_last_response_raises_the_reservation() {
        let mut meter = BudgetMeter::new(PRICING_FULL, 5_000_000, 0, 1_000);
        let before = meter.next_reservation_micros();
        meter.on_user_audio(10_000);
        assert!(meter.next_reservation_micros() > before);
        meter.on_response_created("r1").unwrap();
        meter.on_response_done("r1", Some(&usage(10, 10)));
        assert!(
            meter.next_reservation_micros() < before,
            "audio counter resets, context shrinks"
        );
    }

    #[test]
    fn warning_fires_once_then_reached() {
        let mut meter = BudgetMeter::new(PRICING_FULL, 1_000_000, 790_000, 0);
        assert_eq!(meter.threshold(), None);
        meter.on_stt_seconds(500); // +10_000 → 80 %
        assert_eq!(meter.threshold(), Some(BudgetThreshold::Warning));
        assert_eq!(meter.threshold(), None);
        meter.on_stt_seconds(10_500); // +210_000 → 101 %
        assert_eq!(meter.threshold(), Some(BudgetThreshold::Reached));
    }

    #[test]
    fn utc_day_start_truncates_to_midnight() {
        assert_eq!(utc_day_start_ms(86_400_000 * 3 + 5), 86_400_000 * 3);
        assert_eq!(utc_day_start_ms(0), 0);
    }
}
