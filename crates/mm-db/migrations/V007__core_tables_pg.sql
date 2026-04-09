-- V007: Migrate core tables from SQLite to PostgreSQL.
-- Creates the 8 original tables (rooms, streams, participants, server config,
-- idempotency, media assets, recordings, e2ee key history) in PostgreSQL syntax.
-- This migration is idempotent (uses IF NOT EXISTS / DO blocks).

-- ============================================================================
-- V001 equivalent: rooms, streams, participants, server_config, idempotency,
-- media_assets
-- ============================================================================

CREATE TABLE IF NOT EXISTS mm_rooms (
    id              BIGSERIAL PRIMARY KEY,
    matrix_room_id  TEXT      NOT NULL UNIQUE,
    origin_server   TEXT,
    max_participants INTEGER   NOT NULL DEFAULT 50,
    allowed_media_types TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS mm_streams (
    id                TEXT    PRIMARY KEY,
    room_id           BIGINT  NOT NULL REFERENCES mm_rooms(id),
    host_user_id      TEXT    NOT NULL,
    media_type        TEXT    NOT NULL DEFAULT 'audio',
    title             TEXT,
    status            TEXT    NOT NULL DEFAULT 'active',
    sfu_room_id       TEXT,
    participant_count INTEGER NOT NULL DEFAULT 0,
    started_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at          TIMESTAMPTZ,
    -- E2EE columns (V003 equivalent)
    e2ee_enabled      BOOLEAN NOT NULL DEFAULT false,
    e2ee_algorithm    TEXT,
    e2ee_key_id       TEXT,
    e2ee_key_generation INTEGER,
    e2ee_key_b64      TEXT
);

CREATE TABLE IF NOT EXISTS mm_participants (
    id                  TEXT PRIMARY KEY,
    stream_id           TEXT NOT NULL REFERENCES mm_streams(id),
    user_id             TEXT NOT NULL,
    role                TEXT NOT NULL DEFAULT 'viewer',
    sfu_participant_id  TEXT,
    joined_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    left_at             TIMESTAMPTZ,
    UNIQUE(stream_id, user_id)
);

CREATE TABLE IF NOT EXISTS mm_server_config (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS mm_idempotency (
    key           TEXT PRIMARY KEY,
    response_json TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS mm_media_assets (
    id              TEXT PRIMARY KEY,
    stream_id       TEXT REFERENCES mm_streams(id),
    asset_type      TEXT NOT NULL,
    storage_key     TEXT NOT NULL,
    storage_backend TEXT NOT NULL DEFAULT 'local',
    mime_type       TEXT,
    size_bytes      BIGINT,
    sha256          TEXT,
    cdn_url         TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ============================================================================
-- V002 equivalent: recordings
-- ============================================================================

CREATE TABLE IF NOT EXISTS mm_recordings (
    id              TEXT PRIMARY KEY,
    stream_id       TEXT NOT NULL,
    room_id         BIGINT NOT NULL,
    host_user_id    TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'recording',
    media_type      TEXT NOT NULL,
    storage_key     TEXT NOT NULL,
    storage_backend TEXT NOT NULL DEFAULT 'local',
    mxc_url         TEXT,
    cdn_url         TEXT,
    duration_ms     BIGINT,
    size_bytes      BIGINT,
    mime_type       TEXT NOT NULL DEFAULT 'audio/ogg',
    sha256          TEXT,
    title           TEXT,
    egress_id       TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at    TIMESTAMPTZ
);

-- ============================================================================
-- V003 equivalent: e2ee key history
-- ============================================================================

CREATE TABLE IF NOT EXISTS mm_e2ee_key_history (
    stream_id  TEXT    NOT NULL,
    generation INTEGER NOT NULL,
    key_id     TEXT    NOT NULL,
    key_b64    TEXT    NOT NULL,
    algorithm  TEXT    NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (stream_id, generation)
);

-- ============================================================================
-- Indexes
-- ============================================================================

CREATE INDEX IF NOT EXISTS idx_streams_room_status    ON mm_streams(room_id, status);
CREATE INDEX IF NOT EXISTS idx_participants_stream    ON mm_participants(stream_id, left_at);
CREATE INDEX IF NOT EXISTS idx_idempotency_expires    ON mm_idempotency(expires_at);
CREATE INDEX IF NOT EXISTS idx_recordings_stream      ON mm_recordings(stream_id);
CREATE INDEX IF NOT EXISTS idx_recordings_room        ON mm_recordings(room_id);
CREATE INDEX IF NOT EXISTS idx_recordings_status      ON mm_recordings(status);
CREATE INDEX IF NOT EXISTS idx_recordings_egress      ON mm_recordings(egress_id);
CREATE INDEX IF NOT EXISTS idx_recordings_created     ON mm_recordings(created_at DESC);
