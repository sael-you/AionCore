//! Read-back synthesiser (§3.3.3) and the clip endpoint's store.
//!
//! The frozen read-back text becomes one local clip, never audio on the call:
//! MiniMax `t2a_v2` over its WebSocket API when a MiniMax key exists (wire
//! format verified live in S0-B: `connected_success` → `task_start` →
//! `task_started` → `task_continue` → hex audio chunks → `is_final`), else a
//! short-lived `gpt-realtime-2.1-mini` WebSocket session (text in, audio out,
//! no tools) whose output transcript must equal the text. Spoken confirmation
//! is allowed only when an independent transcription of the clip contains
//! the exact target name (cached per name, voice and dictionary version).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::credentials::ResolvedCredential;
use crate::drafts::evidence::normalize_utterance;

/// Clips are single use and expire after 2 minutes (§3.5 clip endpoint).
pub const CLIP_TTL: Duration = Duration::from_secs(120);
const SYNTH_TIMEOUT: Duration = Duration::from_secs(8);
pub const MINIMAX_TTS_MODEL: &str = "speech-2.8-turbo";
pub const READBACK_REALTIME_MODEL: &str = "gpt-realtime-2.1-mini";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipVoice {
    /// Jarvis's own voice (read-backs, outcome lines).
    Jarvis,
    /// The distinct agent voice for follow-ups, `say` and `ask` (§3.4).
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

#[async_trait::async_trait]
pub trait SynthPort: Send + Sync {
    async fn synthesize(&self, text: &str, voice: ClipVoice) -> Result<Clip, String>;
    /// Voice id used for `voice` (part of the name-check cache key).
    fn voice_id(&self, voice: ClipVoice) -> String;
}

#[async_trait::async_trait]
pub trait TranscribePort: Send + Sync {
    /// Independent transcription of a clip; `hints` are names to bias towards.
    async fn transcribe(&self, clip: &Clip, hints: &[String]) -> Result<String, String>;
}

fn ws_base(base_url: &str) -> String {
    if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base_url.to_owned()
    }
}

async fn connect(url: &str, api_key: &str) -> Result<crate::engine::openai::Sideband, String> {
    let mut request = url.into_client_request().map_err(|_| "invalid URL".to_owned())?;
    let auth = HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|_| "invalid key".to_owned())?;
    request.headers_mut().insert("Authorization", auth);
    let connector = aionui_shell::build_ws_connector().map_err(|e| e.to_string())?;
    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
        .await
        .map_err(|_| "connect failed".to_owned())?;
    Ok(ws)
}

/// MiniMax `t2a_v2` WebSocket synthesis (MP3, 24 kHz mono).
pub struct MiniMaxSynth {
    pub credential: ResolvedCredential,
    pub jarvis_voice: String,
    pub agent_voice: String,
}

#[async_trait::async_trait]
impl SynthPort for MiniMaxSynth {
    fn voice_id(&self, voice: ClipVoice) -> String {
        match voice {
            ClipVoice::Jarvis => self.jarvis_voice.clone(),
            ClipVoice::Agent => self.agent_voice.clone(),
        }
    }

    async fn synthesize(&self, text: &str, voice: ClipVoice) -> Result<Clip, String> {
        let url = format!("{}/ws/v1/t2a_v2", ws_base(&self.credential.base_url));
        let voice_id = self.voice_id(voice);
        let run = async {
            let mut ws = connect(&url, self.credential.api_key.expose()).await?;
            let mut audio = Vec::new();
            while let Some(message) = ws.next().await {
                let Ok(Message::Text(raw)) = message else {
                    if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                        break;
                    }
                    continue;
                };
                let event: Value = serde_json::from_str(raw.as_str()).map_err(|_| "invalid event".to_owned())?;
                match event["event"].as_str() {
                    Some("connected_success") => {
                        let start = json!({
                            "event": "task_start",
                            "model": MINIMAX_TTS_MODEL,
                            "language_boost": "English",
                            "voice_setting": {"voice_id": voice_id, "speed": 1, "vol": 1, "pitch": 0},
                            "audio_setting": {"sample_rate": 24000, "format": "mp3", "channel": 1},
                        });
                        ws.send(Message::Text(start.to_string().into()))
                            .await
                            .map_err(|_| "send failed")?;
                    }
                    Some("task_started") => {
                        let cont = json!({"event": "task_continue", "text": text});
                        ws.send(Message::Text(cont.to_string().into()))
                            .await
                            .map_err(|_| "send failed")?;
                    }
                    Some("task_failed") => return Err("task_failed".to_owned()),
                    _ => {}
                }
                if let Some(hex_audio) = event["data"]["audio"].as_str()
                    && !hex_audio.is_empty()
                {
                    audio.extend(hex::decode(hex_audio).map_err(|_| "invalid audio chunk".to_owned())?);
                }
                if event["is_final"] == true || event["data"]["status"] == 2 {
                    let _ = ws
                        .send(Message::Text(json!({"event": "task_finish"}).to_string().into()))
                        .await;
                    let _ = ws.close(None).await;
                    break;
                }
            }
            if audio.is_empty() {
                return Err("no audio".to_owned());
            }
            Ok(Clip {
                bytes: audio,
                mime: "audio/mpeg",
            })
        };
        tokio::time::timeout(SYNTH_TIMEOUT, run)
            .await
            .map_err(|_| "synthesis timed out".to_owned())?
    }
}

