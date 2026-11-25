-- V064: ADR-0011 section 3, the MM-room marker (`dev.matrixmedia.room_config`).
--
-- What mm-core last wrote as a room's marker. A write is skipped when the
-- content hash is unchanged, so the write points (bot join, enable-mm, the
-- owner's switch, stream start) and the backfill are idempotent. A separate
-- table, not columns on mm_room_config: that table's meaning is "a row exists
-- only where an owner made a choice", and two readers depend on the no-row
-- default.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests).
CREATE TABLE IF NOT EXISTS mm_room_marker (
    matrix_room_id TEXT        PRIMARY KEY,
    event_id       TEXT        NOT NULL,
    content_hash   TEXT        NOT NULL,
    written_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
