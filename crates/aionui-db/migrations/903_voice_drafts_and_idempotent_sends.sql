-- Migration 903 (youssef range 900–999): Voice Concierge slice S1b (card C9,
-- design/voice/PLAN.md v15 §3.3) — drafts, tool-call dedupe, transcripts —
-- and the core change it needs: idempotent user-message sends (§3.3.6).

-- Versioned drafts with compare-and-set transitions (§3.3.1). One row per
-- version; an edit inserts version n+1 and supersedes n.
CREATE TABLE IF NOT EXISTS voice_drafts (
    id               TEXT    NOT NULL,
    version          INTEGER NOT NULL CHECK (version >= 1),
    user_id          TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id       TEXT    REFERENCES voice_sessions(id) ON DELETE SET NULL,
    action           TEXT    NOT NULL CHECK (action IN ('send_message', 'create_conversation')),
    state            TEXT    NOT NULL CHECK (state IN (
                         'drafted', 'ambiguous', 'pending_transcript', 'confirmed', 'committing', 'sending',
                         'queued', 'sent', 'failed', 'saved_not_started', 'cancelled', 'expired',
                         'interrupted', 'superseded')),
    queued_reason    TEXT    CHECK (queued_reason IS NULL OR queued_reason IN ('busy', 'awaiting_approval')),
    target_id        TEXT,
    target_name      TEXT,
    project_id       TEXT,
    -- Frozen request text; purged after 30 days (the hash stays).
    text             TEXT,
    payload_hash     TEXT    NOT NULL,
    readback_id      TEXT,
    readback_text    TEXT,
    -- Spoken confirmation allowed (read-back name check passed, §3.3.3).
    spoken_confirm   INTEGER NOT NULL DEFAULT 0 CHECK (spoken_confirm IN (0, 1)),
    delivery         TEXT,
    hold_deadline_at INTEGER,
    send_started_at  INTEGER,
    outcome_msg_id   TEXT,
    outcome_turn_id  TEXT,
    lease_owner      TEXT,
    lease_until      INTEGER,
    expires_at       INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    PRIMARY KEY (id, version)
);
CREATE INDEX IF NOT EXISTS idx_voice_drafts_user_state ON voice_drafts (user_id, state);
CREATE INDEX IF NOT EXISTS idx_voice_drafts_state ON voice_drafts (state, updated_at);

-- Every provider tool call is executed once (dedupe on the provider call id).
CREATE TABLE IF NOT EXISTS voice_tool_calls (
    session_id   TEXT    NOT NULL REFERENCES voice_sessions(id) ON DELETE CASCADE,
    call_id      TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    status       TEXT    NOT NULL CHECK (status IN ('running', 'done', 'rejected', 'failed')),
    duration_ms  INTEGER,
    result_bytes INTEGER,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (session_id, call_id)
);

-- Short-lived transcripts (7 days, daily purge, §3.3.8).
CREATE TABLE IF NOT EXISTS voice_transcripts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT    NOT NULL REFERENCES voice_sessions(id) ON DELETE CASCADE,
    user_id     TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    turn_no     INTEGER NOT NULL,
    role        TEXT    NOT NULL CHECK (role IN ('user', 'assistant')),
    text        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_voice_transcripts_session ON voice_transcripts (session_id, turn_no);
CREATE INDEX IF NOT EXISTS idx_voice_transcripts_expires ON voice_transcripts (expires_at);

-- Idempotent user-message sends: a send that carries a key records
-- (msg_id, turn_id) here in the same transaction as the user message row; a
-- retry with the same key returns the stored ids instead of persisting a
-- second message or opening a second turn. Kept beside `messages` (not as
-- columns on it) so the message row model and its many writers stay unchanged.
CREATE TABLE IF NOT EXISTS message_idempotency_keys (
    conversation_id   TEXT    NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    idempotency_key   TEXT    NOT NULL,
    user_id           TEXT    NOT NULL,
    msg_id            TEXT    NOT NULL,
    turn_id           TEXT    NOT NULL,
    delivered_midturn INTEGER NOT NULL DEFAULT 0 CHECK (delivered_midturn IN (0, 1)),
    created_at        INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_message_idempotency_msg ON message_idempotency_keys (conversation_id, msg_id);
