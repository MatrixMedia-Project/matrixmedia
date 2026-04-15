-- ============================================================================
-- V010: Advertising tables (Phase 9)
-- ============================================================================

-- 1. Ad creatives (both streamer and platform)
CREATE TABLE IF NOT EXISTS mm_ad_creatives (
    id                  TEXT PRIMARY KEY,
    owner_type          TEXT NOT NULL CHECK (owner_type IN ('creator', 'platform')),
    owner_id            TEXT NOT NULL,
    title               TEXT NOT NULL,
    placement           TEXT NOT NULL CHECK (placement IN ('pre_roll', 'mid_roll', 'post_roll', 'any')),
    duration_secs       INT NOT NULL CHECK (duration_secs > 0 AND duration_secs <= 120),
    storage_key         TEXT NOT NULL,
    storage_backend     TEXT NOT NULL DEFAULT 'local',
    cdn_url             TEXT,
    mime_type           TEXT NOT NULL DEFAULT 'video/mp4',
    file_size_bytes     BIGINT NOT NULL,
    click_through_url   TEXT,
    categories          JSONB NOT NULL DEFAULT '[]',
    status              TEXT NOT NULL DEFAULT 'processing'
                        CHECK (status IN ('processing', 'ready', 'paused', 'deleted')),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_ad_creatives_owner ON mm_ad_creatives(owner_type, owner_id, status);
CREATE INDEX IF NOT EXISTS idx_ad_creatives_placement ON mm_ad_creatives(placement, status);

-- 2. Ad insertion rules
CREATE TABLE IF NOT EXISTS mm_ad_rules (
    id                  TEXT PRIMARY KEY,
    ad_id               TEXT NOT NULL REFERENCES mm_ad_creatives(id) ON DELETE CASCADE,
    rule_type           TEXT NOT NULL CHECK (rule_type IN (
                            'always', 'time_interval', 'viewer_count_min',
                            'viewer_count_max', 'category_match',
                            'time_of_day', 'probability'
                        )),
    rule_config         JSONB NOT NULL,
    priority            INT NOT NULL DEFAULT 0,
    active              BOOLEAN NOT NULL DEFAULT true,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_ad_rules_ad ON mm_ad_rules(ad_id, active);

-- 3. Ad impressions (individual ad views with server-side proof)
CREATE TABLE IF NOT EXISTS mm_ad_impressions (
    id                  TEXT PRIMARY KEY,
    impression_token    TEXT NOT NULL UNIQUE,
    ad_id               TEXT NOT NULL REFERENCES mm_ad_creatives(id),
    stream_id           TEXT NOT NULL,
    viewer_user_id      TEXT NOT NULL,
    slot                TEXT NOT NULL,
    owner_type          TEXT NOT NULL,
    decided_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at          TIMESTAMPTZ,
    completed_at        TIMESTAMPTZ,
    skipped_at          TIMESTAMPTZ,
    clicked_at          TIMESTAMPTZ,
    error_at            TIMESTAMPTZ,
    watch_duration_secs INT,
    quartile_reached    INT DEFAULT 0 CHECK (quartile_reached >= 0 AND quartile_reached <= 4),
    viewport_visible    BOOLEAN,
    audio_audible       BOOLEAN,
    -- Server-side SFU enforcement timestamps (live proof for advertisers)
    sfu_revoked_at      TIMESTAMPTZ,
    sfu_restored_at     TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_impressions_ad ON mm_ad_impressions(ad_id, decided_at DESC);
CREATE INDEX IF NOT EXISTS idx_impressions_stream ON mm_ad_impressions(stream_id, decided_at DESC);
CREATE INDEX IF NOT EXISTS idx_impressions_viewer ON mm_ad_impressions(viewer_user_id, decided_at DESC);
CREATE INDEX IF NOT EXISTS idx_impressions_token ON mm_ad_impressions(impression_token);

-- 4. Platform ad insertion policies
CREATE TABLE IF NOT EXISTS mm_ad_platform_policy (
    id                  TEXT PRIMARY KEY,
    slot                TEXT NOT NULL,
    rule_type           TEXT NOT NULL,
    rule_config         JSONB NOT NULL,
    priority            INT NOT NULL DEFAULT 0,
    active              BOOLEAN NOT NULL DEFAULT true,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 5. Hourly aggregated stats (materialized by background job)
CREATE TABLE IF NOT EXISTS mm_ad_stats_hourly (
    id                  TEXT PRIMARY KEY,
    ad_id               TEXT NOT NULL REFERENCES mm_ad_creatives(id),
    hour                TIMESTAMPTZ NOT NULL,
    impressions         INT NOT NULL DEFAULT 0,
    completions         INT NOT NULL DEFAULT 0,
    skips               INT NOT NULL DEFAULT 0,
    clicks              INT NOT NULL DEFAULT 0,
    errors              INT NOT NULL DEFAULT 0,
    avg_watch_pct       DOUBLE PRECISION DEFAULT 0,
    UNIQUE(ad_id, hour)
);

CREATE INDEX IF NOT EXISTS idx_ad_stats_ad_hour ON mm_ad_stats_hourly(ad_id, hour DESC);
