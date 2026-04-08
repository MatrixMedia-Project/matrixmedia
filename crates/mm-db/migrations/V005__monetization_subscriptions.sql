-- V005: Monetization - Subscriptions & Content Gating (Phase 7b)
-- Applied to PostgreSQL only (not SQLite).

CREATE TABLE IF NOT EXISTS mm_subscription_tiers (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    creator_user_id     TEXT NOT NULL,
    name                TEXT NOT NULL,
    description         TEXT,
    price_cents         BIGINT NOT NULL CHECK (price_cents >= 99 AND price_cents <= 4999),
    currency            TEXT NOT NULL DEFAULT 'usd',
    tier_level          INT NOT NULL CHECK (tier_level >= 1 AND tier_level <= 5),
    perks_json          JSONB NOT NULL DEFAULT '[]',
    badge_url           TEXT,
    is_active           BOOLEAN NOT NULL DEFAULT true,
    stripe_price_id     TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(creator_user_id, tier_level)
);

CREATE INDEX IF NOT EXISTS idx_subscription_tiers_creator
    ON mm_subscription_tiers(creator_user_id);

CREATE TABLE IF NOT EXISTS mm_subscriptions (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subscriber_user_id      TEXT NOT NULL,
    creator_user_id         TEXT NOT NULL,
    tier_id                 UUID NOT NULL REFERENCES mm_subscription_tiers(id),
    status                  TEXT NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active','past_due','cancelled','expired')),
    stripe_subscription_id  TEXT,
    current_period_end      TIMESTAMPTZ NOT NULL,
    cancelled_at            TIMESTAMPTZ,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(subscriber_user_id, creator_user_id)
);

CREATE INDEX IF NOT EXISTS idx_subscriptions_subscriber
    ON mm_subscriptions(subscriber_user_id);
CREATE INDEX IF NOT EXISTS idx_subscriptions_creator
    ON mm_subscriptions(creator_user_id);
CREATE INDEX IF NOT EXISTS idx_subscriptions_status
    ON mm_subscriptions(status);
CREATE INDEX IF NOT EXISTS idx_subscriptions_tier
    ON mm_subscriptions(tier_id);
CREATE INDEX IF NOT EXISTS idx_subscriptions_stripe
    ON mm_subscriptions(stripe_subscription_id);

CREATE TABLE IF NOT EXISTS mm_content_gates (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    content_type        TEXT NOT NULL CHECK (content_type IN ('stream','recording')),
    content_id          TEXT NOT NULL,
    creator_user_id     TEXT NOT NULL,
    min_tier_level      INT NOT NULL CHECK (min_tier_level >= 1 AND min_tier_level <= 5),
    preview_seconds     INT NOT NULL DEFAULT 120,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(content_type, content_id)
);

CREATE INDEX IF NOT EXISTS idx_content_gates_type_id
    ON mm_content_gates(content_type, content_id);
CREATE INDEX IF NOT EXISTS idx_content_gates_creator
    ON mm_content_gates(creator_user_id);
