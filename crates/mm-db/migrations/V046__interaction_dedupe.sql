-- V046: one interaction row per (user, stream, action).
--
-- POST /discover/interactions inserted unconditionally, so a viewer tapping
-- the heart N times (or toggling like off/on, which re-POSTs) created N
-- "like" rows and inflated every count. The handler now upserts; this
-- migration heals the existing duplicates and enforces the invariant.
--
-- Heal first: keep the earliest row per key, drop the rest. The unique
-- index creation below would fail on duplicate keys otherwise. Both
-- statements are re-runnable (the runner re-runs migrations to heal a
-- partially migrated database).
DELETE FROM mm_user_interactions a
USING mm_user_interactions b
WHERE a.user_id = b.user_id
  AND a.stream_id = b.stream_id
  AND a.action_type = b.action_type
  AND (a.created_at, a.id) > (b.created_at, b.id);

CREATE UNIQUE INDEX IF NOT EXISTS uq_interactions_user_stream_action
    ON mm_user_interactions(user_id, stream_id, action_type);
