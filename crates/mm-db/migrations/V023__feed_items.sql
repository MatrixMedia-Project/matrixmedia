-- V023: feed index — one row per (user, feed-event). Read accelerator for
-- GET /_mm/client/v1/feed. Fan-out-on-write for v1; GC at 90 days (Phase 2).
CREATE TABLE IF NOT EXISTS mm_feed_items (
    id          BYTEA       PRIMARY KEY,
    user_id     TEXT        NOT NULL,
    room_id     TEXT        NOT NULL,
    event_id    TEXT        NOT NULL,
    kind        TEXT        NOT NULL,
    ts          BIGINT      NOT NULL,
    origin      TEXT        NOT NULL,
    payload     JSONB       NOT NULL,
    seen_at     BIGINT,
    hidden      BOOLEAN     NOT NULL DEFAULT FALSE,
    CONSTRAINT uq_feed_user_event UNIQUE (user_id, event_id)
);
CREATE INDEX IF NOT EXISTS idx_mm_feed_items_user_ts
    ON mm_feed_items (user_id, ts DESC);
CREATE INDEX IF NOT EXISTS idx_mm_feed_items_room_ts
    ON mm_feed_items (room_id, ts DESC);
CREATE INDEX IF NOT EXISTS idx_mm_feed_items_kind
    ON mm_feed_items (kind);

-- Persist the started-event id on the stream row for broadcast.ended relation.
ALTER TABLE mm_streams ADD COLUMN IF NOT EXISTS feed_started_event_id TEXT;
