-- Migration 901 (youssef range 900–999): built-in project board (Kanban).
--
-- One board per standard project, shared by every conversation of the
-- project. Additive only: new tables plus one partial unique index on
-- `conversations`; no column of an upstream table changes. Every table is
-- user-scoped (`user_id`) and, like 028_project_bind, carries no FOREIGN KEY
-- to upstream tables — the board crate validates ownership in the service
-- layer, and project deletion removes the board explicitly.
--
-- Frozen once deployed: later changes go into 902+.

-- Board header: on/off switch, template, TASK-N counter, settings version.
-- `next_number` is claimed with UPDATE … RETURNING inside the card-create
-- transaction; `version` guards settings and the columns collection.
-- Every board uses the department (`studio`) flow (D39); the column stays so a
-- later template needs no table rebuild. `keeper_conversation_id` is the
-- project's pinned Board Keeper conversation (D40), NULL until created.
CREATE TABLE IF NOT EXISTS board_settings (
    project_id  TEXT    PRIMARY KEY NOT NULL,
    user_id     TEXT    NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    template    TEXT    NOT NULL DEFAULT 'studio' CHECK (template IN ('studio')),
    next_number INTEGER NOT NULL DEFAULT 1,
    version     INTEGER NOT NULL DEFAULT 1,
    settings    TEXT    NOT NULL DEFAULT '{}',
    archived_at INTEGER,
    keeper_conversation_id TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_board_settings_user ON board_settings (user_id);

-- Columns. `name` NULL = the UI renders the i18n default `board.columns.<key>`.
CREATE TABLE IF NOT EXISTS board_columns (
    id          TEXT    PRIMARY KEY NOT NULL,
    project_id  TEXT    NOT NULL,
    user_id     TEXT    NOT NULL,
    key         TEXT    NOT NULL,
    name        TEXT,
    category    TEXT    NOT NULL CHECK (category IN ('backlog', 'todo', 'in_progress', 'done')),
    kind        TEXT    NOT NULL DEFAULT 'normal' CHECK (kind IN ('normal', 'gate', 'terminal')),
    resolution  TEXT    CHECK (resolution IS NULL OR resolution IN ('completed', 'dropped')),
    agent_move  TEXT    NOT NULL DEFAULT 'allow' CHECK (agent_move IN ('allow', 'deny')),
    wip_limit   INTEGER CHECK (wip_limit IS NULL OR wip_limit > 0),
    exit_rule   TEXT,
    gate        TEXT,
    position    INTEGER NOT NULL,
    archived    INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    UNIQUE (project_id, key)
);
CREATE INDEX IF NOT EXISTS idx_board_columns_user_project ON board_columns (user_id, project_id, position);

-- Cards. `id` is a UUID v7; `number` is the per-project TASK-N.
CREATE TABLE IF NOT EXISTS board_cards (
    id             TEXT    PRIMARY KEY NOT NULL,
    project_id     TEXT    NOT NULL,
    user_id        TEXT    NOT NULL,
    number         INTEGER NOT NULL,
    title          TEXT    NOT NULL,
    description    TEXT    NOT NULL DEFAULT '',
    type           TEXT    NOT NULL DEFAULT 'feature'
                           CHECK (type IN ('idea', 'bug', 'feature', 'update', 'chore', 'research')),
    priority       TEXT    NOT NULL DEFAULT 'medium' CHECK (priority IN ('low', 'medium', 'high', 'urgent')),
    size           TEXT    CHECK (size IS NULL OR size IN ('s', 'm', 'l')),
    tier           TEXT    CHECK (tier IS NULL OR tier IN ('t0', 't1', 't2', 't3')),
    column_key     TEXT    NOT NULL,
    position       INTEGER NOT NULL,
    labels         TEXT    NOT NULL DEFAULT '[]',
    skills         TEXT    NOT NULL DEFAULT '[]',
    routing        TEXT    NOT NULL DEFAULT '[]',
    creator_kind   TEXT    NOT NULL CHECK (creator_kind IN ('user', 'agent', 'system')),
    creator_ref    TEXT    NOT NULL,
    assignee_kind  TEXT    CHECK (assignee_kind IS NULL OR assignee_kind IN ('assistant', 'team', 'user')),
    assignee_ref   TEXT,
    blocked        INTEGER NOT NULL DEFAULT 0 CHECK (blocked IN (0, 1)),
    blocked_reason TEXT,
    change_id      TEXT,
    metadata       TEXT    NOT NULL DEFAULT '{}',
    version        INTEGER NOT NULL DEFAULT 1,
    archived_at    INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    UNIQUE (project_id, number)
);
CREATE INDEX IF NOT EXISTS idx_board_cards_column_order ON board_cards (project_id, column_key, position, id);
CREATE INDEX IF NOT EXISTS idx_board_cards_assignee ON board_cards (project_id, assignee_ref);
CREATE INDEX IF NOT EXISTS idx_board_cards_user ON board_cards (user_id, id);

-- One current verdict per department; history lives in board_events.
CREATE TABLE IF NOT EXISTS board_card_reviews (
    id              TEXT    PRIMARY KEY NOT NULL,
    card_id         TEXT    NOT NULL,
    project_id      TEXT    NOT NULL,
    user_id         TEXT    NOT NULL,
    department      TEXT    NOT NULL,
    reviewer_ref    TEXT    NOT NULL,
    conversation_id TEXT,
    verdict         TEXT    NOT NULL CHECK (verdict IN ('pending', 'approve', 'change', 'reject', 'blocked')),
    summary         TEXT    NOT NULL DEFAULT '',
    findings        TEXT    NOT NULL DEFAULT '[]',
    recommendation  TEXT    NOT NULL DEFAULT '',
    assumptions     TEXT    NOT NULL DEFAULT '[]',
    confidence      REAL,
    tokens_used     INTEGER NOT NULL DEFAULT 0,
    version         INTEGER NOT NULL DEFAULT 1,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (card_id, department)
);
CREATE INDEX IF NOT EXISTS idx_board_card_reviews_card ON board_card_reviews (card_id);

-- Dependencies: `card_id` waits for `depends_on_card_id` (same project only;
-- self-links and cycles are rejected by the service). `blocks` is derived.
CREATE TABLE IF NOT EXISTS board_card_deps (
    card_id            TEXT    NOT NULL,
    depends_on_card_id TEXT    NOT NULL,
    project_id         TEXT    NOT NULL,
    user_id            TEXT    NOT NULL,
    created_at         INTEGER NOT NULL,
    PRIMARY KEY (card_id, depends_on_card_id)
);
CREATE INDEX IF NOT EXISTS idx_board_card_deps_target ON board_card_deps (depends_on_card_id);

-- Idempotency keys for gate decisions and assignments. `claim_token` (the
-- fencing token of the current reservation holder) and `packet_message_id`
-- (stable id of the delivered packet message) are written by assign (S4);
-- they exist here because 901 is frozen once deployed.
CREATE TABLE IF NOT EXISTS board_idempotency (
    user_id           TEXT    NOT NULL,
    project_id        TEXT    NOT NULL,
    key               TEXT    NOT NULL,
    request_hash      TEXT    NOT NULL,
    state             TEXT    NOT NULL CHECK (state IN ('pending', 'done')),
    assignment_id     TEXT,
    response          TEXT,
    claim_token       TEXT,
    packet_message_id TEXT,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (user_id, project_id, key)
);

-- Deploy jobs: created by the Deploy approval, executed by the supervisor.
CREATE TABLE IF NOT EXISTS board_deploy_jobs (
    id              TEXT    PRIMARY KEY NOT NULL,
    card_id         TEXT    NOT NULL,
    project_id      TEXT    NOT NULL,
    user_id         TEXT    NOT NULL,
    idempotency_key TEXT    NOT NULL,
    state           TEXT    NOT NULL CHECK (state IN ('requested', 'running', 'ok', 'failed')),
    detail          TEXT    NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (user_id, project_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_board_deploy_jobs_card ON board_deploy_jobs (card_id);

-- The supervisor's acknowledged event checkpoint (bookkeeping, no events).
CREATE TABLE IF NOT EXISTS board_supervisor_cursor (
    project_id TEXT    PRIMARY KEY NOT NULL,
    user_id    TEXT    NOT NULL,
    last_seq   INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

-- Questions between departments; closed by a comment with `answers`.
CREATE TABLE IF NOT EXISTS board_card_questions (
    id                TEXT    PRIMARY KEY NOT NULL,
    card_id           TEXT    NOT NULL,
    project_id        TEXT    NOT NULL,
    user_id           TEXT    NOT NULL,
    from_department   TEXT    NOT NULL,
    to_department     TEXT    NOT NULL,
    text              TEXT    NOT NULL,
    answered_by_event TEXT,
    version           INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_board_card_questions_card ON board_card_questions (card_id);

-- Conversations linked to a card (the card's tree). At most one `lead`.
CREATE TABLE IF NOT EXISTS board_card_links (
    card_id         TEXT    NOT NULL,
    conversation_id TEXT    NOT NULL,
    project_id      TEXT    NOT NULL,
    user_id         TEXT    NOT NULL,
    role            TEXT    NOT NULL CHECK (role IN ('lead', 'review', 'build', 'brainstorm', 'qa', 'other')),
    department      TEXT,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (card_id, conversation_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_board_card_links_single_lead ON board_card_links (card_id) WHERE role = 'lead';
CREATE INDEX IF NOT EXISTS idx_board_card_links_conversation ON board_card_links (conversation_id);

-- Append-only history. `seq` is the replay order (never reused, never copied
-- by recover); `id` is the stable identity used for de-duplication.
CREATE TABLE IF NOT EXISTS board_events (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    id              TEXT    NOT NULL UNIQUE,
    card_id         TEXT,
    project_id      TEXT    NOT NULL,
    user_id         TEXT    NOT NULL,
    actor_kind      TEXT    NOT NULL CHECK (actor_kind IN ('user', 'agent', 'system')),
    actor_ref       TEXT    NOT NULL,
    conversation_id TEXT,
    kind            TEXT    NOT NULL,
    payload         TEXT    NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_board_events_project_seq ON board_events (project_id, seq);
CREATE INDEX IF NOT EXISTS idx_board_events_card_seq ON board_events (card_id, seq);
CREATE INDEX IF NOT EXISTS idx_board_events_conversation_seq ON board_events (conversation_id, seq);

-- Assignment find-or-create (S4): at most one conversation per
-- (user, board_assignment_id). `conversations.extra` is always valid JSON
-- (upstream's idx_conversations_cron_job_id already evaluates json_extract
-- on every row). Lookups must use the same expression to hit the index.
CREATE UNIQUE INDEX IF NOT EXISTS idx_conversations_board_assignment
    ON conversations (user_id, json_extract(extra, '$.board_assignment_id'))
    WHERE json_extract(extra, '$.board_assignment_id') IS NOT NULL;
