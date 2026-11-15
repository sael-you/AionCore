-- Migration 905 (youssef range 900–999): MCP market assignments (card C15).
--
-- One normalized row per (instance, project, assistant, cli):
--   project_id   NULL      = global (only context7 / studio-memory, enforced by the service)
--                 not NULL = projects.project_id
--   assistant_id NULL      = the whole project, delivered through the native project files
--                 not NULL = one assistant, delivered through the AionUi assistant binding
--   cli          claude | codex | gemini
-- The service also keeps one delivery channel per (instance, project, cli):
-- a whole-project row and an assistant row never coexist for the same triple.
-- SQLite treats NULLs as distinct in UNIQUE indexes, hence one partial index
-- per shape.
CREATE TABLE IF NOT EXISTS mcp_assignments (
    id            TEXT    PRIMARY KEY,
    user_id       TEXT    NOT NULL,
    mcp_server_id TEXT    NOT NULL,
    project_id    TEXT,
    assistant_id  TEXT,
    cli           TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,  -- epoch ms
    updated_at    INTEGER NOT NULL   -- epoch ms
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_mcp_assignments_global
    ON mcp_assignments (mcp_server_id, cli) WHERE project_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_mcp_assignments_project
    ON mcp_assignments (mcp_server_id, project_id, cli) WHERE project_id IS NOT NULL AND assistant_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_mcp_assignments_assistant
    ON mcp_assignments (mcp_server_id, project_id, assistant_id, cli) WHERE assistant_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_mcp_assignments_user_project ON mcp_assignments (user_id, project_id);
