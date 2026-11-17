-- Migration 910 (youssef range 900–999): server access pools (card C36, TASK-130).
-- Frozen once deployed: never edit or renumber; a change is a new 9NN file.
--
-- A server is an SSH target the owner registers once (host, user, stored key)
-- and assigns to conversations through the kit attach levels
-- (`kit_attachments.item_kind = 'server'`, `item_ref = servers.id`; global →
-- project → assistant → conversation, see `aionui_db::kit_resolve`). Agents
-- reach it only through the built-in `aionui-servers` MCP servers, which
-- re-read these tables on every call. Design:
-- clauterm-studio/design/servers/C36-SERVER-POOLS.md.
--
-- Enum semantics are validated in the service layer (no CHECK constraints,
-- like 904/907):
--   servers.environment     production | staging | sandbox | local
--   servers.tier            read | write   (guidance shown to the agent only;
--                           a shell cannot enforce it)
--   server_audit.channel    standard | crucial
--   server_audit.decision   ran | failed | needs_approval | refused
--
-- The private key lives in its own table so list queries never read
-- ciphertext; it is encrypted with the provider-key AES-256-GCM crypto (key
-- derived from the storage-encryption secret, 042), like `mcp_secrets`.
-- `server_audit` has no FK: the trail outlives a deleted server.
--
-- Rollback: additive only (three new tables). An older binary refuses a DB
-- holding 910, so a rollback restores the pre-deploy backup; lost = the
-- servers, their keys and the audit rows written since the deploy.

CREATE TABLE IF NOT EXISTS servers (
    id          TEXT    PRIMARY KEY,
    user_id     TEXT    NOT NULL,
    name        TEXT    NOT NULL,          -- [a-z0-9][a-z0-9-]*, what agents call it
    host        TEXT    NOT NULL,
    port        INTEGER NOT NULL DEFAULT 22,
    ssh_user    TEXT    NOT NULL,
    host_key    TEXT    NOT NULL DEFAULT '',   -- known_hosts key part(s); '' = trust on first use
    labels      TEXT    NOT NULL DEFAULT '[]', -- JSON array of strings
    environment TEXT    NOT NULL DEFAULT 'staging',
    tier        TEXT    NOT NULL DEFAULT 'read',
    notes       TEXT    NOT NULL DEFAULT '',   -- owner guidance shown to agents
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL,  -- epoch ms
    updated_at  INTEGER NOT NULL,  -- epoch ms
    deleted_at  INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_servers_user_name_live
    ON servers (user_id, name) WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS server_secrets (
    server_id       TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL,
    value_encrypted TEXT    NOT NULL,   -- base64(nonce + ciphertext + tag)
    updated_at      INTEGER NOT NULL    -- epoch ms
);

CREATE TABLE IF NOT EXISTS server_audit (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL,
    conversation_id TEXT    NOT NULL DEFAULT '',
    server_id       TEXT    NOT NULL DEFAULT '',
    server_name     TEXT    NOT NULL,
    channel         TEXT    NOT NULL,
    decision        TEXT    NOT NULL,
    reason          TEXT    NOT NULL DEFAULT '',
    command         TEXT    NOT NULL DEFAULT '',  -- redacted, truncated
    exit_code       INTEGER,
    duration_ms     INTEGER,
    created_at      INTEGER NOT NULL   -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_server_audit_user_created ON server_audit (user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_server_audit_server_created ON server_audit (server_id, created_at);
