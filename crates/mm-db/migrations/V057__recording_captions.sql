-- V057: recording captions + summaries, and streamer caption consent.
--
-- One row per (recording, language, kind) in `mm_recording_captions`.
-- `kind` is 'source' | 'translated'; `status` is the caption-job row
-- protocol: 'pending' → 'ready' + `storage_key`, or 'failed' + `error`
-- (same conservatism as mp4_tracker's guarded writes: a row a real worker
-- later resolves is never clobbered — stub writes guard on
-- `status = 'pending'`).
--
-- Row presence with `status = 'ready'` IS track readiness: the API only
-- hands out a caption URL for a ready row with a non-null storage key,
-- mirroring the mp4_url gating (never hand out a URL that 404s).
-- `mm_recordings` itself is untouched (same conservatism as V030).
--
-- Summaries live in-row as `summary_text`, one per recording + language
-- (translated with the caption MT engine, same as tracks).
--
-- Streamer consent mirrors V040's transcode opt-in shape (creator default +
-- per-broadcast inherit|on|off override), but WITHOUT a `released` flag:
-- there is no rented GPU to release for captions, so no operator veto.
-- Captions are a consent feature, not a wallet feature. The caption enqueue
-- reads these columns: no opt-in, no caption job.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests). V057 creates the `mm_recording_captions_migrated`
-- marker table in its LAST statement, so a V057 that died partway —
-- marker table not yet created — is not adopted.
CREATE TABLE IF NOT EXISTS mm_recording_captions (
    recording_id TEXT NOT NULL REFERENCES mm_recordings(id) ON DELETE CASCADE,
    lang         TEXT NOT NULL DEFAULT 'en',
    kind         TEXT NOT NULL DEFAULT 'source'
        CONSTRAINT captions_kind_valid
        CHECK (kind IN ('source', 'translated')),
    status       TEXT NOT NULL DEFAULT 'pending'
        CONSTRAINT captions_status_valid
        CHECK (status IN ('pending', 'ready', 'failed')),
    storage_key  TEXT,
    summary_text TEXT,
    error        TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (recording_id, lang, kind)
);

-- Boot resume sweep + per-request lookups filter on pending rows only.
CREATE INDEX IF NOT EXISTS idx_captions_pending
    ON mm_recording_captions(status)
    WHERE status = 'pending';

-- Streamer consent: creator default next to the other per-creator defaults
-- (V013/V014/V040), per-broadcast override next to `transcode_opt_in`.
ALTER TABLE mm_creator_defaults
    ADD COLUMN IF NOT EXISTS captions_opt_in_default BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE mm_streams
    ADD COLUMN IF NOT EXISTS captions_opt_in TEXT NOT NULL DEFAULT 'inherit'
        CONSTRAINT streams_captions_opt_in_valid
        CHECK (captions_opt_in IN ('inherit', 'on', 'off'));

CREATE TABLE IF NOT EXISTS mm_recording_captions_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
