//! Per-turn usage for the analytics ledger (card C42, migration 913).
//!
//! A `SessionEvent::UsageDelta` carries two different things: `total_tokens`
//! is context OCCUPANCY (never summed), while `input_tokens`,
//! `output_tokens` and the breakdown are the turn's own figures — those are
//! what the ledger records. `cost_usd` is SESSION-cumulative on the wire
//! (see `SessionEvent::UsageDelta`), so the turn's cost is the difference to
//! the previously stored cumulative value.

use aionui_db::UsageTurnParams;
use aionui_session::UsageBreakdown;

fn tokens(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// This turn's cost in micro-USD from the session-cumulative `cost_usd` and
/// the cumulative value stored before this report. A cumulative value below
/// the stored one means a new session started: the report is the turn's cost.
pub(crate) fn turn_cost_micro_usd(cost_usd: Option<f64>, previous_cost_usd: Option<f64>) -> Option<i64> {
    let cost = cost_usd.filter(|c| c.is_finite() && *c >= 0.0)?;
    let previous = previous_cost_usd.filter(|p| p.is_finite() && *p >= 0.0).unwrap_or(0.0);
    let delta = if cost >= previous { cost - previous } else { cost };
    // Rounded to whole micro-USD; f64 holds these magnitudes exactly enough.
    #[allow(clippy::cast_possible_truncation)]
    Some((delta * 1_000_000.0).round() as i64)
}

/// The ledger row of one usage report.
pub(crate) fn turn_params<'a>(
    user_id: &'a str,
    conversation_id: &'a str,
    (input_tokens, output_tokens): (u64, u64),
    breakdown: &UsageBreakdown,
    cost_usd: Option<f64>,
    previous_cost_usd: Option<f64>,
) -> UsageTurnParams<'a> {
    UsageTurnParams {
        user_id,
        conversation_id,
        input_tokens: tokens(input_tokens),
        output_tokens: tokens(output_tokens),
        cache_read_tokens: tokens(breakdown.cached_read_tokens),
        cache_write_tokens: tokens(breakdown.cached_write_tokens),
        thought_tokens: tokens(breakdown.thought_tokens),
        cost_micro_usd: turn_cost_micro_usd(cost_usd, previous_cost_usd),
        at: aionui_common::now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_is_the_difference_to_the_stored_cumulative_value() {
        assert_eq!(turn_cost_micro_usd(Some(1.25), Some(1.0)), Some(250_000));
        assert_eq!(
            turn_cost_micro_usd(Some(0.5), None),
            Some(500_000),
            "first report of a session"
        );
        assert_eq!(
            turn_cost_micro_usd(Some(0.2), Some(3.0)),
            Some(200_000),
            "a new session restarts the counter"
        );
        assert_eq!(turn_cost_micro_usd(Some(1.0), Some(1.0)), Some(0));
        assert_eq!(turn_cost_micro_usd(None, Some(1.0)), None, "codex reports no cost");
        assert_eq!(turn_cost_micro_usd(Some(f64::NAN), None), None);
    }

    #[test]
    fn row_takes_the_per_turn_split_not_occupancy() {
        let breakdown = UsageBreakdown {
            cached_read_tokens: 900,
            cached_write_tokens: 30,
            thought_tokens: 7,
        };
        let p = turn_params("u", "c", (12, 34), &breakdown, Some(2.0), Some(1.5));
        assert_eq!(
            (
                p.input_tokens,
                p.output_tokens,
                p.cache_read_tokens,
                p.cache_write_tokens,
                p.thought_tokens
            ),
            (12, 34, 900, 30, 7)
        );
        assert_eq!(p.cost_micro_usd, Some(500_000));
        assert!(!p.is_empty());
        // An occupancy-only ACP update has no per-turn figure: nothing to record.
        assert!(turn_params("u", "c", (0, 0), &UsageBreakdown::default(), None, None).is_empty());
    }
}
