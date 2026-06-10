-- V030: server-side MP4 rendition of recordings (mm-switch finalise
-- transcode). The .webm in storage_key remains the source of truth;
-- mp4_status tracks the derived H.264/AAC rendition independently of
-- the recording's own status column.
--   mp4_status: 'none' | 'pending' | 'ready' | 'failed'
--   mp4_key:    storage key of the rendition, e.g. '/data/recordings/{id}.mp4'
ALTER TABLE mm_recordings ADD COLUMN IF NOT EXISTS mp4_status TEXT NOT NULL DEFAULT 'none';
ALTER TABLE mm_recordings ADD COLUMN IF NOT EXISTS mp4_key TEXT;

-- Startup-sweep + backfill queries filter on pending rows only.
CREATE INDEX IF NOT EXISTS idx_recordings_mp4_pending
    ON mm_recordings(mp4_status)
    WHERE mp4_status = 'pending';
