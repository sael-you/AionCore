//! The draft engine (§3.3): builds drafts from what the user said, reads them
//! back as a local clip, confirms them only on a tap or full spoken evidence,
//! holds 4 s, sends with `draft_id:version` as idempotency key and reports
//! the outcome truthfully. Every state change is a versioned CAS.

use std::sync::Arc;

use aionui_api_types::{
    VoiceDiffOp, VoiceDraftTarget, VoiceDraftUpdated, VoiceFollowUp, VoiceReadbackReady, VoiceServerFrame,
};
use aionui_db::{
    DraftPatch, DraftTransition, IVoiceDraftRepository, IVoiceSessionRepository, VoiceDraftRow, VoiceUsageDelta,
};
use dashmap::DashMap;

use super::evidence::contains_confirm_grammar;
use super::state::{Delivery, DraftAction, DraftState, idempotency_key, payload_hash};
use super::text::{DiffOp, diff, readback_text};
use crate::conversation_mode::{
    STATUS_ADDED, STATUS_FAILED, STATUS_QUEUED, STATUS_SENT, VOICE_MODE_DELIVERY, voice_message,
};
use crate::conversation_port::{ConversationPort, SendOutcome};
use crate::frames::VoiceSockets;
use crate::readback::{ClipStore, ClipVoice, NameCheck, SynthPort, TranscribePort};
use crate::refiner::{RefineInput, RefinerPort, refine_draft};
use crate::resolver::{Candidate, Resolution, resolve};

/// Drafts expire this long after their read-back (§3.3.1).
pub const DRAFT_TTL_MS: i64 = 120_000;
/// Queued drafts expire after 10 minutes.
pub const QUEUE_TTL_MS: i64 = 600_000;
/// A new-turn send counts as not started after this long without a turn.
pub const START_CHECK_MS: i64 = 10_000;
/// Read-back pronunciation dictionary version (name-check cache key).
const DICTIONARY_VERSION: u32 = 1;
/// A queued conversation-mode message waits this long for its turn (a long
/// agent turn is normal; the C9 10 min is for Jarvis drafts).
pub const VOICE_QUEUE_TTL_MS: i64 = 30 * 60_000;

/// The daily budget seen by spoken clips (C26 §3.5): what is left today, and
/// a charge notice so a live call's meter warns and hangs up at 100 %.
#[async_trait::async_trait]
pub trait TtsBudget: Send + Sync {
    async fn remaining_micros(&self, user_id: &str) -> i64;
    fn charged(&self, user_id: &str, session_id: &str, micros: i64);
}

/// Whether a draft row is a queued conversation-mode message (C26).
pub(crate) fn is_voice_mode(row: &VoiceDraftRow) -> bool {
    row.delivery.as_deref() == Some(VOICE_MODE_DELIVERY)
}

/// Per-user speech providers (resolved from the user's own credentials).
#[derive(Clone, Default)]
pub struct SpeechKit {
    pub synth: Option<Arc<dyn SynthPort>>,
    pub transcriber: Option<Arc<dyn TranscribePort>>,
    pub refiner: Option<Arc<dyn RefinerPort>>,
    /// Follow-up summaries (§3.4); `None` → the template alone.
    pub summarizer: Option<Arc<dyn crate::summary::SummaryPort>>,
}

#[async_trait::async_trait]
pub trait SpeechFactory: Send + Sync {
    async fn for_user(&self, user_id: &str) -> SpeechKit;
}

/// What a draft is built from.
#[derive(Debug, Clone, Default)]
pub struct DraftRequest {
    pub transcript: String,
    pub new_conversation: bool,
    pub focus: Option<String>,
    pub tool_refs: Vec<String>,
    pub tool_snippets: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct DraftExtras {
    diff: Vec<DiffOp>,
    options: Vec<(String, String)>,
}

/// Result of a send attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivered {
    Sent,
    AwaitingStart,
    Queued,
    Failed,
    Lost,
}

pub struct DraftEngine {
    pub(crate) repo: Arc<dyn IVoiceDraftRepository>,
    pub(crate) conversations: Arc<dyn ConversationPort>,
    pub(crate) speech: Arc<dyn SpeechFactory>,
    pub(crate) clips: ClipStore,
    name_check: NameCheck,
    sockets: VoiceSockets,
    extras: DashMap<(String, i64), DraftExtras>,
    /// Conversations sent to by voice → user (follow-up watcher).
    pub(crate) watched: DashMap<String, String>,
    /// Bound conversations whose agent spoke (`say`) since the last voice
    /// message: their turn end needs no "finished" clip (C26).
    pub(crate) spoke: DashMap<String, ()>,
    /// Session usage rows: clip characters are charged to the daily budget.
    sessions: Arc<dyn IVoiceSessionRepository>,
    /// Set by the service once it exists (it owns the settings and live calls).
    pub(crate) budget: std::sync::OnceLock<Arc<dyn TtsBudget>>,
}

fn now_ms() -> i64 {
    aionui_common::now_ms()
}

impl DraftEngine {
    pub fn new(
        repo: Arc<dyn IVoiceDraftRepository>,
        conversations: Arc<dyn ConversationPort>,
        speech: Arc<dyn SpeechFactory>,
        clips: ClipStore,
        sockets: VoiceSockets,
        sessions: Arc<dyn IVoiceSessionRepository>,
    ) -> Self {
        Self {
            repo,
            conversations,
            speech,
            clips,
            name_check: NameCheck::default(),
            sockets,
            extras: DashMap::new(),
            watched: DashMap::new(),
            spoke: DashMap::new(),
            sessions,
            budget: std::sync::OnceLock::new(),
        }
    }

