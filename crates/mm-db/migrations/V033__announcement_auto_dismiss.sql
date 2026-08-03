-- V033: optional auto-dismiss timeout for announcement banners.
--
-- When non-null, clients auto-hide the banner this many seconds after it is
-- first shown (in addition to the manual dismiss available when
-- `dismissible = true`). Null = persistent until manually dismissed / expired
-- (the previous behaviour). Used by the demo-disclaimer banner.
ALTER TABLE mm_announcements
    ADD COLUMN IF NOT EXISTS auto_dismiss_secs INTEGER
        CHECK (auto_dismiss_secs IS NULL OR auto_dismiss_secs BETWEEN 1 AND 3600);
