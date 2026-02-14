-- Strata control plane schema
-- Runs on first startup via sqlx::migrate!()

CREATE TABLE IF NOT EXISTS users (
    id          TEXT PRIMARY KEY,          -- usr_<uuid7>
    email       TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role        TEXT NOT NULL DEFAULT 'operator',  -- admin | operator | viewer
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS senders (
    id                TEXT PRIMARY KEY,    -- snd_<uuid7>
    owner_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name              TEXT,
    hostname          TEXT,
    device_public_key TEXT,
    enrollment_token  TEXT,               -- hashed, single-use
    enrolled          BOOLEAN NOT NULL DEFAULT FALSE,
    last_seen_at      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_senders_owner ON senders(owner_id);

CREATE TABLE IF NOT EXISTS destinations (
    id          TEXT PRIMARY KEY,          -- dst_<uuid7>
    owner_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    platform    TEXT NOT NULL,             -- youtube | twitch | custom_rtmp | srt
    name        TEXT NOT NULL,
    url         TEXT NOT NULL,
    stream_key  TEXT,                      -- encrypted at rest (app-level)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_destinations_owner ON destinations(owner_id);

CREATE TABLE IF NOT EXISTS streams (
    id              TEXT PRIMARY KEY,      -- str_<uuid7>
    sender_id       TEXT NOT NULL REFERENCES senders(id) ON DELETE CASCADE,
    destination_id  TEXT REFERENCES destinations(id) ON DELETE SET NULL,
    state           TEXT NOT NULL DEFAULT 'idle',  -- idle|starting|live|stopping|ended|failed
    started_at      TIMESTAMPTZ,
    ended_at        TIMESTAMPTZ,
    config_json     TEXT,                 -- full pipeline config snapshot
    total_bytes     BIGINT NOT NULL DEFAULT 0,
    error_message   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_streams_sender ON streams(sender_id);
CREATE INDEX idx_streams_state  ON streams(state);
