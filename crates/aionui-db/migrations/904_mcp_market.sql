-- Migration 904 (youssef range 900–999): MCP market instances + encrypted secrets (card C15).
--
-- A market instance is an ordinary `mcp_servers` row whose `market_status` is
-- NOT NULL. Its `transport_config` only ever holds the launcher placeholder:
-- the pinned command/args come from the template snapshot in
-- `market_template`, written server-side from the catalogue (never from a
-- client), and are read by `aioncore mcp exec`. Every market row is stored
-- `enabled = 0`. The column is `market_status`, not `status`, because the
-- legacy repair path renames a `status` column on this table.
--
-- Enum semantics are validated in the service layer (no CHECK constraints,
-- like 028):
--   market_status       active | dormant | revoked
--   market_environment  production | staging | sandbox | local
--   market_tier         read | write
--   market_enforcement  server | credential | tools-only
-- JSON columns: market_clis = ["claude","codex","gemini"] subset;
-- market_plain_env = {"KEY": "plain value"}; market_secret_refs =
-- {"KEY": "SECRET_NAME"} (names only, never values); market_tools_allow /
-- market_tools_block = ["tool", ...].
ALTER TABLE mcp_servers ADD COLUMN market_status TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_template_id TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_template TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_environment TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_tier TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_enforcement TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_clis TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_plain_env TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_secret_refs TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_tools_allow TEXT;
ALTER TABLE mcp_servers ADD COLUMN market_tools_block TEXT;
-- Owner exception record for an active production write instance: OA-<yyyymmdd>-<slug>.
ALTER TABLE mcp_servers ADD COLUMN market_approval_ref TEXT;

CREATE INDEX IF NOT EXISTS idx_mcp_servers_market
    ON mcp_servers (user_id, market_status) WHERE market_status IS NOT NULL;

-- Secret values for market instances, encrypted with the provider-key
-- AES-256-GCM crypto (key derived from the storage-encryption secret, 042).
-- A secret belongs to exactly one instance (`instance_id` = mcp_servers.id):
-- the launcher decrypts only the refs that instance's template declares.
CREATE TABLE IF NOT EXISTS mcp_secrets (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL,
    instance_id     TEXT    NOT NULL,
    name            TEXT    NOT NULL,   -- ref name, e.g. TB_PADDLE_PROD_READ__PADDLE_API_KEY
    value_encrypted TEXT    NOT NULL,   -- base64(nonce + ciphertext + tag)
    created_at      INTEGER NOT NULL,   -- epoch ms
    updated_at      INTEGER NOT NULL    -- epoch ms
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_mcp_secrets_user_name ON mcp_secrets (user_id, name);
CREATE INDEX IF NOT EXISTS idx_mcp_secrets_instance ON mcp_secrets (instance_id);
