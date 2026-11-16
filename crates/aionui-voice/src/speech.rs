//! Production [`SpeechFactory`]: per-user speech providers from the user's own
//! provider rows, through the same backend-only credential resolver as calls.
//! MiniMax `t2a_v2` when a MiniMax key exists, with a realtime-mini session as
//! the automatic fallback (C26, D51), else realtime-mini alone;
//! the refiner and the name-check transcriber use the user's OpenAI key.

use std::sync::Arc;

use crate::credentials::CredentialPolicy;
use crate::drafts::{SpeechFactory, SpeechKit};
use crate::ports::CredentialPort;
use crate::readback::SynthPort;
use crate::readback::{FallbackSynth, MiniMaxSynth, OpenAiTranscriber, PRIMARY_SYNTH_TIMEOUT, RealtimeMiniSynth};
use crate::refiner::OpenAiRefiner;

/// MiniMax voices (S0-B probe used `English_expressive_narrator`).
pub const MINIMAX_JARVIS_VOICE: &str = "English_expressive_narrator";
pub const MINIMAX_AGENT_VOICE: &str = "English_CalmWoman";
/// Realtime voices: `marin` (verified in S0-A); the agent voice is unverified
/// until the S2 provider gate.
pub const REALTIME_JARVIS_VOICE: &str = "marin";
pub const REALTIME_AGENT_VOICE: &str = "cedar";

/// MiniMax → OpenAI fallback when both exist; else whichever exists.
pub(crate) fn select_synth(
    minimax: Option<Arc<dyn SynthPort>>,
    openai: Option<Arc<dyn SynthPort>>,
) -> Option<Arc<dyn SynthPort>> {
    match (minimax, openai) {
        (Some(primary), Some(fallback)) => Some(Arc::new(FallbackSynth {
            primary,
            fallback,
            primary_timeout: PRIMARY_SYNTH_TIMEOUT,
        })),
        (Some(primary), None) => Some(primary),
        (None, openai) => openai,
    }
}

pub struct ProviderSpeechFactory {
    pub credentials: Arc<dyn CredentialPort>,
    pub policy: CredentialPolicy,
    pub http: reqwest::Client,
}

#[async_trait::async_trait]
impl SpeechFactory for ProviderSpeechFactory {
    async fn for_user(&self, user_id: &str) -> SpeechKit {
        let rows = match self.credentials.provider_credentials(user_id).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "voice speech providers unavailable");
                return SpeechKit::default();
            }
        };
        let openai = self.policy.resolve_openai(rows.clone(), None).ok();
        let minimax = self.policy.resolve_minimax(&rows);
        let synth = select_synth(
            minimax.map(|credential| {
                Arc::new(MiniMaxSynth {
                    credential,
                    jarvis_voice: MINIMAX_JARVIS_VOICE.into(),
                    agent_voice: MINIMAX_AGENT_VOICE.into(),
                }) as Arc<dyn SynthPort>
            }),
            openai.clone().map(|credential| {
                Arc::new(RealtimeMiniSynth {
                    credential,
                    jarvis_voice: REALTIME_JARVIS_VOICE.into(),
                    agent_voice: REALTIME_AGENT_VOICE.into(),
                }) as Arc<dyn SynthPort>
            }),
        );
        SpeechKit {
            synth,
            transcriber: openai.clone().map(|credential| {
                Arc::new(OpenAiTranscriber {
                    http: self.http.clone(),
                    credential,
                }) as Arc<dyn crate::readback::TranscribePort>
            }),
            refiner: openai
                .clone()
                .map(|credential| Arc::new(OpenAiRefiner::new(credential)) as Arc<dyn crate::refiner::RefinerPort>),
            summarizer: openai.map(|credential| {
                Arc::new(crate::summary::OpenAiSummarizer::new(credential)) as Arc<dyn crate::summary::SummaryPort>
            }),
        }
    }
}
