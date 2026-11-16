-- Migration 907 (youssef range 900–999): Kits + four attach levels (card C27, D49/D50).
-- Frozen once deployed: never edit or renumber; a change is a new 9NN file.
--
-- A kit is a named bundle of library skills and MCP market instances. It is
-- attached (or a single skill / MCP instance is attached) with `add` or
-- `remove` at one of four levels; the effective set of a conversation is
-- resolved top-down global → project → assistant → conversation, a lower
-- level winning (see `aionui_db::kit_resolve`). Design:
-- clauterm-studio/design/skills-mcp/C27-KITS.md.
--
-- Enum semantics are validated in the service layer (no CHECK constraints,
-- like 904):
--   kit_items.kind              skill | mcp | template
--   kit_attachments.level       global | project | assistant | conversation
--   kit_attachments.item_kind   kit | skill | mcp
--   kit_attachments.op          add | remove
-- `kit_items.ref` = skill name | mcp_servers.id (market instance) | catalogue
-- template id (wanted, never delivered). `kit_attachments.target_id` = '' for
-- global, else projects.project_id | assistant id | conversations.id.
--
-- Rollback: additive only (three new tables). An older binary refuses a DB
-- holding 907, so a rollback restores the pre-deploy backup; lost = the kits
-- and attachments written since the deploy, nothing else.

CREATE TABLE IF NOT EXISTS kits (
    id          TEXT    PRIMARY KEY,
    user_id     TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    -- 'K1'..'K12' for the kits seeded from KITS.md; NULL for owner kits.
    builtin_key TEXT,
    created_at  INTEGER NOT NULL,  -- epoch ms
    updated_at  INTEGER NOT NULL,  -- epoch ms
    deleted_at  INTEGER            -- soft delete keeps builtin_key (no re-seed)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_kits_user_name_live
    ON kits (user_id, name) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_kits_user_builtin
    ON kits (user_id, builtin_key) WHERE builtin_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS kit_items (
    kit_id  TEXT NOT NULL,
    user_id TEXT NOT NULL,
    kind    TEXT NOT NULL,
    ref     TEXT NOT NULL,
    PRIMARY KEY (kit_id, kind, ref)
);

CREATE INDEX IF NOT EXISTS idx_kit_items_user_ref ON kit_items (user_id, kind, ref);

CREATE TABLE IF NOT EXISTS kit_attachments (
    id         TEXT    PRIMARY KEY,
    user_id    TEXT    NOT NULL,
    level      TEXT    NOT NULL,
    target_id  TEXT    NOT NULL DEFAULT '',
    item_kind  TEXT    NOT NULL,
    item_ref   TEXT    NOT NULL,
    op         TEXT    NOT NULL,
    created_at INTEGER NOT NULL,  -- epoch ms
    updated_at INTEGER NOT NULL   -- epoch ms
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_kit_attachments_item
    ON kit_attachments (user_id, level, target_id, item_kind, item_ref);
CREATE INDEX IF NOT EXISTS idx_kit_attachments_ref
    ON kit_attachments (user_id, item_kind, item_ref);