    // ---------------------------------------------------------------- frames

    pub(crate) fn emit(&self, row: &VoiceDraftRow, hold: Option<(u64, bool)>) {
        // A queued conversation-mode message is not a draft: no card.
        if is_voice_mode(row) {
            return;
        }
        let frame = self.draft_frame(row, hold);
        self.sockets.send_user(&row.user_id, &frame);
        if DraftState::parse(&row.state).is_some_and(|s| s.is_terminal()) {
            self.extras.remove(&(row.id.clone(), row.version));
        }
    }

    /// Frames that put the user's pending draft card back on a socket that
    /// connected late (`voice.hello`): `draftUpdated`, plus `readbackReady`
    /// while its clip is still unfetched (clips are single use).
    pub async fn replay_for(&self, user_id: &str) -> Vec<VoiceServerFrame> {
        let Some(row) = self.pending_for_user(user_id).await else {
            return Vec::new();
        };
        let mut frames = vec![self.draft_frame(&row, None)];
        if let Some(clip_id) = row.readback_id.as_deref()
            && self.clips.available(user_id, clip_id)
        {
            frames.push(VoiceServerFrame::ReadbackReady(VoiceReadbackReady {
                voice_session_id: row.session_id.clone(),
                draft_id: row.id.clone(),
                version: row.version,
                readback_id: clip_id.to_owned(),
                url: format!("/api/voice/readbacks/{clip_id}"),
            }));
        }
        frames
    }

    fn draft_frame(&self, row: &VoiceDraftRow, hold: Option<(u64, bool)>) -> VoiceServerFrame {
        let extras = self
            .extras
            .get(&(row.id.clone(), row.version))
            .map(|e| e.clone())
            .unwrap_or_default();
        let kind = if row.action == DraftAction::CreateConversation.as_str() {
            "project"
        } else {
            "conversation"
        };
        VoiceServerFrame::DraftUpdated(VoiceDraftUpdated {
            voice_session_id: row.session_id.clone(),
            draft_id: row.id.clone(),
            version: row.version,
            hash: row.payload_hash.clone(),
            state: row.state.clone(),
            target: match (&row.target_id, &row.target_name) {
                (Some(id), Some(name)) => Some(VoiceDraftTarget {
                    id: id.clone(),
                    name: name.clone(),
                    kind: Some(kind.to_owned()),
                }),
                _ => None,
            },
            options: extras
                .options
                .iter()
                .map(|(id, name)| VoiceDraftTarget {
                    id: id.clone(),
                    name: name.clone(),
                    kind: Some("conversation".to_owned()),
                })
                .collect(),
            text: row.text.clone().unwrap_or_default(),
            diff: extras
                .diff
                .iter()
                .map(|d| VoiceDiffOp {
                    op: d.op.to_owned(),
                    text: d.text.clone(),
                    transform: d.transform.map(str::to_owned),
                })
                .collect(),
            delivery: row
                .delivery
                .clone()
                .unwrap_or_else(|| Delivery::NewTurn.as_str().to_owned()),
            spoken_confirm: row.spoken_confirm,
            hold_remaining_ms: hold.map(|(ms, _)| ms),
            hold_frozen: hold.map(|(_, frozen)| frozen),
        })
    }

    /// Speak a short status line (Jarvis or agent voice) as a follow-up clip.
    pub(crate) async fn speak_line(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        conversation_id: &str,
        status: &str,
        line: &str,
        voice: ClipVoice,
    ) -> Option<String> {
        if contains_confirm_grammar(line) {
            tracing::warn!(status, "voice line refused: contains confirm grammar");
            self.send_line(user_id, session_id, conversation_id, status, line, None);
            return None;
        }
        self.speak_unchecked(user_id, session_id, conversation_id, status, line, voice)
            .await
    }

    /// Speak a line without the confirm-grammar ban: only for the bound
    /// conversation's own agent in conversation mode, where no draft exists
    /// and a clip is never confirmation evidence (C26 §2.3 4).
    pub(crate) async fn speak_unchecked(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        conversation_id: &str,
        status: &str,
        line: &str,
        voice: ClipVoice,
    ) -> Option<String> {
        let clip_id = match self.speech.for_user(user_id).await.synth {
            Some(synth) => match self
                .synthesize_metered(synth.as_ref(), user_id, session_id, line, voice)
                .await
            {
                Ok(clip) => Some(self.clips.insert(user_id, session_id, clip)),
                Err(err) => {
                    tracing::warn!(error = %err, "voice line synthesis failed; showing text");
                    None
                }
            },
            None => None,
        };
        self.send_line(user_id, session_id, conversation_id, status, line, clip_id.clone());
        clip_id
    }

    /// A text-only line (no clip): conversation-mode outcomes.
    pub(crate) fn voice_line(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        conversation_id: &str,
        status: &str,
        line: &str,
    ) {
        self.send_line(user_id, session_id, conversation_id, status, line, None);
    }

    pub(crate) fn send_line(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        conversation_id: &str,
        status: &str,
        line: &str,
        clip_id: Option<String>,
    ) {
        self.sockets.send_user(
            user_id,
            &VoiceServerFrame::FollowUp(VoiceFollowUp {
                voice_session_id: session_id.map(str::to_owned),
                conversation_id: conversation_id.to_owned(),
                status: status.to_owned(),
                line: line.to_owned(),
                clip_id,
            }),
        );
    }

