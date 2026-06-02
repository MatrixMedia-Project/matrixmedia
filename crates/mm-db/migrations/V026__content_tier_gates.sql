-- V026__content_tier_gates.sql
--
-- Per-content tier gate. NULL = free, anyone can watch. Non-NULL =
-- requires an active subscription in this room at >= the given level.
--
-- Nullable on purpose: existing streams/recordings stay NULL and keep
-- their pre-V026 (free) behavior. Enforcement of the gate on viewer
-- entry lands in a later stage; this migration only adds the column so
-- the create/read path can persist and echo it.

ALTER TABLE mm_streams     ADD COLUMN IF NOT EXISTS min_tier_level INT NULL;
ALTER TABLE mm_recordings  ADD COLUMN IF NOT EXISTS min_tier_level INT NULL;

-- Partial index: only gated streams carry a value, so the index stays
-- small and serves the "what are the paid streams in this room" query.
CREATE INDEX IF NOT EXISTS idx_mm_streams_room_mintier
    ON mm_streams (room_id, min_tier_level)
    WHERE min_tier_level IS NOT NULL;
