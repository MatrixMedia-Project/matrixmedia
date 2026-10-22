-- V054: a manual lease's rolling-window size (Task 10, Task 7's carried ruling (b)).
--
-- `LEASE_WINDOW` (a crate constant) is replaced by `duration_hint`: a per-lease parameter
-- that sizes how far `create` and every extension-pass tick push the rolling deadline out.
-- Stored on the desired row so the extension pass (which reads every manual lease's row,
-- not the API call that created it) knows how far to push each node's deadline, rather than
-- re-guessing a single global window for every lease.
--
-- Re-runnable like every migration here: ADD COLUMN IF NOT EXISTS.
ALTER TABLE mm_fleet_desired
    ADD COLUMN IF NOT EXISTS lease_duration_hint_secs INT;

-- LAST on purpose: mm-db/src/lib.rs's adoption probe keys on the newest migration's LAST
-- statement being a `CREATE TABLE IF NOT EXISTS` (migration_registry_tests enforces this).
-- Same marker pattern as V053: the table's presence alone proves this migration ran to
-- completion rather than dying after the ALTER and being adopted as complete anyway.
CREATE TABLE IF NOT EXISTS mm_lease_duration_hint_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
