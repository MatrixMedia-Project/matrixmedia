-- MM Ads P0: player-measured viewability proof (see WorkingDirectory/MM_ADS_PLAN.md §2.4).
--
-- viewable_at: first player heartbeat meeting the MRC video rule
-- (>=50% pixels, >=2 continuous seconds), stamped once by
-- ImpressionService::record_viewability. NULL = served-but-undetermined:
-- counted in impressions, never in viewables (MRC-honest reporting).
-- max_coverage_pct: highest coverage observed across heartbeats (0-100).

ALTER TABLE mm_ad_impressions ADD COLUMN IF NOT EXISTS viewable_at TIMESTAMPTZ;
ALTER TABLE mm_ad_impressions ADD COLUMN IF NOT EXISTS max_coverage_pct REAL;

CREATE INDEX IF NOT EXISTS idx_impressions_viewable
    ON mm_ad_impressions(ad_id, viewable_at DESC) WHERE viewable_at IS NOT NULL;
