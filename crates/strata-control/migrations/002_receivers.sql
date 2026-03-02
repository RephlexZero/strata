-- Receiver fleet management table.
-- Each running strata-receiver daemon registers here on connect.

CREATE TABLE IF NOT EXISTS receivers (
    id              TEXT PRIMARY KEY,          -- rcv_<uuid7>
    owner_id        TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name            TEXT,
    hostname        TEXT,
    region          TEXT,                      -- "eu-central", "us-east", etc.
    bind_host       TEXT NOT NULL,             -- public IP/hostname senders connect to
    link_ports      INTEGER[] NOT NULL,        -- UDP ports available for bonded links
    max_streams     INTEGER NOT NULL DEFAULT 6,
    active_streams  INTEGER NOT NULL DEFAULT 0,
    enrollment_token TEXT,                     -- hashed, single-use
    enrolled        BOOLEAN NOT NULL DEFAULT FALSE,
    online          BOOLEAN NOT NULL DEFAULT FALSE,
    last_seen_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_receivers_owner ON receivers(owner_id);
CREATE INDEX IF NOT EXISTS idx_receivers_online ON receivers(online);

-- Add receiver_id to streams so we know which receiver is handling each stream.
ALTER TABLE streams ADD COLUMN IF NOT EXISTS receiver_id TEXT REFERENCES receivers(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_streams_receiver ON streams(receiver_id);
