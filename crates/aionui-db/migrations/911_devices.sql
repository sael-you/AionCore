-- 911 · Devices registry (C38, TASK-136, D42 registry).
--
-- What: three tables behind Settings → Devices.
--   * `devices`: phones, the Mac and add-ons reached through a relay client,
--     plus the agent switch (and its timeout) of every device kind. Server
--     browsers and the owner's own browsers (C30/C34) keep their data where it
--     is; they only get a row here for state this page owns (none in v1).
--   * `device_pairings`: relay clients (Mac helper, Android companion app,
--     Wi-Fi direct) with their HASHED token (SHA-256 hex), never the token.
--   * `device_audit`: every agent action on a device: verb, app package,
--     origin, outcome. Never screenshots, typed text or command output.
-- Why: agents may use the owner's phone / Mac only while he enables it, every
-- action is audited, and pairings are revocable (C38 §3).
--
-- Additive only: new tables and indexes, nothing existing is touched.
-- Rollback: there are no down migrations; an older binary refuses a DB that
-- holds 911, so a rollback restores the pre-deploy DB backup. Lost: devices
-- paired, switches set and audit rows written after the deploy (the relay
-- clients must pair again).
--
-- Frozen once deployed: never edit or renumber this file.

CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    -- phone | mac | addon (server_browser / my_browser rows are aggregated)
    kind TEXT NOT NULL,
    -- The relay client that reaches it (device_pairings.id); NULL for
    -- server-hosted add-ons.
    pairing_id TEXT,
    -- Phone serial from the client's inventory, `mac`, or the add-on name.
    source_id TEXT NOT NULL,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    -- Epoch ms; the switch turns off at this time (reconciled at startup).
    enabled_until INTEGER,
    -- Last facts from the client (model, transport, chrome, version, disk).
    meta TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_seen_at INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_source
    ON devices(user_id, kind, COALESCE(pairing_id, ''), source_id);

CREATE TABLE IF NOT EXISTS device_pairings (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    -- mac_helper | companion | wifi_direct
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    token_sha256 TEXT NOT NULL UNIQUE,
    version TEXT,
    -- JSON array of the verbs the client said it supports.
    capabilities TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER,
    revoked_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_device_pairings_user ON device_pairings(user_id);

CREATE TABLE IF NOT EXISTS device_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL,
    device_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    verb TEXT NOT NULL,
    package TEXT,
    origin TEXT,
    -- ok | refused | approved | denied | timeout | error
    outcome TEXT NOT NULL,
    conversation_id TEXT,
    detail TEXT
);

CREATE INDEX IF NOT EXISTS idx_device_audit_device ON device_audit(user_id, device_id, id);
