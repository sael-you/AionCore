//! Provider prices and cost formulas (integer micro-USD).
//!
//! Prices from the OpenAI pricing page as recorded in S0-A
//! (`design/voice/s0/openai.md`, 2026-11-14), per 1M tokens.

/// Prices in micro-USD per 1M tokens (so `$32 / 1M` is `32_000_000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPricing {
    pub text_in: u64,
    pub text_cached: u64,
    pub text_out: u64,
    pub audio_in: u64,
    pub audio_cached: u64,
    pub audio_out: u64,
}

pub const PRICING_FULL: ModelPricing = ModelPricing {
    text_in: 4_000_000,
    text_cached: 400_000,
    text_out: 24_000_000,
    audio_in: 32_000_000,
    audio_cached: 400_000,
    audio_out: 64_000_000,
};

pub const PRICING_MINI: ModelPricing = ModelPricing {
    text_in: 600_000,
    text_cached: 60_000,
    text_out: 2_400_000,
    audio_in: 10_000_000,
    audio_cached: 300_000,
    audio_out: 20_000_000,
};

/// `gpt-transcribe` ≈ $0.07/h of input audio (S0-A estimate) → 20 µUSD/s.
pub const STT_MICROS_PER_SECOND: u64 = 20;

/// User audio is billed at 10 tokens per second (S0-A).
pub const USER_AUDIO_TOKENS_PER_SECOND: u64 = 10;

/// Pricing for a model id; unknown ids get the (more expensive) full model.
pub fn pricing_for(model: &str) -> ModelPricing {
    if model.ends_with("-mini") {
        PRICING_MINI
    } else {
        PRICING_FULL
    }
}

/// Token counts of one response, as reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub input_text: u64,
    pub input_audio: u64,
    pub cached_text: u64,
    pub cached_audio: u64,
    pub output_text: u64,
    pub output_audio: u64,
}

impl Usage {
    pub fn cached_tokens(&self) -> u64 {
        self.cached_text + self.cached_audio
    }
}

/// Cost of `tokens` at `price` µUSD/1M tokens, rounded up.
fn tokens_cost(tokens: u64, price: u64) -> u64 {
    (tokens.saturating_mul(price)).div_ceil(1_000_000)
}

/// Exact cost of a reported usage block.
pub fn usage_cost_micros(pricing: &ModelPricing, usage: &Usage) -> i64 {
    let uncached_text = usage.input_text.saturating_sub(usage.cached_text);
    let uncached_audio = usage.input_audio.saturating_sub(usage.cached_audio);
    let cost = tokens_cost(uncached_text, pricing.text_in)
        + tokens_cost(usage.cached_text, pricing.text_cached)
        + tokens_cost(uncached_audio, pricing.audio_in)
        + tokens_cost(usage.cached_audio, pricing.audio_cached)
        + tokens_cost(usage.output_text, pricing.text_out)
        + tokens_cost(usage.output_audio, pricing.audio_out);
    i64::try_from(cost).unwrap_or(i64::MAX)
}

/// What a response may cost at most, reserved before it runs (§3.5):
/// `max_output_tokens × output rate + (context + audio since the last
/// response) × uncached input rate + STT of the turn`. Audio rates are used
/// for both because they are the higher ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationInput {
    pub max_output_tokens: u64,
    pub context_tokens: u64,
    pub audio_tokens_since_last: u64,
    pub stt_seconds: u64,
}

pub fn reservation_micros(pricing: &ModelPricing, input: &ReservationInput) -> i64 {
    let cost = tokens_cost(input.max_output_tokens, pricing.audio_out)
        + tokens_cost(
            input.context_tokens.saturating_add(input.audio_tokens_since_last),
            pricing.audio_in,
        )
        + input.stt_seconds.saturating_mul(STT_MICROS_PER_SECOND);
    i64::try_from(cost).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_cost_matches_captured_response() {
        // response.done usage captured live in S0-A on gpt-realtime-2.1-mini:
        // 356 text in, 0 cached, 90 text out, 73 audio out.
        let usage = Usage {
            input_tokens: 356,
            output_tokens: 163,
            input_text: 356,
            output_text: 90,
            output_audio: 73,
            ..Default::default()
        };
        // 356*0.6 + 90*2.4 + 73*20 = 213.6 + 216 + 1460 → 214 + 216 + 1460
        assert_eq!(usage_cost_micros(&PRICING_MINI, &usage), 1890);
    }

    #[test]
    fn cached_tokens_are_billed_at_cached_rate() {
        let usage = Usage {
            input_text: 1_000_000,
            cached_text: 1_000_000,
            ..Default::default()
        };
        assert_eq!(usage_cost_micros(&PRICING_FULL, &usage), 400_000);
    }

    #[test]
    fn reservation_follows_plan_formula() {
        let input = ReservationInput {
            max_output_tokens: 600,
            context_tokens: 2_000,
            audio_tokens_since_last: 50,
            stt_seconds: 5,
        };
        // 600*64 + 2050*32 + 5*20 = 38_400 + 65_600 + 100
        assert_eq!(reservation_micros(&PRICING_FULL, &input), 104_100);
        assert!(reservation_micros(&PRICING_MINI, &input) < 104_100);
    }

    #[test]
    fn unknown_models_are_priced_as_full() {
        assert_eq!(pricing_for("gpt-realtime-3"), PRICING_FULL);
        assert_eq!(pricing_for("gpt-realtime-2.1-mini"), PRICING_MINI);
    }
}