/// Fallback: a short-lived `gpt-realtime-2.1-mini` text-in/audio-out session.
pub struct RealtimeMiniSynth {
    pub credential: ResolvedCredential,
    pub jarvis_voice: String,
    pub agent_voice: String,
}

#[async_trait::async_trait]
impl SynthPort for RealtimeMiniSynth {
    fn voice_id(&self, voice: ClipVoice) -> String {
        match voice {
            ClipVoice::Jarvis => self.jarvis_voice.clone(),
            ClipVoice::Agent => self.agent_voice.clone(),
        }
    }

    async fn synthesize(&self, text: &str, voice: ClipVoice) -> Result<Clip, String> {
        let url = format!(
            "{}/v1/realtime?model={READBACK_REALTIME_MODEL}",
            ws_base(&self.credential.base_url)
        );
        let voice_id = self.voice_id(voice);
        let run = async {
            let mut ws = connect(&url, self.credential.api_key.expose()).await?;
            let update = json!({"type": "session.update", "session": {
                "type": "realtime",
                "output_modalities": ["audio"],
                "reasoning": {"effort": "minimal"},
                "tools": [],
                "tool_choice": "none",
                "audio": {"output": {"format": {"type": "audio/pcm", "rate": 24000}, "voice": voice_id}},
            }});
            ws.send(Message::Text(update.to_string().into()))
                .await
                .map_err(|_| "send failed")?;
            let create = json!({"type": "response.create", "response": {
                "conversation": "none",
                "input": [],
                "tool_choice": "none",
                "instructions": format!("Say exactly the following text and nothing else: {text}"),
            }});
            ws.send(Message::Text(create.to_string().into()))
                .await
                .map_err(|_| "send failed")?;
            let mut pcm = Vec::new();
            let mut spoken = String::new();
            while let Some(message) = ws.next().await {
                let Ok(Message::Text(raw)) = message else {
                    if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                        break;
                    }
                    continue;
                };
                let event: Value = serde_json::from_str(raw.as_str()).map_err(|_| "invalid event".to_owned())?;
                match event["type"].as_str() {
                    Some("response.output_audio.delta") => {
                        let delta = event["delta"].as_str().unwrap_or_default();
                        pcm.extend(
                            base64::engine::general_purpose::STANDARD
                                .decode(delta)
                                .map_err(|_| "invalid audio delta".to_owned())?,
                        );
                    }
                    Some("response.output_audio_transcript.done") => {
                        spoken = event["transcript"].as_str().unwrap_or_default().to_owned();
                    }
                    Some("response.done") => break,
                    Some("error") => return Err("provider error".to_owned()),
                    _ => {}
                }
            }
            let _ = ws.close(None).await;
            // S0-A: compare the output transcript to the text before it counts.
            if normalize_utterance(&spoken) != normalize_utterance(text) {
                return Err("spoken text differs from the read-back".to_owned());
            }
            if pcm.is_empty() {
                return Err("no audio".to_owned());
            }
            Ok(Clip {
                bytes: wav_from_pcm16(&pcm, 24_000),
                mime: "audio/wav",
            })
        };
        tokio::time::timeout(SYNTH_TIMEOUT, run)
            .await
            .map_err(|_| "synthesis timed out".to_owned())?
    }
}

/// MiniMax first, OpenAI realtime-mini when MiniMax fails or is slow
/// (D51, C26 §2.3 6): before, a MiniMax error meant text only.
pub struct FallbackSynth {
    pub primary: Arc<dyn SynthPort>,
    pub fallback: Arc<dyn SynthPort>,
    /// The primary gets this long before the fallback is asked.
    pub primary_timeout: Duration,
}

