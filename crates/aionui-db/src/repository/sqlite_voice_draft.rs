use aionui_common::TimestampMs;
use sqlx::{QueryBuilder, Sqlite};

use crate::error::DbError;
use crate::models::VoiceDraftRow;
use crate::repository::sqlite_voice::SqliteVoiceRepository;
use crate::repository::voice_draft::{DraftTransition, IVoiceDraftRepository, NewVoiceTranscript};

fn push_states(builder: &mut QueryBuilder<'_, Sqlite>, states: &[&str]) {
    builder.push(" (");
    let mut separated = builder.separated(", ");
    for state in states {
        separated.push_bind(state.to_string());
    }
    separated.push_unseparated(")");
}

#[async_trait::async_trait]
impl IVoiceDraftRepository for SqliteVoiceRepository {
    async fn insert_draft(&self, row: &VoiceDraftRow) -> Result<(), DbError> {
        let result = sqlx::query(
            "INSERT INTO voice_drafts \
                (id, version, user_id, session_id, action, state, queued_reason, target_id, target_name, \
                 project_id, text, payload_hash, readback_id, readback_text, spoken_confirm, delivery, \
                 hold_deadline_at, send_started_at, outcome_msg_id, outcome_turn_id, lease_owner, lease_until, \
                 expires_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(row.version)
        .bind(&row.user_id)
        .bind(&row.session_id)
        .bind(&row.action)
        .bind(&row.state)
        .bind(&row.queued_reason)
        .bind(&row.target_id)
        .bind(&row.target_name)
        .bind(&row.project_id)
        .bind(&row.text)
        .bind(&row.payload_hash)
        .bind(&row.readback_id)
        .bind(&row.readback_text)
        .bind(row.spoken_confirm)
        .bind(&row.delivery)
        .bind(row.hold_deadline_at)
        .bind(row.send_started_at)
        .bind(&row.outcome_msg_id)
        .bind(&row.outcome_turn_id)
        .bind(&row.lease_owner)
        .bind(row.lease_until)
        .bind(row.expires_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(self.pool())
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(err) => {
                let err = DbError::from(err);
                if err.is_unique_violation() {
                    Err(DbError::Conflict(format!("draft {}:{} exists", row.id, row.version)))
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn get_draft(&self, user_id: &str, id: &str, version: i64) -> Result<Option<VoiceDraftRow>, DbError> {
        Ok(sqlx::query_as::<_, VoiceDraftRow>(
            "SELECT * FROM voice_drafts WHERE user_id = ? AND id = ? AND version = ?",
        )
        .bind(user_id)
        .bind(id)
        .bind(version)
        .fetch_optional(self.pool())
        .await?)
    }

    async fn latest_draft(&self, user_id: &str, id: &str) -> Result<Option<VoiceDraftRow>, DbError> {
        Ok(sqlx::query_as::<_, VoiceDraftRow>(
            "SELECT * FROM voice_drafts WHERE user_id = ? AND id = ? ORDER BY version DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(id)
        .fetch_optional(self.pool())
        .await?)
    }

    async fn transition_draft(&self, t: &DraftTransition<'_>) -> Result<bool, DbError> {
        if t.from.is_empty() {
            return Ok(false);
        }
        let mut builder: QueryBuilder<'_, Sqlite> = QueryBuilder::new("UPDATE voice_drafts SET updated_at = ");
        builder.push_bind(aionui_common::now_ms());
        if let Some(to) = t.to {
            builder.push(", state = ").push_bind(to.to_owned());
        }
        let p = &t.patch;
        macro_rules! set_opt {
            ($field:literal, $value:expr) => {
                if let Some(value) = $value {
                    builder.push(concat!(", ", $field, " = ")).push_bind(value);
                }
            };
        }
        set_opt!("session_id", p.session_id.map(|v| v.map(str::to_owned)));
        set_opt!("queued_reason", p.queued_reason.map(|v| v.map(str::to_owned)));
        set_opt!("readback_id", p.readback_id.map(|v| v.map(str::to_owned)));
        set_opt!("readback_text", p.readback_text.map(|v| v.map(str::to_owned)));
        set_opt!("spoken_confirm", p.spoken_confirm);
        set_opt!("delivery", p.delivery.map(|v| v.map(str::to_owned)));
        set_opt!("hold_deadline_at", p.hold_deadline_at);
        set_opt!("send_started_at", p.send_started_at);
        set_opt!("outcome_msg_id", p.outcome_msg_id.map(|v| v.map(str::to_owned)));
        set_opt!("outcome_turn_id", p.outcome_turn_id.map(|v| v.map(str::to_owned)));
        set_opt!("expires_at", p.expires_at);
        if p.clear_lease {
            builder.push(", lease_owner = NULL, lease_until = NULL");
        }
        builder
            .push(" WHERE user_id = ")
            .push_bind(t.user_id.to_owned())
            .push(" AND id = ")
            .push_bind(t.id.to_owned())
            .push(" AND version = ")
            .push_bind(t.version)
            .push(" AND state IN");
        push_states(&mut builder, t.from);
        let result = builder.build().execute(self.pool()).await?;
        Ok(result.rows_affected() == 1)
    }

    async fn drafts_in_states(&self, states: &[&str]) -> Result<Vec<VoiceDraftRow>, DbError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let mut builder: QueryBuilder<'_, Sqlite> = QueryBuilder::new("SELECT * FROM voice_drafts WHERE state IN");
        push_states(&mut builder, states);
        builder.push(" ORDER BY updated_at ASC");
        Ok(builder.build_query_as::<VoiceDraftRow>().fetch_all(self.pool()).await?)
    }

    async fn user_drafts_in_states(&self, user_id: &str, states: &[&str]) -> Result<Vec<VoiceDraftRow>, DbError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let mut builder: QueryBuilder<'_, Sqlite> = QueryBuilder::new("SELECT * FROM voice_drafts WHERE user_id = ");
        builder.push_bind(user_id.to_owned()).push(" AND state IN");
        push_states(&mut builder, states);
        builder.push(" ORDER BY updated_at ASC");
        Ok(builder.build_query_as::<VoiceDraftRow>().fetch_all(self.pool()).await?)
    }

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
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE voice_drafts SET lease_owner = ?, lease_until = ?, updated_at = ? \
             WHERE user_id = ? AND id = ? AND version = ? AND state = ? \
               AND (lease_until IS NULL OR lease_until <= ? OR lease_owner = ?)",
        )
        .bind(owner)
        .bind(until)
        .bind(now)
        .bind(user_id)
        .bind(id)
        .bind(version)
        .bind(state)
        .bind(now)
        .bind(owner)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn begin_tool_call(&self, session_id: &str, call_id: &str, name: &str) -> Result<bool, DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO voice_tool_calls (session_id, call_id, name, status, created_at, updated_at) \
             VALUES (?, ?, ?, 'running', ?, ?)",
        )
        .bind(session_id)
        .bind(call_id)
        .bind(name)
        .bind(now)
        .bind(now)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn finish_tool_call(
        &self,
        session_id: &str,
        call_id: &str,
        status: &str,
        duration_ms: i64,
        result_bytes: i64,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE voice_tool_calls SET status = ?, duration_ms = ?, result_bytes = ?, updated_at = ? \
             WHERE session_id = ? AND call_id = ?",
        )
        .bind(status)
        .bind(duration_ms)
        .bind(result_bytes)
        .bind(aionui_common::now_ms())
        .bind(session_id)
        .bind(call_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    async fn add_transcript(&self, entry: &NewVoiceTranscript<'_>) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO voice_transcripts (session_id, user_id, turn_no, role, text, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(entry.session_id)
        .bind(entry.user_id)
        .bind(entry.turn_no)
        .bind(entry.role)
        .bind(entry.text)
        .bind(aionui_common::now_ms())
        .bind(entry.expires_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    async fn recent_transcripts(&self, session_id: &str, limit: u32) -> Result<Vec<(String, String)>, DbError> {
        let mut rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, text FROM voice_transcripts WHERE session_id = ? AND expires_at > ? \
             ORDER BY turn_no DESC, id DESC LIMIT ?",
        )
        .bind(session_id)
        .bind(aionui_common::now_ms())
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await?;
        rows.reverse();
        Ok(rows)
    }

    async fn purge_voice_data(&self, now: TimestampMs, draft_text_before: TimestampMs) -> Result<u64, DbError> {
        let transcripts = sqlx::query("DELETE FROM voice_transcripts WHERE expires_at <= ?")
            .bind(now)
            .execute(self.pool())
            .await?
            .rows_affected();
        let drafts = sqlx::query(
            "UPDATE voice_drafts SET text = NULL, readback_text = NULL \
             WHERE created_at < ? AND (text IS NOT NULL OR readback_text IS NOT NULL)",
        )
        .bind(draft_text_before)
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(transcripts + drafts)
    }
}

#[cfg(test)]
#[path = "sqlite_voice_draft_test.rs"]
mod sqlite_voice_draft_test;
