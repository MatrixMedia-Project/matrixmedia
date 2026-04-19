-- V014: Per-creator ad opt-out
--
-- Default true: ads remain enabled for any creator who hasn't explicitly
-- opted out. Set to false to disable all ad serving on this creator's
-- streams (pre-roll, mid-roll, post-roll).

ALTER TABLE mm_creator_defaults
    ADD COLUMN IF NOT EXISTS ads_enabled BOOLEAN NOT NULL DEFAULT true;
