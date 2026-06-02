//! Embedded SQL migrations for MatrixMedia.
//!
//! Migrations are applied in order on startup via `Database::migrate()`.
//! In production, the `mm-server migrate` subcommand can be used
//! to run migrations separately (e.g. in an init container).

/// Initial schema: rooms, streams, participants, server config, idempotency, media assets.
pub const V001_INITIAL: &str = r#"
CREATE TABLE IF NOT EXISTS mm_rooms (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    matrix_room_id  TEXT    NOT NULL UNIQUE,
    origin_server   TEXT,
    max_participants INTEGER NOT NULL DEFAULT 50,
    allowed_media_types TEXT,
    created_at      TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS mm_streams (
    id                TEXT    PRIMARY KEY,
    room_id           INTEGER NOT NULL REFERENCES mm_rooms(id),
    host_user_id      TEXT    NOT NULL,
    media_type        TEXT    NOT NULL DEFAULT 'audio',
    title             TEXT,
    status            TEXT    NOT NULL DEFAULT 'active',
    sfu_room_id       TEXT,
    participant_count INTEGER NOT NULL DEFAULT 0,
    started_at        TEXT    NOT NULL DEFAULT (datetime('now')),
    ended_at          TEXT,
    state_event_id    TEXT
);

CREATE TABLE IF NOT EXISTS mm_participants (
    id                  TEXT    PRIMARY KEY,
    stream_id           TEXT    NOT NULL REFERENCES mm_streams(id),
    user_id             TEXT    NOT NULL,
    role                TEXT    NOT NULL DEFAULT 'viewer',
    sfu_participant_id  TEXT,
    joined_at           TEXT    NOT NULL DEFAULT (datetime('now')),
    left_at             TEXT,
    UNIQUE(stream_id, user_id)
);

CREATE TABLE IF NOT EXISTS mm_server_config (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS mm_idempotency (
    key           TEXT PRIMARY KEY,
    response_json TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS mm_media_assets (
    id              TEXT PRIMARY KEY,
    stream_id       TEXT REFERENCES mm_streams(id),
    asset_type      TEXT NOT NULL,
    storage_key     TEXT NOT NULL,
    storage_backend TEXT NOT NULL DEFAULT 'local',
    mime_type       TEXT,
    size_bytes      INTEGER,
    sha256          TEXT,
    cdn_url         TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_streams_room_status ON mm_streams(room_id, status);
CREATE INDEX IF NOT EXISTS idx_participants_stream ON mm_participants(stream_id, left_at);
CREATE INDEX IF NOT EXISTS idx_idempotency_expires ON mm_idempotency(expires_at);
"#;

/// Recordings table for stream recording lifecycle.
pub const V002_RECORDINGS: &str = r#"
CREATE TABLE IF NOT EXISTS mm_recordings (
    id              TEXT PRIMARY KEY,
    stream_id       TEXT NOT NULL,
    room_id         INTEGER NOT NULL,
    host_user_id    TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'recording',
    media_type      TEXT NOT NULL,
    storage_key     TEXT NOT NULL,
    storage_backend TEXT NOT NULL DEFAULT 'local',
    mxc_url         TEXT,
    cdn_url         TEXT,
    duration_ms     INTEGER,
    size_bytes      INTEGER,
    mime_type       TEXT NOT NULL DEFAULT 'audio/ogg',
    sha256          TEXT,
    title           TEXT,
    egress_id       TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_recordings_stream ON mm_recordings(stream_id);
CREATE INDEX IF NOT EXISTS idx_recordings_room ON mm_recordings(room_id);
CREATE INDEX IF NOT EXISTS idx_recordings_status ON mm_recordings(status);
CREATE INDEX IF NOT EXISTS idx_recordings_egress ON mm_recordings(egress_id);
"#;

/// E2EE: add key columns to streams + key history table.
///
/// SQLite does not support `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`, so
/// the migration runner treats "duplicate column name" errors as a no-op,
/// which keeps migrations idempotent on repeat runs.
pub const V003_E2EE: &str = r#"
ALTER TABLE mm_streams ADD COLUMN e2ee_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE mm_streams ADD COLUMN e2ee_algorithm TEXT;
ALTER TABLE mm_streams ADD COLUMN e2ee_key_id TEXT;
ALTER TABLE mm_streams ADD COLUMN e2ee_key_generation INTEGER;
ALTER TABLE mm_streams ADD COLUMN e2ee_key_b64 TEXT;

CREATE TABLE IF NOT EXISTS mm_e2ee_key_history (
    stream_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    key_id TEXT NOT NULL,
    key_b64 TEXT NOT NULL,
    algorithm TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (stream_id, generation)
);
"#;

/// Per-content tier gate (V026 parity for the SQLite dev/test backend).
///
/// `min_tier_level` is NULL by default = free, anyone can watch. The
/// Postgres equivalent lives in `migrations/V026__content_tier_gates.sql`.
/// SQLite lacks `ADD COLUMN IF NOT EXISTS`, so the runner treats the
/// "duplicate column name" error as a no-op for idempotency.
pub const V026_CONTENT_TIER_GATES: &str = r#"
ALTER TABLE mm_streams ADD COLUMN min_tier_level INTEGER;
ALTER TABLE mm_recordings ADD COLUMN min_tier_level INTEGER;
"#;

/// Return all migrations in order.
pub fn all_migrations() -> Vec<(&'static str, &'static str)> {
    vec![
        ("V001_initial", V001_INITIAL),
        ("V002_recordings", V002_RECORDINGS),
        ("V003_e2ee", V003_E2EE),
        ("V026_content_tier_gates", V026_CONTENT_TIER_GATES),
    ]
}
