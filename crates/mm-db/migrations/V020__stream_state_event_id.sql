-- V020: persist the STARTED `com.matrixmedia.stream` state-event id on
-- each stream row so clients can anchor the stream-comments thread on a
-- real Matrix event even when the room timeline has no loaded marker
-- (synthetic past-broadcast bubbles). Nullable: legacy rows stay NULL
-- and fall back to the client-side timestamp scan.
ALTER TABLE mm_streams ADD COLUMN IF NOT EXISTS state_event_id TEXT;
