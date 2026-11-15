use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::VoiceDraftRow;

/// Voice drafts (versioned, CAS transitions), tool-call dedupe and
/// short-lived transcripts (migration 903).
#[async_trait::async_trait]
pub trait IVoiceDraftRepository: Send + Sync {
    /// Insert a draft version. `Conflict` if `(id, version)` exists.
    async fn insert_draft(&self, row: &VoiceDraftRow) -> Result<(), DbError>;

    async fn get_draft(&self, user_id: &str, id: &str, version: i64) -> Result<Option<VoiceDraftRow>, DbError>;

    /// Highest version of a draft.
    async fn latest_draft(&self, user_id: &str, id: &str) -> Result<Option<VoiceDraftRow>, DbError>;

    /// Compare-and-set transition: applies only while the row is at
    /// `(id, version)` and in one of `from`. Returns whether it applied.
    async fn transition_draft(&self, transition: &DraftTransition<'_>) -> Result<bool, DbError>;

    /// Drafts of every user in `states` (startup/periodic recovery).
    async fn drafts_in_states(&self, states: &[&str]) -> Result<Vec<VoiceDraftRow>, DbError>;

    /// The user's drafts in `states` (kill switch, session views).
    async fn user_drafts_in_states(&self, user_id: &str, states: &[&str]) -> Result<Vec<VoiceDraftRow>, DbError>;

    /// Take the recovery lease of a draft version still in `state`
    /// (free or expired lease only).
    #[allow(clippy::too_many_arguments)]
    async fn claim_draft_lease(
        &self,
        user_id: &str,
        id: &str,
        version: i64,
        state: &str,
        owner: &str,
        until: TimestampMs,
        now: TimestampMs,
    ) -> Result<bool, DbError>;

    /// Record a provider tool call; `false` when it was already recorded
    /// (dedupe on the provider call id).
    async fn begin_tool_call(&self, session_id: &str, call_id: &str, name: &str) -> Result<bool, DbError>;

    async fn finish_tool_call(
        &self,
        session_id: &str,
        call_id: &str,
        status: &str,
        duration_ms: i64,
        result_bytes: i64,
    ) -> Result<(), DbError>;

    async fn add_transcript(&self, entry: &NewVoiceTranscript<'_>) -> Result<(), DbError>;

    /// The last `limit` transcript turns of a session, oldest first, as
    /// `(role, text)`; expired turns are skipped.
    async fn recent_transcripts(&self, session_id: &str, limit: u32) -> Result<Vec<(String, String)>, DbError>;

    /// Delete expired transcripts and blank draft texts older than
    /// `draft_text_before` (the payload hash stays). Returns rows touched.
    async fn purge_voice_data(&self, now: TimestampMs, draft_text_before: TimestampMs) -> Result<u64, DbError>;
}

/// Fields a transition may change. `None` keeps the value; `Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct DraftPatch<'a> {
    /// Rebind the draft to another voice session (re-announced to a new call).
    pub session_id: Option<Option<&'a str>>,
    pub queued_reason: Option<Option<&'a str>>,
    pub readback_id: Option<Option<&'a str>>,
    pub readback_text: Option<Option<&'a str>>,
    pub spoken_confirm: Option<bool>,
    pub delivery: Option<Option<&'a str>>,
    pub hold_deadline_at: Option<Option<TimestampMs>>,
    pub send_started_at: Option<Option<TimestampMs>>,
    pub outcome_msg_id: Option<Option<&'a str>>,
    pub outcome_turn_id: Option<Option<&'a str>>,
    pub expires_at: Option<Option<TimestampMs>>,
    /// Release the recovery lease.
    pub clear_lease: bool,
}

#[derive(Debug, Clone)]
pub struct DraftTransition<'a> {
    pub user_id: &'a str,
    pub id: &'a str,
    pub version: i64,
    pub from: &'a [&'a str],
    /// New state; `None` keeps the state (field update under the same CAS).
    pub to: Option<&'a str>,
    pub patch: DraftPatch<'a>,
}

#[derive(Debug, Clone)]
pub struct NewVoiceTranscript<'a> {
    pub session_id: &'a str,
    pub user_id: &'a str,
    pub turn_no: i64,
    pub role: &'a str,
    pub text: &'a str,
    pub expires_at: TimestampMs,
}