/// MiniMax's budget inside [`FallbackSynth`] (cold TTFA P95 ≈ 1.1 s in S0).
pub const PRIMARY_SYNTH_TIMEOUT: Duration = Duration::from_secs(4);

#[async_trait::async_trait]
impl SynthPort for FallbackSynth {
    fn voice_id(&self, voice: ClipVoice) -> String {
        self.primary.voice_id(voice)
    }

    async fn synthesize(&self, text: &str, voice: ClipVoice) -> Result<Clip, String> {
        let primary = tokio::time::timeout(self.primary_timeout, self.primary.synthesize(text, voice))
            .await
            .unwrap_or_else(|_| Err("synthesis timed out".to_owned()));
        match primary {
            Ok(clip) => Ok(clip),
            Err(err) => {
                tracing::warn!(error = %err, "voice primary synthesis failed; using the OpenAI fallback");
                self.fallback.synthesize(text, voice).await
            }
        }
    }
}

/// Mono PCM16 → WAV container.
pub fn wav_from_pcm16(pcm: &[u8], rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

/// Independent transcription with `gpt-transcribe` (reuses the STT client).
pub struct OpenAiTranscriber {
    pub http: reqwest::Client,
    pub credential: ResolvedCredential,
}

#[async_trait::async_trait]
impl TranscribePort for OpenAiTranscriber {
    async fn transcribe(&self, clip: &Clip, hints: &[String]) -> Result<String, String> {
        let config = aionui_api_types::OpenAISpeechToTextConfig {
            api_key: self.credential.api_key.expose().to_owned(),
            base_url: Some(self.credential.base_url.clone()),
            model: "gpt-transcribe".to_owned(),
            language: Some("en".to_owned()),
            prompt: (!hints.is_empty()).then(|| format!("Names: {}", hints.join(", "))),
            temperature: None,
        };
        let (name, mime) = match clip.mime {
            "audio/mpeg" => ("readback.mp3", "audio/mpeg"),
            _ => ("readback.wav", "audio/wav"),
        };
        aionui_shell::transcribe_openai(&self.http, &config, clip.bytes.clone(), name, mime, Some("en"))
            .await
            .map(|result| result.text)
            .map_err(|e| e.to_string())
    }
}

/// Name check: does an independent transcription contain the target name?
/// Cached per (name, voice id, dictionary version) — only new names cost a pass.
#[derive(Default)]
pub struct NameCheck {
    cache: DashMap<(String, String, u32), bool>,
}

impl NameCheck {
    pub async fn passes(
        &self,
        transcriber: Option<&dyn TranscribePort>,
        clip: &Clip,
        target_name: &str,
        voice_id: &str,
        dictionary_version: u32,
    ) -> bool {
        let key = (target_name.to_owned(), voice_id.to_owned(), dictionary_version);
        if let Some(hit) = self.cache.get(&key) {
            return *hit;
        }
        let Some(transcriber) = transcriber else {
            // No transcription provider: tap-confirm only (not cached).
            return false;
        };
        let passed = match transcriber.transcribe(clip, &[target_name.to_owned()]).await {
            Ok(text) => {
                let heard = normalize_utterance(&text);
                heard.contains(&normalize_utterance(target_name)) && heard.contains("message")
            }
            Err(err) => {
                tracing::warn!(error = %err, "voice read-back name check failed; tap-confirm only");
                return false;
            }
        };
        self.cache.insert(key, passed);
        passed
    }
}

struct StoredClip {
    clip: Clip,
    user_id: String,
    session_id: Option<String>,
    expires_at: Instant,
}

/// In-memory, single-use, user-bound clips for `GET /api/voice/readbacks/{id}`.
#[derive(Clone, Default)]
pub struct ClipStore {
    clips: Arc<DashMap<String, StoredClip>>,
}

impl ClipStore {
    pub fn insert(&self, user_id: &str, session_id: Option<&str>, clip: Clip) -> String {
        let now = Instant::now();
        self.clips.retain(|_, stored| stored.expires_at > now);
        let id = aionui_common::generate_prefixed_id("rb");
        self.clips.insert(
            id.clone(),
            StoredClip {
                clip,
                user_id: user_id.to_owned(),
                session_id: session_id.map(str::to_owned),
                expires_at: now + CLIP_TTL,
            },
        );
        id
    }

    /// Take the clip once; only its owner, only before it expires.
    pub fn take(&self, user_id: &str, clip_id: &str) -> Option<Clip> {
        let owned = self
            .clips
            .get(clip_id)
            .is_some_and(|stored| stored.user_id == user_id && stored.expires_at > Instant::now());
        if !owned {
            return None;
        }
        self.clips.remove(clip_id).map(|(_, stored)| stored.clip)
    }

    /// Whether the owner can still fetch the clip (not taken, not expired).
    pub fn available(&self, user_id: &str, clip_id: &str) -> bool {
        self.clips
            .get(clip_id)
            .is_some_and(|stored| stored.user_id == user_id && stored.expires_at > Instant::now())
    }

    /// The clip's session (for binding checks), without consuming it.
    pub fn session_of(&self, clip_id: &str) -> Option<String> {
        self.clips.get(clip_id).and_then(|s| s.session_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Hears(&'static str);

    #[async_trait::async_trait]
    impl TranscribePort for Hears {
        async fn transcribe(&self, _clip: &Clip, _hints: &[String]) -> Result<String, String> {
            Ok(self.0.to_owned())
        }
    }

    fn clip() -> Clip {
        Clip {
            bytes: vec![1, 2, 3],
            mime: "audio/mpeg",
        }
    }

    #[tokio::test]
    async fn name_check_requires_the_exact_name_and_is_cached() {
        let check = NameCheck::default();
        assert!(
            check
                .passes(
                    Some(&Hears("Message for AionCore: add logging.")),
                    &clip(),
                    "AionCore",
                    "v",
                    1
                )
                .await
        );
        // Cached: a transcriber that would fail is not asked again.
        assert!(check.passes(Some(&Hears("nothing")), &clip(), "AionCore", "v", 1).await);
        // Misheard name fails; no transcriber → tap only.
        assert!(
            !check
                .passes(
                    Some(&Hears("Message for IonCore: add logging.")),
                    &clip(),
                    "Kanban build",
                    "v",
                    1
                )
                .await
        );
        assert!(!check.passes(None, &clip(), "Other", "v", 1).await);
    }

    #[tokio::test]
    async fn clips_are_single_use_and_owner_bound() {
        let store = ClipStore::default();
        let id = store.insert("alice", Some("vs_1"), clip());
        assert_eq!(store.session_of(&id).as_deref(), Some("vs_1"));
        assert!(store.take("bob", &id).is_none(), "other users cannot fetch it");
        assert_eq!(store.take("alice", &id), Some(clip()));
        assert!(store.take("alice", &id).is_none(), "single use");
    }

    #[tokio::test(start_paused = true)]
    async fn clips_expire() {
        let store = ClipStore::default();
        let id = store.insert("alice", None, clip());
        tokio::time::advance(CLIP_TTL + Duration::from_secs(1)).await;
        assert!(store.take("alice", &id).is_none());
    }

    struct Fixed(Result<&'static str, &'static str>, Duration);

    #[async_trait::async_trait]
    impl SynthPort for Fixed {
        fn voice_id(&self, _voice: ClipVoice) -> String {
            "fixed".into()
        }

        async fn synthesize(&self, _text: &str, _voice: ClipVoice) -> Result<Clip, String> {
            tokio::time::sleep(self.1).await;
            self.0
                .map(|tag| Clip {
                    bytes: tag.as_bytes().to_vec(),
                    mime: "audio/mpeg",
                })
                .map_err(str::to_owned)
        }
    }

    fn fallback(primary: Fixed) -> FallbackSynth {
        FallbackSynth {
            primary: Arc::new(primary),
            fallback: Arc::new(Fixed(Ok("openai"), Duration::ZERO)),
            primary_timeout: PRIMARY_SYNTH_TIMEOUT,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn minimax_first_openai_when_it_fails_or_stalls() {
        let ok = fallback(Fixed(Ok("minimax"), Duration::ZERO));
        assert_eq!(ok.synthesize("hi", ClipVoice::Agent).await.unwrap().bytes, b"minimax");
        let failing = fallback(Fixed(Err("task_failed"), Duration::ZERO));
        assert_eq!(
            failing.synthesize("hi", ClipVoice::Agent).await.unwrap().bytes,
            b"openai"
        );
        let stalled = fallback(Fixed(Ok("minimax"), Duration::from_secs(30)));
        assert_eq!(
            stalled.synthesize("hi", ClipVoice::Agent).await.unwrap().bytes,
            b"openai"
        );
    }

    #[test]
    fn wav_header_is_valid() {
        let wav = wav_from_pcm16(&[0u8; 48], 24_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 48);
    }
}
