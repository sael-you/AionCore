//! Speech adapters on the wire, against the fake provider: realtime-mini and
//! MiniMax `t2a_v2` synthesis, `gpt-transcribe` name check, and the refiner
//! over the Responses API — resolved through the production speech factory
//! from the user's provider rows.

use std::sync::Arc;

use aionui_voice::drafts::SpeechFactory;
use aionui_voice::fake_provider::FakeOpenAi;
use aionui_voice::{
    ApiKey, ClipVoice, CredentialPolicy, CredentialPort, ProviderCredential, ProviderSpeechFactory, RefineInput,
};
use serde_json::json;

struct Rows(Vec<ProviderCredential>);

#[async_trait::async_trait]
impl CredentialPort for Rows {
    async fn provider_credentials(&self, _user_id: &str) -> Result<Vec<ProviderCredential>, String> {
        Ok(self.0.clone())
    }
}

fn row(id: &str, platform: &str, base: &str) -> ProviderCredential {
    ProviderCredential {
        provider_id: id.into(),
        platform: platform.into(),
        base_url: base.into(),
        enabled: true,
        api_key: ApiKey::new(format!("key-{id}")),
    }
}

fn factory(fake: &FakeOpenAi, rows: Vec<ProviderCredential>) -> ProviderSpeechFactory {
    ProviderSpeechFactory {
        credentials: Arc::new(Rows(rows)),
        policy: CredentialPolicy::with_fake_provider_url(fake.base_url()),
        http: reqwest::Client::new(),
    }
}

#[tokio::test]
async fn openai_only_uses_realtime_mini_and_checks_the_spoken_text() {
    let fake = FakeOpenAi::start().await;
    let kit = factory(&fake, vec![row("oa", "OpenAI", "https://api.openai.com/v1")])
        .for_user("alice")
        .await;
    let synth = kit.synth.expect("realtime-mini fallback");
    let clip = synth
        .synthesize("Message for Kanban build: rerun the tests.", ClipVoice::Jarvis)
        .await
        .unwrap();
    assert_eq!(clip.mime, "audio/wav");
    assert_eq!(&clip.bytes[..4], b"RIFF");
    assert!(String::from_utf8_lossy(&clip.bytes[44..]).contains("Kanban build"));

    // Independent transcription of the clip (name check).
    let heard = kit
        .transcriber
        .expect("transcriber")
        .transcribe(&clip, &["Kanban build".into()])
        .await
        .unwrap();
    assert!(heard.contains("Message for Kanban build"));

    // Refiner over the Responses API (structured output).
    fake.set_refinement(
        json!({"goal": "Rerun the tests", "context": "", "constraints": "", "acceptance": "",
        "references": [], "summary": "rerun the tests"}),
    );
    let refined = kit
        .refiner
        .expect("refiner")
        .refine(&RefineInput {
            transcript: "rerun the tests".into(),
            target_name: "Kanban build".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(refined.goal, "Rerun the tests");
}

#[tokio::test]
async fn minimax_row_wins_for_read_back_speech() {
    let fake = FakeOpenAi::start().await;
    let kit = factory(
        &fake,
        vec![
            row("oa", "OpenAI", "https://api.openai.com/v1"),
            row("mm", "minimax", "https://api.minimax.io/v1"),
        ],
    )
    .for_user("alice")
    .await;
    let clip = kit
        .synth
        .expect("minimax synth")
        .synthesize("Message for Kanban build: tag the release.", ClipVoice::Agent)
        .await
        .unwrap();
    assert_eq!(clip.mime, "audio/mpeg");
    assert_eq!(clip.bytes, b"Message for Kanban build: tag the release.".to_vec());
}

#[tokio::test]
async fn no_provider_means_no_speech_and_tap_only() {
    let fake = FakeOpenAi::start().await;
    let kit = factory(&fake, vec![]).for_user("alice").await;
    assert!(kit.synth.is_none() && kit.transcriber.is_none() && kit.refiner.is_none());
}
