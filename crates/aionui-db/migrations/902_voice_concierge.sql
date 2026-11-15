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

-- Spend per user, session and UTC day, written when usage is charged, so the
-- daily budget counts cost on the day it was incurred (a call that crosses
-- midnight is split).
CREATE TABLE IF NOT EXISTS voice_usage_daily (
    user_id      TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id   TEXT    NOT NULL REFERENCES voice_sessions(id) ON DELETE CASCADE,
    day_start    INTEGER NOT NULL,
    cost_micros  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, session_id, day_start)
);
CREATE INDEX IF NOT EXISTS idx_voice_usage_daily_user_day ON voice_usage_daily (user_id, day_start);

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
