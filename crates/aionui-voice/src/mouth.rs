//! The cascade's voice: every [`crate::engine::EngineEvent::AssistantSpeech`]
//! sentence becomes a clip from the user's speech kit (MiniMax `t2a_v2` first,
//! the OpenAI realtime-mini fallback after it; D51), synthesized one at a time
//! in the order spoken and delivered as `voice.followUp` with status `reply`
//! (no conversation). A barge-in drops every sentence not delivered yet.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::drafts::DraftEngine;
use crate::drafts::evidence::contains_confirm_grammar;
use crate::readback::ClipVoice;

/// `voice.followUp` status of a cascade answer clip.
pub(crate) const REPLY_STATUS: &str = "reply";

pub(crate) struct Mouth {
    tx: mpsc::UnboundedSender<(u64, String)>,
    epoch: Arc<AtomicU64>,
}

impl Mouth {
    pub(crate) fn spawn(drafts: Arc<DraftEngine>, user_id: String, session_id: String) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, String)>();
        let epoch = Arc::new(AtomicU64::new(0));
        let current = epoch.clone();
        tokio::spawn(async move {
            while let Some((said_in, line)) = rx.recv().await {
                let wanted = || current.load(Ordering::SeqCst) == said_in;
                if wanted() {
                    speak_reply(&drafts, &user_id, &session_id, &line, &wanted).await;
                }
            }
        });
        Self { tx, epoch }
    }

    /// Queue a sentence of the current answer.
    pub(crate) fn say(&self, line: String) {
        let _ = self.tx.send((self.epoch.load(Ordering::SeqCst), line));
    }

    /// Barge-in / call end: nothing queued or in synthesis is delivered.
    pub(crate) fn hush(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
}

/// Synthesize (metered against the daily budget) and deliver one sentence,
/// unless it stopped being wanted meanwhile. A sentence with confirmation
/// grammar is shown as text only, like every Jarvis line (it must never be
/// heard as the user's own "send it").
async fn speak_reply(
    drafts: &DraftEngine,
    user_id: &str,
    session_id: &str,
    line: &str,
    wanted: &(dyn Fn() -> bool + Send + Sync),
) {
    let started = Instant::now();
    let clip = if contains_confirm_grammar(line) {
        tracing::warn!(session_id, "voice reply clip refused: contains confirm grammar");
        None
    } else {
        match drafts.speech.for_user(user_id).await.synth {
            Some(synth) => drafts
                .synthesize_metered(synth.as_ref(), user_id, Some(session_id), line, ClipVoice::Jarvis)
                .await
                .map_err(|err| tracing::warn!(session_id, error = %err, "voice reply synthesis failed; showing text"))
                .ok(),
            None => None,
        }
    };
    if !wanted() {
        tracing::debug!(session_id, "voice reply clip dropped (barge-in)");
        return;
    }
    let clip_id = clip.map(|clip| drafts.clips.insert(user_id, Some(session_id), clip));
    tracing::info!(
        session_id,
        synth_ms = started.elapsed().as_millis() as u64,
        chars = line.chars().count(),
        clip = clip_id.is_some(),
        "voice cascade reply clip"
    );
    drafts.send_line(user_id, Some(session_id), "", REPLY_STATUS, line, clip_id);
}
