-- V032: relax V005 tier constraints for the level-0 Spectator tier.
--
-- ensure_spectator_tier() inserts (tier_level 0, price_cents 0), but V005
-- shipped CHECK (price_cents >= 99 AND <= 4999) and CHECK (tier_level >= 1
-- AND <= 5) — so the insert is rejected on any FRESH database (every new
-- self-host install; surfaced by permissions_test). Paid tiers keep the
-- original 99..4999 cent bounds; only the free spectator row is special.
ALTER TABLE mm_subscription_tiers
    DROP CONSTRAINT IF EXISTS mm_subscription_tiers_price_cents_check;
ALTER TABLE mm_subscription_tiers
    ADD CONSTRAINT mm_subscription_tiers_price_cents_check
    CHECK ((tier_level = 0 AND price_cents = 0)
           OR (price_cents >= 99 AND price_cents <= 4999));

ALTER TABLE mm_subscription_tiers
    DROP CONSTRAINT IF EXISTS mm_subscription_tiers_tier_level_check;
ALTER TABLE mm_subscription_tiers
    ADD CONSTRAINT mm_subscription_tiers_tier_level_check
    CHECK (tier_level >= 0 AND tier_level <= 5);
