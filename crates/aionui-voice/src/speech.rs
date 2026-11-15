//! Production [`SpeechFactory`]: per-user speech providers from the user's own
//! provider rows, through the same backend-only credential resolver as calls.
//! MiniMax `t2a_v2` when a MiniMax key exists, else a realtime-mini session;
//! the refiner and the name-check transcriber use the user's OpenAI key.

use std::sync::Arc;

use crate::credentials::CredentialPolicy;
use crate::drafts::{SpeechFactory, SpeechKit};
use crate::ports::CredentialPort;
use crate::readback::{MiniMaxSynth, OpenAiTranscriber, RealtimeMiniSynth};
use crate::refiner::OpenAiRefiner;

/// MiniMax voices (S0-B probe used `English_expressive_narrator`).
pub const MINIMAX_JARVIS_VOICE: &str = "English_expressive_narrator";
pub const MINIMAX_AGENT_VOICE: &str = "English_CalmWoman";
/// Realtime voices: `marin` (verified in S0-A); the agent voice is unverified
/// until the S2 provider gate.
pub const REALTIME_JARVIS_VOICE: &str = "marin";
pub const REALTIME_AGENT_VOICE: &str = "cedar";

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
        let synth: Option<Arc<dyn crate::readback::SynthPort>> = match (&minimax, &openai) {
            (Some(credential), _) => Some(Arc::new(MiniMaxSynth {
                credential: credential.clone(),
                jarvis_voice: MINIMAX_JARVIS_VOICE.into(),
                agent_voice: MINIMAX_AGENT_VOICE.into(),
            })),
            (None, Some(credential)) => Some(Arc::new(RealtimeMiniSynth {
                credential: credential.clone(),
                jarvis_voice: REALTIME_JARVIS_VOICE.into(),
                agent_voice: REALTIME_AGENT_VOICE.into(),
            })),
            (None, None) => None,
        };
        SpeechKit {
            synth,
            transcriber: openai.clone().map(|credential| {
                Arc::new(OpenAiTranscriber {
                    http: self.http.clone(),
                    credential,
                }) as Arc<dyn crate::readback::TranscribePort>
            }),
            refiner: openai
                .map(|credential| Arc::new(OpenAiRefiner::new(credential)) as Arc<dyn crate::refiner::RefinerPort>),
        }
    }
}
