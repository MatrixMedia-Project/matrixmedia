-- Ad-creative conversion on upload (internal spec 2026-10-11 §5.3). Idempotent on purpose:
-- the runner re-runs migrations to heal a partially migrated database, so every statement is
-- IF NOT EXISTS / DROP-then-ADD, and enums are TEXT + CHECK. Values MUST match
-- mm_ads::conversion (ErrorCode, RenditionKind, SourceKind) and mm_ads::lifecycle.
-- The adoption probe in run_pg_migrations keys on mm_ad_creatives.media_purge_after, which is
-- created by the LAST statement here.

-- 1. Status gains draft (created, no media yet) and failed (newest job failed, nothing live).
ALTER TABLE mm_ad_creatives DROP CONSTRAINT IF EXISTS mm_ad_creatives_status_check;
ALTER TABLE mm_ad_creatives ADD CONSTRAINT mm_ad_creatives_status_check
    CHECK (status IN ('draft','processing','ready','failed','paused','deleted'));
ALTER TABLE mm_ad_creatives ALTER COLUMN status SET DEFAULT 'draft';

-- The job whose renditions are live. NULL = nothing converted is live.
ALTER TABLE mm_ad_creatives ADD COLUMN IF NOT EXISTS active_job_id UUID;
ALTER TABLE mm_ad_creatives ADD COLUMN IF NOT EXISTS media_warnings JSONB NOT NULL DEFAULT '[]'::jsonb;

-- 2. URL-only creatives never went through conversion (D8): they stop serving until a file
--    is uploaded. Converted ones (cdn_url under /_mm/ads/r/) and legacy local uploads
--    (/_mm/ads/{id}.webm, re-encoded by the worker's legacy import) are untouched, so a
--    re-run is a no-op.
UPDATE mm_ad_creatives SET status = 'draft', updated_at = now()
 WHERE status IN ('ready','processing')
   AND active_job_id IS NULL
   AND (cdn_url IS NULL OR cdn_url NOT LIKE 'http://mm-web/_mm/ads/%');

CREATE TABLE IF NOT EXISTS mm_ad_conversion_jobs (
    id               UUID PRIMARY KEY,
    creative_id      TEXT NOT NULL REFERENCES mm_ad_creatives(id) ON DELETE CASCADE,
    -- incoming/{upload_id}.src until the worker moves it to sources/; NULL for legacy imports
    -- and re-renders, whose original already sits in sources/.
    upload_id        UUID,
    source_sha256    TEXT NOT NULL CHECK (source_sha256 ~ '^[0-9a-f]{64}$'),
    source_bytes     BIGINT NOT NULL CHECK (source_bytes > 0),
    source_kind      TEXT NOT NULL CHECK (source_kind IN ('mov','matroska')),
    profile_version  INTEGER NOT NULL,
    -- Settings snapshot at enqueue (mm_ads::conversion::Limits): the worker reads no settings.
    limits           JSONB NOT NULL,
    state            TEXT NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued','running','succeeded','failed','cancelled','superseded')),
    stage            TEXT
        CHECK (stage IS NULL OR stage IN ('probe','loudness','encode_vp8','encode_h264','verify','publish')),
    progress_pct     SMALLINT NOT NULL DEFAULT 0 CHECK (progress_pct BETWEEN 0 AND 100),
    attempt          SMALLINT NOT NULL DEFAULT 0,
    max_attempts     SMALLINT NOT NULL DEFAULT 3,
    not_before       TIMESTAMPTZ NOT NULL DEFAULT now(),
    lease_owner      TEXT,
    lease_expires_at TIMESTAMPTZ,
    error_code       TEXT,
    error_message    TEXT,
    error_detail     TEXT,
    retryable        BOOLEAN,
    warnings         JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_by       TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at       TIMESTAMPTZ,
    finished_at      TIMESTAMPTZ,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The same bytes are never encoded twice under one profile (R9).
    UNIQUE (creative_id, source_sha256, profile_version)
);
CREATE INDEX IF NOT EXISTS mm_ad_conversion_jobs_open
    ON mm_ad_conversion_jobs (created_at) WHERE state IN ('queued','running');
CREATE INDEX IF NOT EXISTS mm_ad_conversion_jobs_creative
    ON mm_ad_conversion_jobs (creative_id, created_at DESC);

CREATE TABLE IF NOT EXISTS mm_ad_renditions (
    id               UUID PRIMARY KEY,
    job_id           UUID NOT NULL REFERENCES mm_ad_conversion_jobs(id) ON DELETE CASCADE,
    creative_id      TEXT NOT NULL REFERENCES mm_ad_creatives(id) ON DELETE CASCADE,
    kind             TEXT NOT NULL CHECK (kind IN ('webrtc_vp8','hls_h264_720')),
    -- Relative to the public ads dir: r/{creative_id}/{job8}-{kind}-{sha16}.{webm|mp4}. Immutable.
    rel_path         TEXT NOT NULL UNIQUE,
    sha256           TEXT NOT NULL,
    bytes            BIGINT NOT NULL,
    duration_ms      BIGINT NOT NULL,
    width            INTEGER NOT NULL,
    height           INTEGER NOT NULL,
    fps_num          INTEGER NOT NULL,
    fps_den          INTEGER NOT NULL,
    max_kf_gap_ms    INTEGER NOT NULL,
    first_kf_bytes   INTEGER NOT NULL,
    integrated_lufs  DOUBLE PRECISION,
    has_source_audio BOOLEAN NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Set when a newer job goes live or the creative is deleted; the worker's sweep removes
    -- the file and the row 24 h later.
    superseded_at    TIMESTAMPTZ,
    UNIQUE (job_id, kind)
);
CREATE INDEX IF NOT EXISTS mm_ad_renditions_superseded
    ON mm_ad_renditions (superseded_at) WHERE superseded_at IS NOT NULL;

-- The worker's liveness: it has no port, so mm-core reads this for the MMAdWorkerDown alert.
CREATE TABLE IF NOT EXISTS mm_media_worker_status (
    worker_id    TEXT PRIMARY KEY,
    version      TEXT NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL
);

-- LAST on purpose: the adoption probe keys on this column. Stamped at delete time from
-- advertising.source_retention_days; the worker removes the retained original after it.
ALTER TABLE mm_ad_creatives ADD COLUMN IF NOT EXISTS media_purge_after TIMESTAMPTZ;
