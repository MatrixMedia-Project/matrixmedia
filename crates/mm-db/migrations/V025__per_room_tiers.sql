-- V025__per_room_tiers.sql
--
-- Scope subscription tiers and subscriptions to an optional room_id.
-- NULL = creator-wide default ladder (preserves existing data).
-- Non-NULL = room-specific list that fully overrides the default for
-- that room.

ALTER TABLE mm_subscription_tiers ADD COLUMN IF NOT EXISTS room_id TEXT NULL;
ALTER TABLE mm_subscriptions      ADD COLUMN IF NOT EXISTS room_id TEXT NULL;

-- Existing uniqueness was (creator_user_id, tier_level) [V005], plus a
-- partial unique index on (tier_level) WHERE creator_user_id IS NULL for
-- platform-default tiers [V012]. Replace both with room-aware variants so a
-- creator can keep a distinct level-1 (etc.) tier per room. COALESCE(room_id,
-- '') folds NULL (creator-default ladder) into a single bucket while keeping
-- one row per (creator, room, level).
ALTER TABLE mm_subscription_tiers
    DROP CONSTRAINT IF EXISTS mm_subscription_tiers_creator_user_id_tier_level_key;

DROP INDEX IF EXISTS uq_subscription_tiers_platform_level;

-- Creator-owned tiers: one row per (creator, room, level).
CREATE UNIQUE INDEX IF NOT EXISTS mm_subscription_tiers_creator_room_level_key
    ON mm_subscription_tiers (creator_user_id, COALESCE(room_id, ''), tier_level)
    WHERE creator_user_id IS NOT NULL;

-- Platform-default tiers (creator_user_id IS NULL): one row per (room, level).
CREATE UNIQUE INDEX IF NOT EXISTS uq_subscription_tiers_platform_room_level
    ON mm_subscription_tiers (COALESCE(room_id, ''), tier_level)
    WHERE creator_user_id IS NULL;

-- Subscriptions were unique on (subscriber, creator). With room scoping a
-- subscriber may hold one subscription per (creator, room). COALESCE folds the
-- creator-default subscription (room_id IS NULL) into a single bucket.
ALTER TABLE mm_subscriptions
    DROP CONSTRAINT IF EXISTS mm_subscriptions_subscriber_user_id_creator_user_id_key;

CREATE UNIQUE INDEX IF NOT EXISTS mm_subscriptions_subscriber_creator_room_key
    ON mm_subscriptions (subscriber_user_id, creator_user_id, COALESCE(room_id, ''));

-- Indexes for the new query shapes.
CREATE INDEX IF NOT EXISTS idx_mm_subscription_tiers_room
    ON mm_subscription_tiers (room_id) WHERE room_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_mm_subscriptions_creator_room
    ON mm_subscriptions (creator_user_id, room_id);
