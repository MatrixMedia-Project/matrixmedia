-- V057: recording captions + summaries, and streamer caption consent.
--
-- One row per language track in `mm_recording_captions` (source or
-- translated WebVTT sidecar served from the existing `/_mm/recordings/`
-- static route next to the WebM/MP4/JPG siblings). Row presence IS track
-- readiness: the API only hands out a caption URL for a listed language,
-- mirroring the mp4_url gating (never hand out a URL that 404s).
-- `mm_recordings` itself is untouched (same conservatism as V030).
--
-- Summaries live in `mm_recording_summaries`, one row per recording +
-- language (translated with the caption MT engine, same as tracks).
--
-- Streamer consent mirrors V040's transcode opt-in shape (creator default +
-- per-broadcast inherit|on|off override), but WITHOUT a `released` flag:
-- there is no rented GPU to release for captions, so no operator veto.
-- Captions are a consent feature, not a wallet feature.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests). V057 creates the `mm_captions_migrated`
-- marker table in its LAST statement, so a V057 that died partway —
-- marker table not yet created — is not adopted.
CREATE TABLE IF NOT EXISTS mm_recording_captions (
    recording_id TEXT NOT NULL REFERENCES mm_recordings(id) ON DELETE CASCADE,
    lang         TEXT NOT NULL,
    kind         TEXT NOT NULL DEFAULT 'source'
        CONSTRAINT captions_kind_valid
        CHECK (kind IN ('source', 'translated')),
    storage_key  TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (recording_id, lang)
);
CREATE INDEX IF NOT EXISTS idx_recording_captions_recording
    ON mm_recording_captions(recording_id);

CREATE TABLE IF NOT EXISTS mm_recording_summaries (
    recording_id TEXT NOT NULL REFERENCES mm_recordings(id) ON DELETE CASCADE,
    lang         TEXT NOT NULL,
    summary      TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (recording_id, lang)
);

-- Streamer consent: creator default next to the other per-creator defaults
-- (V013/V014/V040), per-broadcast override next to `transcode_opt_in`.
ALTER TABLE mm_creator_defaults
    ADD COLUMN IF NOT EXISTS captions_opt_in_default BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE mm_streams
    ADD COLUMN IF NOT EXISTS captions_opt_in TEXT NOT NULL DEFAULT 'inherit'
        CONSTRAINT streams_captions_opt_in_valid
        CHECK (captions_opt_in IN ('inherit', 'on', 'off'));

CREATE TABLE IF NOT EXISTS mm_captions_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
