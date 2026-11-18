-- Migration 912 (youssef range 900–999): rules system (card C37, TASK-131).
-- Frozen once deployed: never edit or renumber; a change is a new 9NN file.
--
-- A rule is an owner-written markdown document that is ALWAYS applied to the
-- conversations it is attached to (unlike a skill, which the model loads on
-- demand). Attachment reuses the kit levels: `kit_attachments.item_kind =
-- 'rule'`, `item_ref = rules.id`, global → project → assistant →
-- conversation (see `aionui_db::kit_resolve`); kits may hold rules
-- (`kit_items.kind = 'rule'`). The effective rules are composed into the
-- session's preset context. Design: clauterm-studio/design/rules/C37-RULES.md.
--
-- `rule_sources` records the files rules were imported from (the global
-- AGENTS.md, a project's AGENTS.md / CLAUDE.md / GEMINI.md): the original
-- content is kept so nothing is lost when AionUi later generates the global
-- file or replaces a repo file by a pointer (both only after the owner
-- confirms). Enum semantics are validated in the service layer:
--   rules.origin          owner | import
--   rule_sources.scope    global | project
--   rule_sources.status   imported | managed   (managed = AionUi writes the file)
--
-- Rollback: additive only (two new tables). An older binary refuses a DB
-- holding 912, so a rollback restores the pre-deploy backup; lost = rules and
-- import records written since the deploy. Files AionUi rewrote keep their
-- generated/pointer content; their originals are in `rule_sources.original`
-- of the newer DB (export them before restoring).

CREATE TABLE IF NOT EXISTS rules (
    id          TEXT    PRIMARY KEY,
    user_id     TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    body        TEXT    NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    origin      TEXT    NOT NULL DEFAULT 'owner',
    origin_ref  TEXT    NOT NULL DEFAULT '',   -- rule_sources.id for imported rules
    created_at  INTEGER NOT NULL,  -- epoch ms
    updated_at  INTEGER NOT NULL,  -- epoch ms
    deleted_at  INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_rules_user_name_live
    ON rules (user_id, name) WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS rule_sources (
    id          TEXT    PRIMARY KEY,
    user_id     TEXT    NOT NULL,
    scope       TEXT    NOT NULL,
    project_id  TEXT    NOT NULL DEFAULT '',   -- '' for global
    path        TEXT    NOT NULL,              -- absolute file path
    original    TEXT    NOT NULL,              -- file content at import time
    status      TEXT    NOT NULL DEFAULT 'imported',
    imported_at INTEGER NOT NULL,  -- epoch ms
    managed_at  INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_rule_sources_path ON rule_sources (user_id, path);
