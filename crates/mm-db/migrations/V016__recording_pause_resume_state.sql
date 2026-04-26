-- V016 — single-file pause/resume recording state.
--
-- The status column already exists with values like 'recording',
-- 'ready', 'failed'. We add 'paused' so we can track the period
-- between a host pressing pause and either resuming or finalising.
--
-- The mm-switch path produces a single .webm file per stream session,
-- so segment is now meaningful only as a "1 always" sentinel for
-- mm-switch sources (LiveKit egress recordings keep using segment for
-- their _seg{N}.mp4 grouping). No data migration is needed for that.

-- Postgres CHECK constraint on status (if any) needs to permit
-- 'paused'. Defensive: drop + re-add if present.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_name = 'mm_recordings' AND column_name = 'status'
    ) THEN
        ALTER TABLE mm_recordings
            DROP CONSTRAINT IF EXISTS mm_recordings_status_check;
    END IF;
END $$;

ALTER TABLE mm_recordings
    ADD CONSTRAINT mm_recordings_status_check
    CHECK (status IN ('recording', 'paused', 'ready', 'failed'));
