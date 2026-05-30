-- V024: feed engagement counters + author creator profile pointer.
-- Reactions and threaded-reply (comments) counts are per (user_id, event_id)
-- because mm_feed_items is denormalized fan-out — each user's row gets the
-- same count updated when the AS sees a relating event in the source room.
-- author_creator_profile_id lets the client render tip/upgrade affordances
-- on post cards without a second mm-core round-trip.
ALTER TABLE mm_feed_items
    ADD COLUMN IF NOT EXISTS reactions_count           INT  NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS comments_count            INT  NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS author_creator_profile_id TEXT;

-- Index for the AS handler's UPDATE WHERE event_id = $1 path (reactions /
-- comments / redactions all hit this lookup).
CREATE INDEX IF NOT EXISTS idx_mm_feed_items_event_id
    ON mm_feed_items (event_id);

-- Rev-lookup index: maps a reaction or thread-reply event_id back to the
-- feed event it targets so a later m.room.redaction can decrement the
-- appropriate counter without re-scanning every feed row.
--
-- Why a separate table (not an inline column on mm_feed_items):
--   - A reaction's event_id is NOT itself a feed item — only its target is.
--   - We need to look up the target by reaction_event_id at redaction time.
--   - Adding a sparse column to mm_feed_items would either bloat every row
--     or require a separate query path anyway.
-- The kind column lets us count reactions and thread-replies independently
-- (so a redacted reaction doesn't decrement comments_count and vice versa).
CREATE TABLE IF NOT EXISTS mm_feed_engagement_refs (
    event_id        TEXT    PRIMARY KEY,         -- the reaction or reply event
    target_event_id TEXT    NOT NULL,            -- the feed event being counted
    room_id         TEXT    NOT NULL,
    kind            TEXT    NOT NULL,            -- 'reaction' | 'thread_reply'
    created_at      BIGINT  NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mm_feed_engagement_refs_target
    ON mm_feed_engagement_refs (target_event_id);