    /// Synthesize a clip and charge its characters to the daily budget
    /// (clips were unmetered before C26): to the given session, else to the
    /// user's latest one.
    pub(crate) async fn synthesize_metered(
        &self,
        synth: &dyn SynthPort,
        user_id: &str,
        session_id: Option<&str>,
        text: &str,
        voice: ClipVoice,
    ) -> Result<crate::readback::Clip, String> {
        let cost = crate::pricing::tts_cost_micros(text.chars().count());
        if let Some(budget) = self.budget.get()
            && budget.remaining_micros(user_id).await < cost
        {
            tracing::info!(user_id, cost_micros = cost, "voice clip refused: daily budget reached");
            return Err("daily voice budget reached".to_owned());
        }
        let clip = synth.synthesize(text, voice).await?;
        let session = match session_id {
            Some(id) => Some(id.to_owned()),
            None => self
                .sessions
                .list(user_id, 1)
                .await
                .ok()
                .and_then(|rows| rows.into_iter().next().map(|r| r.id)),
        };
        if let Some(session) = session {
            let delta = VoiceUsageDelta {
                cost_micros: cost,
                ..Default::default()
            };
            if let Err(err) = self.sessions.add_usage(user_id, &session, &delta, now_ms()).await {
                tracing::warn!(error = %err, "voice clip usage not recorded");
            } else if let Some(budget) = self.budget.get() {
                budget.charged(user_id, &session, cost);
            }
        }
        Ok(clip)
    }

    async fn transition(&self, row: &VoiceDraftRow, to: Option<DraftState>, patch: DraftPatch<'_>) -> bool {
        let from = match to {
            Some(state) => state.allowed_from().iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            None => vec![row.state.as_str()],
        };
        let applied = self
            .repo
            .transition_draft(&DraftTransition {
                user_id: &row.user_id,
                id: &row.id,
                version: row.version,
                from: &from,
                to: to.map(|s| s.as_str()),
                patch,
            })
            .await
            .unwrap_or_else(|err| {
                tracing::error!(error = %err, draft_id = %row.id, "voice draft transition failed");
                false
            });
        if applied {
            tracing::info!(
                draft_id = %row.id,
                version = row.version,
                from = %row.state,
                to = to.map_or(row.state.as_str(), |s| s.as_str()),
                payload_hash = %row.payload_hash,
                "voice draft transition"
            );
        }
        applied
    }

    async fn reload(&self, row: &VoiceDraftRow) -> Option<VoiceDraftRow> {
        self.repo
            .get_draft(&row.user_id, &row.id, row.version)
            .await
            .ok()
            .flatten()
    }

    async fn transition_and_emit(
        &self,
        row: &VoiceDraftRow,
        to: DraftState,
        patch: DraftPatch<'_>,
    ) -> Option<VoiceDraftRow> {
        if !self.transition(row, Some(to), patch).await {
            return None;
        }
        let fresh = self.reload(row).await?;
        self.emit(&fresh, None);
        Some(fresh)
    }

    // -------------------------------------------------------------- creation

    /// Build a draft from what the user said (`refine_request`).
    pub async fn create(&self, user_id: &str, session_id: Option<&str>, req: DraftRequest) -> Option<VoiceDraftRow> {
        let id = aionui_common::generate_prefixed_id("vd");
        self.build_version(user_id, session_id, &id, 1, req, None).await
    }

