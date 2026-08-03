-- V031: stream marker lifecycle hardening.
--
-- `ended_event_id` persists the terminal `com.matrixmedia.stream` state-event
-- id written by the shared finalize path (mirror of V020's `state_event_id`
-- for the STARTED marker).
--
-- `marker_generation` is the monotonic publish counter for the room marker:
-- 1 at stream create, incremented on every republish for the same stream id
-- (host resume, terminal event).
--
-- NOTE: V030 is taken by the MP4-transcode work; this migration is V031.

ALTER TABLE mm_streams ADD COLUMN IF NOT EXISTS ended_event_id TEXT;
ALTER TABLE mm_streams ADD COLUMN IF NOT EXISTS marker_generation INTEGER NOT NULL DEFAULT 1;
