-- Broker schema (design doc §12).
-- Stores account ids, license and device pseudonyms and token lifecycle only.
-- No session content, no clipboard, no file names, no raw NodeId (§15).

CREATE TABLE IF NOT EXISTS accounts (
    id           TEXT PRIMARY KEY,
    created_at   INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS licenses (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    plan         INTEGER NOT NULL, -- 0 trial, 1 pro, 2 team
    expires_at   INTEGER NOT NULL,
    revoked_at   INTEGER,
    created_at   INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS devices (
    id                TEXT PRIMARY KEY, -- random device_id, never a hardware fingerprint (§7)
    license_id        TEXT NOT NULL REFERENCES licenses (id) ON DELETE CASCADE,
    last_heartbeat_at INTEGER,
    trial_seconds_used INTEGER NOT NULL DEFAULT 0, -- against TRIAL_SESSION_LIMIT_SECS (§12.3)
    created_at        INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS devices_license_idx ON devices (license_id);

-- Retrying /v1/license/issue with the same key returns the previous token
-- instead of creating another device row (§12.2, §18).
CREATE TABLE IF NOT EXISTS issued_tokens (
    idempotency_key TEXT PRIMARY KEY,
    license_id      TEXT NOT NULL REFERENCES licenses (id) ON DELETE CASCADE,
    device_id       TEXT NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    token           BLOB NOT NULL,
    issued_at       INTEGER NOT NULL
) STRICT;

-- Webhook replay protection: unique event_id inside the same transaction that
-- changes entitlement, plus a 5 minute timestamp window (§12.2).
CREATE TABLE IF NOT EXISTS webhook_events (
    event_id     TEXT PRIMARY KEY,
    provider     TEXT NOT NULL,
    received_at  INTEGER NOT NULL,
    event_ts     INTEGER NOT NULL
) STRICT;
