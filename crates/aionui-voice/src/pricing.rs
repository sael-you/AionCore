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

/// `MiniMax-M3` on the `priority` tier (≤ 512k input; MiniMax pricing page as
/// saved in S0-B, 2026-11-14: $0.45 in / $1.80 out / $0.09 cache read per 1M).
/// Text only: the audio rates equal the text rates so a usage block without
/// details is never undercharged.
pub const PRICING_MINIMAX_M3_PRIORITY: ModelPricing = ModelPricing {
    text_in: 450_000,
    text_cached: 90_000,
    text_out: 1_800_000,
    audio_in: 450_000,
    audio_cached: 90_000,
    audio_out: 1_800_000,
};

/// `gpt-transcribe` ≈ $0.07/h of input audio (S0-A estimate) → 20 µUSD/s.
pub const STT_MICROS_PER_SECOND: u64 = 20;

/// Spoken clips (C26 §2.3 7): MiniMax `speech-2.8-turbo` is $60 per million
/// characters (S0, 2026-11-14) = 60 µUSD per character; the OpenAI
/// realtime-mini fallback is charged at the same, higher-or-equal estimate.
pub const TTS_MICROS_PER_CHAR: i64 = 60;

/// Daily-budget charge of one synthesized clip of `chars` characters.
pub fn tts_cost_micros(chars: usize) -> i64 {
    i64::try_from(chars)
        .unwrap_or(i64::MAX)
        .saturating_mul(TTS_MICROS_PER_CHAR)
}

/// User audio is billed at 10 tokens per second (S0-A).
pub const USER_AUDIO_TOKENS_PER_SECOND: u64 = 10;

/// Pricing for a model id; unknown ids get the (more expensive) full model.
pub fn pricing_for(model: &str) -> ModelPricing {
    if model.starts_with("MiniMax-") {
        PRICING_MINIMAX_M3_PRIORITY
    } else if model.ends_with("-mini") {
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

    /// Text part of the context after this response (input + output).
    pub fn context_text(&self) -> u64 {
        self.input_text + self.output_text
    }

    /// Audio part of the context after this response, incl. shortfalls.
    pub fn context_audio(&self) -> u64 {
        self.input_audio + self.output_audio + self.input_shortfall() + self.output_shortfall()
    }
}

/// Cost of `tokens` at `price` µUSD/1M tokens, rounded up.
fn tokens_cost(tokens: u64, price: u64) -> u64 {
    (tokens.saturating_mul(price)).div_ceil(1_000_000)
}

impl Usage {
    /// Input tokens the details do not account for (charged as audio).
    pub fn input_shortfall(&self) -> u64 {
        self.input_tokens.saturating_sub(self.input_text + self.input_audio)
    }

    /// Output tokens the details do not account for (charged as audio).
    pub fn output_shortfall(&self) -> u64 {
        self.output_tokens.saturating_sub(self.output_text + self.output_audio)
    }
}

/// Exact cost of a reported usage block. Tokens missing from the details are
/// charged at the (higher) audio rates.
pub fn usage_cost_micros(pricing: &ModelPricing, usage: &Usage) -> i64 {
    let uncached_text = usage.input_text.saturating_sub(usage.cached_text);
    let uncached_audio = usage.input_audio.saturating_sub(usage.cached_audio);
    let cost = tokens_cost(uncached_text, pricing.text_in)
        + tokens_cost(usage.cached_text, pricing.text_cached)
        + tokens_cost(uncached_audio + usage.input_shortfall(), pricing.audio_in)
        + tokens_cost(usage.cached_audio, pricing.audio_cached)
        + tokens_cost(usage.output_text, pricing.text_out)
        + tokens_cost(usage.output_audio + usage.output_shortfall(), pricing.audio_out);
    i64::try_from(cost).unwrap_or(i64::MAX)
}

/// What a response may cost at most, reserved before it runs (§3.5):
/// `max_output_tokens × output rate + (context + audio since the last
/// response) × uncached input rate + STT of the turn`. The output is priced at
/// the audio rate (worst case); the context at the uncached rate of its
/// modality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationInput {
    pub max_output_tokens: u64,
    pub context_text_tokens: u64,
    pub context_audio_tokens: u64,
    pub audio_tokens_since_last: u64,
    pub stt_seconds: u64,
}

pub fn reservation_micros(pricing: &ModelPricing, input: &ReservationInput) -> i64 {
    let cost = tokens_cost(input.max_output_tokens, pricing.audio_out)
        + tokens_cost(input.context_text_tokens, pricing.text_in)
        + tokens_cost(
            input.context_audio_tokens.saturating_add(input.audio_tokens_since_last),
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
    fn reservation_prices_context_per_modality() {
        let input = ReservationInput {
            max_output_tokens: 600,
            context_text_tokens: 2_000,
            context_audio_tokens: 500,
            audio_tokens_since_last: 50,
            stt_seconds: 5,
        };
        // 600*64 + 2000*4 + 550*32 + 5*20 = 38_400 + 8_000 + 17_600 + 100
        assert_eq!(reservation_micros(&PRICING_FULL, &input), 64_100);
        assert!(reservation_micros(&PRICING_MINI, &input) < 64_100);
    }

    #[test]
    fn missing_details_are_charged_at_audio_rates() {
        // Totals only, no breakdown.
        let usage = Usage {
            input_tokens: 1_000,
            output_tokens: 100,
            ..Default::default()
        };
        assert_eq!(usage.input_shortfall(), 1_000);
        // 1000*32 + 100*64
        assert_eq!(usage_cost_micros(&PRICING_FULL, &usage), 32_000 + 6_400);
        // Partial details: the rest is audio.
        let partial = Usage {
            input_tokens: 1_000,
            input_text: 600,
            output_tokens: 100,
            output_text: 100,
            ..Default::default()
        };
        assert_eq!(usage_cost_micros(&PRICING_FULL, &partial), 2_400 + 12_800 + 2_400);
    }

    #[test]
    fn clips_are_charged_per_character() {
        // ≈ 900 characters per spoken minute ≈ $0.05 (design §2.6).
        assert_eq!(tts_cost_micros(900), 54_000);
        assert_eq!(tts_cost_micros(0), 0);
    }

    #[test]
    fn unknown_models_are_priced_as_full() {
        assert_eq!(pricing_for("gpt-realtime-3"), PRICING_FULL);
        assert_eq!(pricing_for("gpt-realtime-2.1-mini"), PRICING_MINI);
        assert_eq!(pricing_for("MiniMax-M3"), PRICING_MINIMAX_M3_PRIORITY);
        // 1k prompt (800 cached) + 100 output tokens on M3 priority:
        // 200 × 0.45 + 800 × 0.09 + 100 × 1.80 = 90 + 72 + 180 = 342 µUSD.
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 100,
            input_text: 1000,
            cached_text: 800,
            output_text: 100,
            ..Default::default()
        };
        assert_eq!(usage_cost_micros(&PRICING_MINIMAX_M3_PRIORITY, &usage), 342);
    }
}