    async fn build_version(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        id: &str,
        version: i64,
        req: DraftRequest,
        forced_target: Option<(String, String)>,
    ) -> Option<VoiceDraftRow> {
        let conversations = self.conversations.list_conversations(user_id).await.ok()?;
        let projects = self.conversations.list_projects(user_id).await.unwrap_or_default();
        let project_name = |pid: &Option<String>| {
            pid.as_ref()
                .and_then(|p| projects.iter().find(|x| &x.id == p).map(|x| x.name.clone()))
        };
        let (action, resolution) = if req.new_conversation {
            let candidates: Vec<Candidate> = projects
                .iter()
                .map(|p| Candidate {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    project_name: None,
                })
                .collect();
            (
                DraftAction::CreateConversation,
                resolve(&req.transcript, &candidates, None),
            )
        } else {
            let candidates: Vec<Candidate> = conversations
                .iter()
                .map(|c| Candidate {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    project_name: project_name(&c.project_id),
                })
                .collect();
            let resolution = match &forced_target {
                Some((id, name)) => Resolution::Unique {
                    id: id.clone(),
                    name: name.clone(),
                    score: 1.0,
                },
                None => resolve(&req.transcript, &candidates, req.focus.as_deref()),
            };
            (DraftAction::SendMessage, resolution)
        };
        let target = match &resolution {
            Resolution::Unique { id, name, .. } => Some((id.clone(), name.clone())),
            Resolution::Focus { id } => conversations
                .iter()
                .find(|c| &c.id == id)
                .map(|c| (c.id.clone(), c.name.clone())),
            _ => None,
        };
        let options = match &resolution {
            Resolution::Ambiguous { options } => options.clone(),
            _ => Vec::new(),
        };

        let kit = self.speech.for_user(user_id).await;
        let mut allowed = req.tool_refs.clone();
        allowed.extend(conversations.iter().map(|c| c.name.clone()));
        allowed.extend(projects.iter().map(|p| p.name.clone()));
        let refined = refine_draft(
            kit.refiner.as_deref(),
            &RefineInput {
                transcript: req.transcript.clone(),
                target_name: target.as_ref().map(|(_, n)| n.clone()).unwrap_or_default(),
                allowed_references: allowed,
                tool_snippets: req.tool_snippets.clone(),
            },
        )
        .await;

        let (delivery, project_id) = match (action, &target) {
            (DraftAction::CreateConversation, Some((pid, _))) => (Delivery::NewConversation, Some(pid.clone())),
            (DraftAction::SendMessage, Some((cid, _))) => (self.delivery_for(user_id, cid).await, None),
            _ => (Delivery::NewTurn, None),
        };
        let state = if target.is_some() {
            DraftState::Drafted
        } else {
            DraftState::Ambiguous
        };
        let target_id = target.as_ref().map(|(id, _)| id.clone());
        let now = now_ms();
        let row = VoiceDraftRow {
            id: id.to_owned(),
            version,
            user_id: user_id.to_owned(),
            session_id: session_id.map(str::to_owned),
            action: action.as_str().to_owned(),
            state: state.as_str().to_owned(),
            queued_reason: None,
            target_id: target_id.clone(),
            target_name: target.as_ref().map(|(_, n)| n.clone()),
            project_id: project_id.clone(),
            text: Some(refined.text.clone()),
            payload_hash: payload_hash(action, target_id.as_deref(), project_id.as_deref(), &refined.text),
            readback_id: None,
            readback_text: None,
            spoken_confirm: false,
            delivery: Some(delivery.as_str().to_owned()),
            hold_deadline_at: None,
            send_started_at: None,
            outcome_msg_id: None,
            outcome_turn_id: None,
            lease_owner: None,
            lease_until: None,
            expires_at: Some(now + DRAFT_TTL_MS),
            created_at: now,
            updated_at: now,
        };
        if let Err(err) = self.repo.insert_draft(&row).await {
            tracing::error!(error = %err, "voice draft insert failed");
            return None;
        }
        self.extras.insert(
            (row.id.clone(), row.version),
            DraftExtras {
                diff: diff(&req.transcript, &refined.text, &refined.references),
                options: options.clone(),
            },
        );
        tracing::info!(draft_id = %row.id, version, state = %row.state, payload_hash = %row.payload_hash,
            refined = !refined.fell_back, "voice draft created");

        // The read-back clip (or the "A or B?" question).
        let spoken = match &target {
            Some((_, name)) => readback_text(name, &refined.summary, delivery),
            None if options.is_empty() => "Which conversation should it go to?".to_owned(),
            None => format!(
                "Which one: {}?",
                options
                    .iter()
                    .map(|(_, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", or ")
            ),
        };
        let mut readback_id = None;
        let mut spoken_confirm = false;
        if let Some(synth) = &kit.synth {
            match self
                .synthesize_metered(synth.as_ref(), user_id, session_id, &spoken, ClipVoice::Jarvis)
                .await
            {
                Ok(clip) => {
                    if let Some((_, name)) = &target
                        && !contains_confirm_grammar(&spoken)
                    {
                        spoken_confirm = self
                            .name_check
                            .passes(kit.transcriber.as_deref(), &clip, name, DICTIONARY_VERSION)
                            .await;
                    }
                    readback_id = Some(self.clips.insert(user_id, session_id, clip));
                }
                Err(err) => tracing::warn!(error = %err, "voice read-back synthesis failed; tap-confirm only"),
            }
        }
        let patched = self
            .transition(
                &row,
                None,
                DraftPatch {
                    readback_id: Some(readback_id.as_deref()),
                    readback_text: Some(Some(&spoken)),
                    spoken_confirm: Some(spoken_confirm),
                    expires_at: Some(Some(now_ms() + DRAFT_TTL_MS)),
                    ..Default::default()
                },
            )
            .await;
        let row = if patched {
            self.reload(&row).await.unwrap_or(row)
        } else {
            row
        };
        self.emit(&row, None);
        if let Some(clip_id) = &readback_id {
            self.sockets.send_user(
                user_id,
                &VoiceServerFrame::ReadbackReady(VoiceReadbackReady {
                    voice_session_id: session_id.map(str::to_owned),
                    draft_id: row.id.clone(),
                    version: row.version,
                    readback_id: clip_id.clone(),
                    url: format!("/api/voice/readbacks/{clip_id}"),
                }),
            );
        }
        Some(row)
    }

    async fn delivery_for(&self, user_id: &str, conversation_id: &str) -> Delivery {
        match self.conversations.status(user_id, conversation_id).await {
            Ok(Some(status)) if status.pending_confirmations > 0 => Delivery::QueueApproval,
            Ok(Some(status)) if status.is_processing && status.can_steer => Delivery::Steer,
            Ok(Some(status)) if status.is_processing => Delivery::QueueBusy,
            _ => Delivery::NewTurn,
        }
    }

    /// "Change it to…": supersede and build a fresh version (new read-back).
    pub async fn edit(&self, row: &VoiceDraftRow, change: &str) -> Option<VoiceDraftRow> {
        if !self
            .transition(row, Some(DraftState::Superseded), DraftPatch::default())
            .await
        {
            return None;
        }
        let original = row.text.clone().unwrap_or_default();
        let target = row.target_id.clone().zip(row.target_name.clone());
        self.build_version(
            &row.user_id,
            row.session_id.as_deref(),
            &row.id,
            row.version + 1,
            DraftRequest {
                transcript: format!("{original}\nChange: {change}"),
                new_conversation: row.action == DraftAction::CreateConversation.as_str(),
                ..Default::default()
            },
            target.filter(|_| row.action == DraftAction::SendMessage.as_str()),
        )
        .await
    }

    /// Answer to "A or B?": pick among the offered options only.
    pub async fn choose(&self, row: &VoiceDraftRow, utterance: &str) -> Option<VoiceDraftRow> {
        let options = self
            .extras
            .get(&(row.id.clone(), row.version))
            .map(|e| e.options.clone())?;
        let candidates: Vec<Candidate> = options
            .iter()
            .map(|(id, name)| Candidate {
                id: id.clone(),
                name: name.clone(),
                project_name: None,
            })
            .collect();
        let Resolution::Unique { id, name, .. } = resolve(utterance, &candidates, None) else {
            return None;
        };
        if !self
            .transition(row, Some(DraftState::Superseded), DraftPatch::default())
            .await
        {
            return None;
        }
        self.build_version(
            &row.user_id,
            row.session_id.as_deref(),
            &row.id,
            row.version + 1,
            DraftRequest {
                transcript: row.text.clone().unwrap_or_default(),
                ..Default::default()
            },
            Some((id, name)),
        )
        .await
    }

    // ---------------------------------------------------------- confirmation

    /// A tap carrying the version hash, or complete spoken evidence: commit
    /// (→ `committing`, 4 s hold). Returns the committing row.
    pub async fn confirm(&self, row: &VoiceDraftRow, hash: Option<&str>) -> Option<VoiceDraftRow> {
        if let Some(hash) = hash
            && hash != row.payload_hash
        {
            tracing::warn!(draft_id = %row.id, "voice confirm refused: stale draft hash");
            return None;
        }
        row.target_id.as_ref()?;
        let confirmed = self
            .transition_and_emit(row, DraftState::Confirmed, DraftPatch::default())
            .await?;
        let deadline = now_ms() + super::evidence::HOLD.as_millis() as i64;
        let applied = self
            .transition(
                &confirmed,
                Some(DraftState::Committing),
                DraftPatch {
                    hold_deadline_at: Some(Some(deadline)),
                    ..Default::default()
                },
            )
            .await;
        if !applied {
            return None;
        }
        let committing = self.reload(&confirmed).await?;
        self.emit(&committing, Some((super::evidence::HOLD.as_millis() as u64, false)));
        Some(committing)
    }

    /// Tap or spoken cancel: wins from any state before `sending`.
    pub async fn cancel(&self, row: &VoiceDraftRow) -> bool {
        self.transition_and_emit(row, DraftState::Cancelled, DraftPatch::default())
            .await
            .is_some()
    }

    /// Kill switch: cancel every pending draft of the user.
    pub async fn cancel_all_for_user(&self, user_id: &str) -> usize {
        let states = [
            "drafted",
            "ambiguous",
            "pending_transcript",
            "confirmed",
            "committing",
            "queued",
        ];
        let rows = self
            .repo
            .user_drafts_in_states(user_id, &states)
            .await
            .unwrap_or_default();
        let mut cancelled = 0;
        for row in rows {
            if self.cancel(&row).await {
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Model-issued act on an unconfirmed draft (rejected; audited by caller).
    pub fn reject_act(&self, name: &str) {
        tracing::warn!(tool = name, "voice act tool rejected: no user confirmation");
    }

    // --------------------------------------------------------------- sending

    /// Hold due: claim the send (`committing → sending`) and deliver.
    pub async fn start_send(&self, row: &VoiceDraftRow) -> Option<Delivered> {
        let sending = self
            .transition_and_emit(
                row,
                DraftState::Sending,
                DraftPatch {
                    send_started_at: Some(Some(now_ms())),
                    hold_deadline_at: Some(None),
                    ..Default::default()
                },
            )
            .await?;
        Some(self.deliver(&sending).await)
    }

    /// Send a `sending` (or re-claimed `queued`) draft with its idempotency
    /// key. Safe to repeat: the key returns the original message.
    pub async fn deliver(&self, row: &VoiceDraftRow) -> Delivered {
        let name = row.target_name.clone().unwrap_or_default();
        let key = idempotency_key(&row.id, row.version);
        let user = row.user_id.clone();
        let target_conversation = if row.action == DraftAction::CreateConversation.as_str() {
            let title: String = row
                .text
                .as_deref()
                .unwrap_or("Voice request")
                .split_whitespace()
                .filter(|w| !w.ends_with(':'))
                .take(6)
                .collect::<Vec<_>>()
                .join(" ");
            match self
                .conversations
                .create_conversation(&user, row.project_id.as_deref(), &title, &key)
                .await
            {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(draft_id = %row.id, error = %err, "voice create_conversation failed");
                    self.finish(row, DraftState::Failed, None, None).await;
                    self.outcome_line(
                        row,
                        "failed",
                        &format!("The new conversation in {name} could not be created."),
                    )
                    .await;
                    return Delivered::Failed;
                }
            }
        } else {
            let Some(target) = row.target_id.clone() else {
                self.finish(row, DraftState::Failed, None, None).await;
                return Delivered::Failed;
            };
            // A turn blocked on an approval answers 409 like a busy one;
            // read it first so the read-out is truthful.
            if let Ok(Some(status)) = self.conversations.status(&user, &target).await
                && status.pending_confirmations > 0
            {
                return self.queue(row, "awaiting_approval").await;
            }
            target
        };
        let text = row.text.clone().unwrap_or_default();
        match self
            .conversations
            .send_message(&user, &target_conversation, &text, &key)
            .await
        {
            SendOutcome::Delivered {
                msg_id,
                turn_id,
                midturn,
            } => {
                if is_voice_mode(row) {
                    // The bound conversation's follow-ups come from the binding.
                    self.spoke.remove(&target_conversation);
                } else {
                    self.watched.insert(target_conversation.clone(), user.clone());
                }
                if midturn {
                    self.finish(row, DraftState::Sent, Some(&msg_id), Some(&turn_id)).await;
                    let status = if is_voice_mode(row) { STATUS_ADDED } else { "sent" };
                    self.outcome_line(row, status, &format!("It joined the running turn of {name}."))
                        .await;
                    Delivered::Sent
                } else if row.action == DraftAction::CreateConversation.as_str() {
                    // A fresh conversation's first message always opens its turn.
                    self.finish(row, DraftState::Sent, Some(&msg_id), Some(&turn_id)).await;
                    self.outcome_line(row, "sent", &format!("A new conversation in {name} has it."))
                        .await;
                    Delivered::Sent
                } else {
                    // `sent` only once the turn started (§3.3.6): the worker
                    // probes after START_CHECK_MS.
                    self.transition(
                        row,
                        None,
                        DraftPatch {
                            outcome_msg_id: Some(Some(&msg_id)),
                            outcome_turn_id: Some(Some(&turn_id)),
                            ..Default::default()
                        },
                    )
                    .await;
                    Delivered::AwaitingStart
                }
            }
            SendOutcome::Busy => self.queue(row, "busy").await,
            SendOutcome::NotFound | SendOutcome::Rejected(_) => {
                self.finish(row, DraftState::Failed, None, None).await;
                self.outcome_line(row, "failed", &format!("The message for {name} failed."))
                    .await;
                Delivered::Failed
            }
        }
    }

    async fn queue(&self, row: &VoiceDraftRow, reason: &str) -> Delivered {
        let name = row.target_name.clone().unwrap_or_default();
        let applied = self
            .transition_and_emit(
                row,
                DraftState::Queued,
                DraftPatch {
                    queued_reason: Some(Some(reason)),
                    // A voice message keeps its own 30 min from when you said it.
                    expires_at: (!is_voice_mode(row)).then_some(Some(now_ms() + QUEUE_TTL_MS)),
                    ..Default::default()
                },
            )
            .await;
        if applied.is_none() {
            return Delivered::Lost;
        }
        if is_voice_mode(row) {
            // Said once when it was first queued; retries stay quiet.
            return Delivered::Queued;
        }
        let line = if reason == "busy" {
            format!("Queued for {name} until its turn ends.")
        } else {
            format!("{name} is waiting for an approval; the message waits too.")
        };
        self.outcome_line(row, "queued", &line).await;
        Delivered::Queued
    }

    async fn finish(&self, row: &VoiceDraftRow, state: DraftState, msg_id: Option<&str>, turn_id: Option<&str>) {
        self.transition_and_emit(
            row,
            state,
            DraftPatch {
                outcome_msg_id: msg_id.map(Some),
                outcome_turn_id: turn_id.map(Some),
                clear_lease: true,
                ..Default::default()
            },
        )
        .await;
    }

    pub(crate) async fn outcome_line(&self, row: &VoiceDraftRow, status: &str, line: &str) {
        let conversation = row.target_id.clone().unwrap_or_default();
        if is_voice_mode(row) {
            // Conversation mode: the message is visible in the chat; outcomes
            // are text for the voice bar, never a clip over the agent.
            let line = match status {
                STATUS_ADDED => "Added to the running task.",
                STATUS_SENT => "Sent.",
                "saved_not_started" => "Saved, but the turn did not start.",
                "expired" => "Your queued message expired before the turn ended.",
                _ => "Your message could not be sent.",
            };
            self.voice_line(&row.user_id, row.session_id.as_deref(), &conversation, status, line);
            return;
        }
        self.speak_line(
            &row.user_id,
            row.session_id.as_deref(),
            &conversation,
            status,
            line,
            ClipVoice::Jarvis,
        )
        .await;
    }

    /// A non-steered send: `sent` once a turn ran or replied, `failed` on a
    /// failure tip, else `saved_not_started` (§3.3.6).
    pub async fn check_started(&self, row: &VoiceDraftRow) {
        let (Some(target), Some(msg_id)) = (row.target_id.clone(), row.outcome_msg_id.clone()) else {
            return;
        };
        let name = row.target_name.clone().unwrap_or_default();
        let conversation = target;
        let Ok(probe) = self
            .conversations
            .probe_send(&row.user_id, &conversation, &msg_id)
            .await
        else {
            return;
        };
        if probe.failed {
            self.finish(row, DraftState::Failed, None, None).await;
            self.outcome_line(row, "failed", &format!("The message for {name} failed."))
                .await;
        } else if probe.turn_active || probe.replied {
            self.finish(row, DraftState::Sent, None, None).await;
            self.outcome_line(row, "sent", &format!("{name} has it.")).await;
        } else {
            self.finish(row, DraftState::SavedNotStarted, None, None).await;
            self.outcome_line(
                row,
                "saved_not_started",
                &format!("Saved in {name}, but it did not start. Tap to start it."),
            )
            .await;
        }
    }

    /// Retry a queued draft (target turn ended / approval removed / tick).
    pub async fn retry_queued(&self, row: &VoiceDraftRow) -> Option<Delivered> {
        if is_voice_mode(row)
            && let Some(target) = row.target_id.as_deref()
            && let Some(earlier) = self.oldest_queued_voice(&row.user_id, target, Some(&row.id)).await
            && (earlier.created_at, earlier.id.as_str()) < (row.created_at, row.id.as_str())
        {
            return None;
        }
        if row.expires_at.is_some_and(|t| t <= now_ms()) {
            self.transition_and_emit(row, DraftState::Expired, DraftPatch::default())
                .await?;
            let name = row.target_name.clone().unwrap_or_default();
            self.outcome_line(row, "expired", &format!("The message for {name} expired."))
                .await;
            return None;
        }
        let sending = self
            .transition_and_emit(
                row,
                DraftState::Sending,
                DraftPatch {
                    queued_reason: Some(None),
                    ..Default::default()
                },
            )
            .await?;
        Some(self.deliver(&sending).await)
    }

    /// Latest non-terminal draft of a session (for edits and taps).
    pub async fn latest_open(&self, user_id: &str, session_id: &str) -> Option<VoiceDraftRow> {
        let states = ["drafted", "ambiguous", "pending_transcript", "confirmed", "committing"];
        self.repo
            .user_drafts_in_states(user_id, &states)
            .await
            .ok()?
            .into_iter()
            .filter(|r| r.session_id.as_deref() == Some(session_id))
            .max_by_key(|r| (r.updated_at, r.version))
    }

    /// The user's newest draft still waiting for a confirmation (any session).
    pub async fn pending_for_user(&self, user_id: &str) -> Option<VoiceDraftRow> {
        let states = ["drafted", "ambiguous", "pending_transcript"];
        let now = now_ms();
        self.repo
            .user_drafts_in_states(user_id, &states)
            .await
            .ok()?
            .into_iter()
            .filter(|r| r.expires_at.is_none_or(|t| t > now))
            .max_by_key(|r| (r.updated_at, r.version))
    }

    /// A new call started while a draft waits: bind it to that call, read it
    /// back again (fresh clip, new expiry) and show the card, so it can be
    /// confirmed by voice there — not only by tap.
    pub async fn reannounce(&self, row: &VoiceDraftRow, session_id: &str) -> Option<VoiceDraftRow> {
        let row = if row.state == DraftState::PendingTranscript.as_str() {
            self.back_to_drafted(row).await?
        } else {
            row.clone()
        };
        let spoken = row.readback_text.clone()?;
        let readback_id = match self.speech.for_user(&row.user_id).await.synth {
            Some(synth) => match self
                .synthesize_metered(
                    synth.as_ref(),
                    &row.user_id,
                    Some(session_id),
                    &spoken,
                    ClipVoice::Jarvis,
                )
                .await
            {
                Ok(clip) => Some(self.clips.insert(&row.user_id, Some(session_id), clip)),
                Err(err) => {
                    tracing::warn!(error = %err, "voice read-back synthesis failed; tap-confirm only");
                    None
                }
            },
            None => None,
        };
        let applied = self
            .transition(
                &row,
                None,
                DraftPatch {
                    session_id: Some(Some(session_id)),
                    readback_id: Some(readback_id.as_deref()),
                    expires_at: Some(Some(now_ms() + DRAFT_TTL_MS)),
                    ..Default::default()
                },
            )
            .await;
        if !applied {
            return None;
        }
        let fresh = self.reload(&row).await?;
        tracing::info!(draft_id = %fresh.id, version = fresh.version, "voice draft re-announced to a new call");
        self.emit(&fresh, None);
        if let Some(clip_id) = &readback_id {
            self.sockets.send_user(
                &fresh.user_id,
                &VoiceServerFrame::ReadbackReady(VoiceReadbackReady {
                    voice_session_id: Some(session_id.to_owned()),
                    draft_id: fresh.id.clone(),
                    version: fresh.version,
                    readback_id: clip_id.clone(),
                    url: format!("/api/voice/readbacks/{clip_id}"),
                }),
            );
        }
        Some(fresh)
    }

    /// Mark `pending_transcript` while a confirm-like utterance is evaluated.
    pub async fn mark_pending(&self, row: &VoiceDraftRow) -> Option<VoiceDraftRow> {
        self.transition_and_emit(row, DraftState::PendingTranscript, DraftPatch::default())
            .await
    }

    /// Back to `drafted` after a rejected utterance.
    pub async fn back_to_drafted(&self, row: &VoiceDraftRow) -> Option<VoiceDraftRow> {
        self.transition_and_emit(row, DraftState::Drafted, DraftPatch::default())
            .await
    }

    pub(crate) async fn expire(&self, row: &VoiceDraftRow) -> bool {
        self.transition_and_emit(row, DraftState::Expired, DraftPatch::default())
            .await
            .is_some()
    }

    // ------------------------------------------------- conversation mode (C26)

    /// Post a conversation-mode utterance into the bound conversation:
    /// verbatim + `[[AION_VOICE]]`, mid-turn when a turn runs; queued for the
    /// worker when an approval is open or the agent cannot be steered.
    pub(crate) async fn send_voice_message(
        &self,
        user_id: &str,
        session_id: &str,
        conversation_id: &str,
        transcript: &str,
        car: bool,
    ) -> Delivered {
        let text = voice_message(transcript, car);
        let id = aionui_common::generate_prefixed_id("vd");
        let key = idempotency_key(&id, 1);
        let status = match self.conversations.status(user_id, conversation_id).await {
            Ok(Some(status)) => status,
            Ok(None) => {
                self.voice_line(
                    user_id,
                    Some(session_id),
                    conversation_id,
                    STATUS_FAILED,
                    "This conversation no longer exists.",
                );
                return Delivered::Failed;
            }
            Err(err) => {
                tracing::warn!(error = %err, "voice conversation status unavailable; sending anyway");
                Default::default()
            }
        };
        if status.pending_confirmations > 0 {
            return self
                .queue_voice_message(user_id, session_id, conversation_id, &id, &text, "awaiting_approval")
                .await;
        }
        // Your words keep their order: never overtake an earlier queued one.
        if let Some(earlier) = self.oldest_queued_voice(user_id, conversation_id, None).await {
            let reason = earlier.queued_reason.as_deref().unwrap_or("busy").to_owned();
            return self
                .queue_voice_message(user_id, session_id, conversation_id, &id, &text, &reason)
                .await;
        }
        match self
            .conversations
            .send_message(user_id, conversation_id, &text, &key)
            .await
        {
            SendOutcome::Delivered { midturn, .. } => {
                self.spoke.remove(conversation_id);
                let (status, line) = if midturn {
                    (STATUS_ADDED, "Added to the running task.")
                } else {
                    (STATUS_SENT, "Sent.")
                };
                tracing::info!(conversation_id, midturn, "voice conversation-mode message delivered");
                self.voice_line(user_id, Some(session_id), conversation_id, status, line);
                if midturn {
                    Delivered::Sent
                } else {
                    Delivered::AwaitingStart
                }
            }
            SendOutcome::Busy => {
                self.queue_voice_message(user_id, session_id, conversation_id, &id, &text, "busy")
                    .await
            }
            SendOutcome::NotFound | SendOutcome::Rejected(_) => {
                self.voice_line(
                    user_id,
                    Some(session_id),
                    conversation_id,
                    STATUS_FAILED,
                    "Your message could not be sent.",
                );
                Delivered::Failed
            }
        }
    }

    /// Persist a conversation-mode message as a queued `voice_drafts` row:
    /// the C9 worker sends it with the same key when the turn ends or the
    /// approval clears, and after a restart (no duplicates).
    async fn queue_voice_message(
        &self,
        user_id: &str,
        session_id: &str,
        conversation_id: &str,
        id: &str,
        text: &str,
        reason: &str,
    ) -> Delivered {
        let name = self
            .conversations
            .list_conversations(user_id)
            .await
            .ok()
            .and_then(|list| list.into_iter().find(|c| c.id == conversation_id).map(|c| c.name))
            .unwrap_or_default();
        let now = now_ms();
        let row = VoiceDraftRow {
            id: id.to_owned(),
            version: 1,
            user_id: user_id.to_owned(),
            session_id: Some(session_id.to_owned()),
            action: DraftAction::SendMessage.as_str().to_owned(),
            state: DraftState::Queued.as_str().to_owned(),
            queued_reason: Some(reason.to_owned()),
            target_id: Some(conversation_id.to_owned()),
            target_name: Some(name),
            project_id: None,
            text: Some(text.to_owned()),
            payload_hash: payload_hash(DraftAction::SendMessage, Some(conversation_id), None, text),
            readback_id: None,
            readback_text: None,
            spoken_confirm: false,
            delivery: Some(VOICE_MODE_DELIVERY.to_owned()),
            hold_deadline_at: None,
            send_started_at: None,
            outcome_msg_id: None,
            outcome_turn_id: None,
            lease_owner: None,
            lease_until: None,
            expires_at: Some(now + VOICE_QUEUE_TTL_MS),
            created_at: now,
            updated_at: now,
        };
        if let Err(err) = self.repo.insert_draft(&row).await {
            tracing::error!(error = %err, "voice conversation-mode queue insert failed");
            self.voice_line(
                user_id,
                Some(session_id),
                conversation_id,
                STATUS_FAILED,
                "Your message could not be sent.",
            );
            return Delivered::Failed;
        }
        tracing::info!(draft_id = %id, conversation_id, reason, "voice conversation-mode message queued");
        let line = if reason == "busy" {
            "Queued until the current turn ends."
        } else {
            "An approval is open on your screen; your message waits for it."
        };
        self.voice_line(user_id, Some(session_id), conversation_id, STATUS_QUEUED, line);
        Delivered::Queued
    }

    /// The oldest queued conversation-mode message for a conversation (other
    /// than `except`): later ones wait behind it (FIFO by creation).
    pub(crate) async fn oldest_queued_voice(
        &self,
        user_id: &str,
        conversation_id: &str,
        except: Option<&str>,
    ) -> Option<VoiceDraftRow> {
        self.repo
            .user_drafts_in_states(user_id, &["queued", "sending"])
            .await
            .ok()?
            .into_iter()
            .filter(|r| {
                is_voice_mode(r)
                    && r.target_id.as_deref() == Some(conversation_id)
                    && except.is_none_or(|id| r.id != id)
                    && (r.state == "queued" || r.outcome_msg_id.is_none())
            })
            .min_by_key(|r| (r.created_at, r.id.clone()))
    }

    /// "Cancel that": drop the user's queued conversation-mode messages for
    /// this conversation. Returns how many were cancelled.
    pub(crate) async fn cancel_voice_queue(&self, user_id: &str, conversation_id: &str) -> usize {
        let rows = self
            .repo
            .user_drafts_in_states(user_id, &["queued"])
            .await
            .unwrap_or_default();
        let mut cancelled = 0;
        for row in rows
            .iter()
            .filter(|r| is_voice_mode(r) && r.target_id.as_deref() == Some(conversation_id))
        {
            if self.cancel(row).await {
                cancelled += 1;
            }
        }
        cancelled
    }

    pub(crate) async fn interrupt(&self, row: &VoiceDraftRow) -> bool {
        let name = row.target_name.clone().unwrap_or_default();
        let applied = self
            .transition_and_emit(row, DraftState::Interrupted, DraftPatch::default())
            .await
            .is_some();
        if applied {
            self.outcome_line(
                row,
                "interrupted",
                &format!("The message for {name} was held when the backend restarted; ask me again to deliver it."),
            )
            .await;
        }
        applied
    }
}
