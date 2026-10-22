-- V055: provider purpose (Transcode vs Relay).
--
-- A provider's job, orthogonal to its vendor kind: `transcode` (GPU work:
-- test boots, rescale, AI filters) or `relay` (hourly fan-out capacity
-- multiplying outgoing traffic). Defaults to transcode, so every existing
-- row keeps its behavior; the dashboard sets relay for fan-out pools.
--
-- Re-runnable like every migration here: ADD IF NOT EXISTS, then the CHECK.
-- The kind-list discipline (V051) applies: PURPOSES in providers_db.rs MUST
-- match the CHECK below.
ALTER TABLE mm_fleet_providers ADD COLUMN IF NOT EXISTS purpose TEXT NOT NULL DEFAULT 'transcode';
ALTER TABLE mm_fleet_providers DROP CONSTRAINT IF EXISTS mm_fleet_providers_purpose_check;
ALTER TABLE mm_fleet_providers ADD CONSTRAINT mm_fleet_providers_purpose_check
    CHECK (purpose IN ('transcode','relay'));

-- LAST on purpose: mm-db/src/lib.rs's adoption probe keys on the newest migration's LAST
-- statement being a `CREATE TABLE IF NOT EXISTS` (migration_registry_tests enforces this).
-- Same marker pattern as V054: the table's presence alone proves this migration ran to
-- completion rather than dying after the ALTER and being adopted as complete anyway.
CREATE TABLE IF NOT EXISTS mm_provider_purpose_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
