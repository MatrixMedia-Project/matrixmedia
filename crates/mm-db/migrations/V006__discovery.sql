-- V006: Discovery & Recommendations (Phase 7c)
-- Applied to PostgreSQL only (not SQLite).

CREATE TABLE IF NOT EXISTS mm_user_interactions (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id             TEXT NOT NULL,
    stream_id           TEXT NOT NULL,
    action_type         TEXT NOT NULL CHECK (action_type IN ('view','like','share')),
    view_duration_secs  INT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_interactions_user
    ON mm_user_interactions(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_interactions_stream
    ON mm_user_interactions(stream_id, action_type);

CREATE TABLE IF NOT EXISTS mm_creator_follows (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id             TEXT NOT NULL,
    creator_user_id     TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(user_id, creator_user_id)
);

CREATE INDEX IF NOT EXISTS idx_follows_user
    ON mm_creator_follows(user_id);
CREATE INDEX IF NOT EXISTS idx_follows_creator
    ON mm_creator_follows(creator_user_id);

CREATE TABLE IF NOT EXISTS mm_trending_cache (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stream_id           TEXT NOT NULL,
    period              TEXT NOT NULL DEFAULT 'hourly',
    trending_score      DOUBLE PRECISION NOT NULL,
    calculated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_trending_score
    ON mm_trending_cache(period, trending_score DESC);

CREATE TABLE IF NOT EXISTS mm_content_categories (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                TEXT NOT NULL UNIQUE,
    description         TEXT,
    icon_url            TEXT,
    display_order       INT NOT NULL DEFAULT 0
);
