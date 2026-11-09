-- V057: caption tracks for recordings (video-to-text serving + caption jobs).
-- One row per (recording, language, kind). The .vtt sibling lives next to the
-- recording file (e.g. /data/recordings/{id}.en.vtt) and is served by the
-- existing nginx /_mm/recordings/ route as text/vtt.
--   kind:   'source' | 'translated'
--   status: 'pending' | 'ready' | 'failed'
-- The caption worker itself is not built yet: until it lands, the mm-core
-- stub in caption_tracker records its outcome here, so the enqueue path,
-- the boot resume sweep, and the response gating are exercised end to end.
-- summary_text holds the per-language recording summary once the
-- summarisation step lands (the feed announcement reads it from here).
CREATE TABLE IF NOT EXISTS mm_recording_captions (
    recording_id TEXT NOT NULL REFERENCES mm_recordings(id) ON DELETE CASCADE,
    lang TEXT NOT NULL DEFAULT 'en',
    kind TEXT NOT NULL DEFAULT 'source',
    status TEXT NOT NULL DEFAULT 'pending',
    storage_key TEXT,
    summary_text TEXT,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (recording_id, lang, kind)
);

-- Boot resume sweep + per-request lookups filter on pending rows only.
CREATE INDEX IF NOT EXISTS idx_captions_pending
    ON mm_recording_captions(status)
    WHERE status = 'pending';

-- LAST on purpose: mm-db's adoption probe keys on the newest migration's LAST
-- statement being a `CREATE TABLE IF NOT EXISTS` (see migration_registry_tests).
CREATE TABLE IF NOT EXISTS mm_recording_captions_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
