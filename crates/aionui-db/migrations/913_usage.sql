-- Migration 913 (youssef range 900–999): per-turn usage + daily aggregate
-- (card C42 project page, TASK-169; folds in the storage part of TASK-160).
-- Frozen once deployed: never edit or renumber; a change is a new 9NN file.
--
-- `usage_turns`: one row per usage report of a finished turn (the per-turn
-- token split a backend reports, never the running context occupancy).
-- `usage_daily`: the same numbers summed per (user, project, UTC day, backend,
-- model), upserted in the same transaction; the analytics read path touches
-- only this aggregate (plus one indexed range query for the top conversations).
-- `project_id` is '' for a conversation outside any project. `cost_micro_usd`
-- is micro-USD and NULL when the backend reported no cost.
--
-- History before this migration is not back-filled.
--
-- Rollback: additive only (two new tables). An older binary refuses a DB
-- holding 913, so a rollback restores the pre-deploy backup; lost = the usage
-- rows recorded since the deploy, nothing else.

CREATE TABLE IF NOT EXISTS usage_turns (
    id                 TEXT    PRIMARY KEY,
    user_id            TEXT    NOT NULL,
    conversation_id    TEXT    NOT NULL,
    project_id         TEXT    NOT NULL DEFAULT '',
    backend            TEXT    NOT NULL DEFAULT '',
    model              TEXT    NOT NULL DEFAULT '',
    started_at         INTEGER NOT NULL,  -- epoch ms (when the report was recorded)
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    thought_tokens     INTEGER NOT NULL DEFAULT 0,
    cost_micro_usd     INTEGER
);

CREATE INDEX IF NOT EXISTS idx_usage_turns_user_project_started
    ON usage_turns (user_id, project_id, started_at);

CREATE TABLE IF NOT EXISTS usage_daily (
    user_id            TEXT    NOT NULL,
    project_id         TEXT    NOT NULL DEFAULT '',
    day                TEXT    NOT NULL,  -- YYYY-MM-DD (UTC)
    backend            TEXT    NOT NULL DEFAULT '',
    model              TEXT    NOT NULL DEFAULT '',
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    thought_tokens     INTEGER NOT NULL DEFAULT 0,
    cost_micro_usd     INTEGER,
    turns              INTEGER NOT NULL DEFAULT 0,
    first_at           INTEGER NOT NULL,  -- epoch ms of the first report of the bucket
    PRIMARY KEY (user_id, project_id, day, backend, model)
);
