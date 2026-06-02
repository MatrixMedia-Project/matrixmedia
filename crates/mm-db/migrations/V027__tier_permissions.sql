-- V027__tier_permissions.sql
--
-- Add a JSONB permissions blob to each tier describing what its
-- subscribers can do inside the (creator, room) pair. The canonical
-- shape (8 booleans, snake_case on the wire) is documented in
-- crates/mm-core/src/permissions.rs (struct TierPermissions).
--
-- Backwards-compat: existing tiers are backfilled with permissive
-- defaults so behavior pre-V027 is unchanged. A brand-new tier inserted
-- without an explicit blob gets '{}' which deserializes (via #[serde(default)]
-- on TierPermissions) to all-false; callers that want a meaningful tier
-- always pass an explicit blob.

ALTER TABLE mm_subscription_tiers
    ADD COLUMN IF NOT EXISTS permissions JSONB NOT NULL DEFAULT '{}'::jsonb;

CREATE INDEX IF NOT EXISTS idx_mm_subscription_tiers_permissions_gin
    ON mm_subscription_tiers USING GIN (permissions);

-- Backfill: existing tiers (the creator-default ladder + any room tiers
-- created by Stage A) get permissive defaults so subscribers keep the
-- access they had before per-tier permissions existed.
UPDATE mm_subscription_tiers
   SET permissions = '{
         "can_read": true,
         "can_send": true,
         "can_react": true,
         "can_comment": true,
         "can_watch_recordings": true,
         "can_join_live": true,
         "can_tip": true,
         "can_manage_room": false
       }'::jsonb
 WHERE permissions = '{}'::jsonb;
