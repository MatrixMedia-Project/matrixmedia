-- V012: Platform-default subscription tiers
--
-- A tier row with creator_user_id IS NULL is a platform-wide default, available
-- to every creator unless they override the same tier_level with their own row.
--
-- Free tier (tier 0) is intentionally NOT a row: "free" means "no subscription".

ALTER TABLE mm_subscription_tiers ALTER COLUMN creator_user_id DROP NOT NULL;

-- The original UNIQUE(creator_user_id, tier_level) treats multiple NULLs as
-- distinct, so it cannot enforce one-row-per-platform-tier. Add a partial index.
CREATE UNIQUE INDEX IF NOT EXISTS uq_subscription_tiers_platform_level
    ON mm_subscription_tiers(tier_level)
    WHERE creator_user_id IS NULL;

-- Seed the four platform defaults. ON CONFLICT DO NOTHING for idempotency
-- (re-running the migration with the partial index in place will no-op).
INSERT INTO mm_subscription_tiers
    (creator_user_id, name, description, price_cents, currency, tier_level, perks_json, is_active)
VALUES
    (NULL, 'Supporter',  'Show your support',                     100,  'usd', 1, '["sub-only chat"]', true),
    (NULL, 'Fan',        'Access subscriber-only streams',        500,  'usd', 2, '["sub-only chat","sub-only streams"]', true),
    (NULL, 'Superfan',   'Priority access and early recordings', 1000,  'usd', 3, '["sub-only chat","sub-only streams","early recordings"]', true),
    (NULL, 'Patron',     'Top-tier perks',                       2000,  'usd', 4, '["sub-only chat","sub-only streams","early recordings","direct DM"]', true)
ON CONFLICT DO NOTHING;
