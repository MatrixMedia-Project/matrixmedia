-- V061: creator ingest (stream) keys.
--
-- One durable key per creator for external encoders (OBS/WHIP). Only the
-- SHA-256 hash of the key is stored; the plaintext is shown once, in the
-- create/rotate API response, and exists nowhere in this schema (no
-- plaintext column, by design). `key_prefix` holds the first 16 chars of
-- the key (`mm_live_` + 8 hex) purely so the UI can show a masked form.
-- `revoked_at` is set by revoke and cleared by rotate; a rotated key is a
-- new hash on the same row, so the old hash stops resolving at once.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests). V061 creates the `mm_ingest_keys_migrated`
-- marker table in its LAST statement, so a V061 that died partway —
-- marker table not yet created — is not adopted.
CREATE TABLE IF NOT EXISTS mm_ingest_keys (
    id              UUID PRIMARY KEY,
    creator_user_id TEXT NOT NULL UNIQUE,
    key_hash        TEXT NOT NULL UNIQUE,
    key_prefix      TEXT NOT NULL,
    default_room_id TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at    TIMESTAMPTZ,
    revoked_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_mm_ingest_keys_hash ON mm_ingest_keys (key_hash);

CREATE TABLE IF NOT EXISTS mm_ingest_keys_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
