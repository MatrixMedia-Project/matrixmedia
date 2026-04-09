-- V008: Optimization indexes (pass 2)
-- Adds missing indexes and composite indexes for frequently-queried columns.

-- ============================================================================
-- mm_donations: stripe_session_id lookup (update_donation_status)
-- ============================================================================
-- The webhook handler looks up donations by stripe_session_id. Without an index
-- this is a full table scan on every Stripe webhook callback.
CREATE INDEX IF NOT EXISTS idx_donations_stripe_session
    ON mm_donations(stripe_session_id)
    WHERE stripe_session_id IS NOT NULL;

-- ============================================================================
-- mm_donations: composite index for donation feed query
-- ============================================================================
-- get_donation_feed queries:
--   WHERE stream_id = $1 AND status = 'succeeded' AND created_at > $2
--   ORDER BY created_at DESC LIMIT $3
-- A composite index covering all three columns avoids scanning the entire
-- donations table and enables an index-only scan with the ORDER BY.
CREATE INDEX IF NOT EXISTS idx_donations_feed
    ON mm_donations(stream_id, created_at DESC)
    WHERE status = 'succeeded';

-- ============================================================================
-- mm_creator_profiles: stripe_account_id lookup (onboarding webhook)
-- ============================================================================
-- set_creator_onboarding_complete queries:
--   WHERE stripe_account_id = $2
CREATE INDEX IF NOT EXISTS idx_creator_profiles_stripe_account
    ON mm_creator_profiles(stripe_account_id)
    WHERE stripe_account_id IS NOT NULL;

-- ============================================================================
-- mm_creator_profiles: partial index for onboarding_complete (list_creators)
-- ============================================================================
-- list_creators queries:
--   WHERE onboarding_complete = true ORDER BY display_name ASC
CREATE INDEX IF NOT EXISTS idx_creator_profiles_onboarded
    ON mm_creator_profiles(display_name)
    WHERE onboarding_complete = true;

-- ============================================================================
-- mm_recordings: composite index for room recordings list
-- ============================================================================
-- list_room_recordings queries:
--   WHERE room_id = $1 AND status = 'ready' ORDER BY created_at DESC
CREATE INDEX IF NOT EXISTS idx_recordings_room_ready
    ON mm_recordings(room_id, created_at DESC)
    WHERE status = 'ready';

-- ============================================================================
-- mm_recordings: partial index for retention cleanup
-- ============================================================================
-- recordings_older_than queries:
--   WHERE created_at < $1 AND status != 'deleted' ORDER BY created_at ASC
CREATE INDEX IF NOT EXISTS idx_recordings_not_deleted
    ON mm_recordings(created_at ASC)
    WHERE status != 'deleted';

-- ============================================================================
-- mm_streams: index for host_user_id (useful for admin queries)
-- ============================================================================
CREATE INDEX IF NOT EXISTS idx_streams_host
    ON mm_streams(host_user_id);
