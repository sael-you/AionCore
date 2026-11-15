-- Migration 902 (youssef range 900–999): Voice Concierge "Jarvis" (card C9,
-- design/voice/PLAN.md v15 §3). All voice-owned tables live here so the
-- domain stays in one schema step. S1a uses voice_settings, voice_sessions and
-- voice_audit; voice_drafts, voice_tool_calls and voice_transcripts are filled
-- from S1b, voice_devices and voice_pairing_codes from S3. Migration 903 is
-- reserved for the S1b core change (idempotent message sends, §3.3.6).
--
-- Money is stored as integer micro-USD (1 USD = 1_000_000) to avoid float
-- drift in budget sums. Timestamps are epoch milliseconds like every table.

CREATE TABLE IF NOT EXISTS voice_settings (
    user_id             TEXT    PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Kill switch / deploy flag: voice is OFF until the user turns it on.
    enabled             INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    engine              TEXT    NOT NULL DEFAULT 'openai-realtime',
    model               TEXT    NOT NULL DEFAULT 'gpt-realtime-2.1',
    car_mode            INTEGER NOT NULL DEFAULT 0 CHECK (car_mode IN (0, 1)),
    voice               TEXT    NOT NULL DEFAULT 'marin',
    eagerness           TEXT    NOT NULL DEFAULT 'auto',
    daily_budget_micros INTEGER NOT NULL DEFAULT 5000000 CHECK (daily_budget_micros >= 0),
    -- Optional pinned provider row; NULL = first eligible OpenAI provider.
    provider_id         TEXT,
    -- Forward-compatible settings added by later slices (JSON object).
    extra               TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(extra)),
    updated_at          INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS voice_sessions (
    id                  TEXT    PRIMARY KEY NOT NULL,
    user_id             TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    engine              TEXT    NOT NULL,
    model               TEXT    NOT NULL,
    -- Provider call id (OpenAI `rtc_…`), persisted so a restarted backend can
    -- re-attach a sideband to the live call (§3.6.4).
    call_id             TEXT,
    state               TEXT    NOT NULL CHECK (state IN ('starting', 'live', 'ended', 'failed')),
    end_reason          TEXT,
    resume_of           TEXT,
    device              TEXT,
    instructions_hash   TEXT,
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    cached_input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    responses           INTEGER NOT NULL DEFAULT 0,
    -- Responses whose usage was lost (lossy re-attach) and were charged at the
    -- full reservation (§3.5).
    unknown_responses   INTEGER NOT NULL DEFAULT 0,
    stt_seconds         INTEGER NOT NULL DEFAULT 0,
    cost_micros         INTEGER NOT NULL DEFAULT 0,
    created_at          INTEGER NOT NULL,
    ready_at            INTEGER,
    ended_at            INTEGER,
    updated_at          INTEGER NOT NULL
);
-- One live call per user (§3.6.1): the second admission hits this index.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voice_sessions_one_live
    ON voice_sessions (user_id) WHERE state IN ('starting', 'live');
CREATE INDEX IF NOT EXISTS idx_voice_sessions_user_created
    ON voice_sessions (user_id, created_at);

CREATE TABLE IF NOT EXISTS voice_audit (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    -- No FK: audit metadata outlives "delete my voice data" (§3.3.8).
    user_id     TEXT    NOT NULL,
    session_id  TEXT,
    event       TEXT    NOT NULL,
    -- Metadata only (ids, codes, amounts) — never transcripts or keys.
    detail      TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(detail)),
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_voice_audit_user_created ON voice_audit (user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_voice_audit_created ON voice_audit (created_at);

-- S1b: versioned drafts with CAS transitions (§3.3.1). One row per version.
CREATE TABLE IF NOT EXISTS voice_drafts (
    id               TEXT    NOT NULL,
    version          INTEGER NOT NULL CHECK (version >= 1),
    user_id          TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id       TEXT    REFERENCES voice_sessions(id) ON DELETE SET NULL,
    action           TEXT    NOT NULL CHECK (action IN ('send_message', 'create_conversation', 'create_card')),
    state            TEXT    NOT NULL CHECK (state IN (
                         'drafted', 'ambiguous', 'pending_transcript', 'confirmed', 'committing', 'sending',
                         'queued', 'sent', 'failed', 'saved_not_started', 'cancelled', 'expired',
                         'interrupted', 'superseded')),
    queued_reason    TEXT    CHECK (queued_reason IS NULL OR queued_reason IN ('busy', 'awaiting_approval')),
    target_id        TEXT,
    target_name      TEXT,
    -- Frozen action fields (JSON); text is purged after 30 days, the hash stays.
    payload          TEXT    CHECK (payload IS NULL OR json_valid(payload)),
    payload_hash     TEXT    NOT NULL,
    readback_id      TEXT,
    readback_text    TEXT,
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
CREATE INDEX IF NOT EXISTS idx_voice_drafts_state_lease ON voice_drafts (state, lease_until);

-- S1b: every provider tool call is executed once (dedupe on the provider call id).
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

-- S1b: short-lived transcripts (7 days, daily purge, §3.3.8).
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

-- S3: paired devices (`jarvis_link.py`). Only the token hash is stored.
CREATE TABLE IF NOT EXISTS voice_devices (
    id           TEXT    PRIMARY KEY NOT NULL,
    user_id      TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    token_hash   TEXT    NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_voice_devices_user ON voice_devices (user_id);

-- S3: single-use pairing codes (5 min, 5 failures → 15 min lockout).
CREATE TABLE IF NOT EXISTS voice_pairing_codes (
    id              TEXT    PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash       TEXT    NOT NULL UNIQUE,
    expires_at      INTEGER NOT NULL,
    used_at         INTEGER,
    failed_attempts INTEGER NOT NULL DEFAULT 0,
    locked_until    INTEGER,
    created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_voice_pairing_codes_user ON voice_pairing_codes (user_id);
