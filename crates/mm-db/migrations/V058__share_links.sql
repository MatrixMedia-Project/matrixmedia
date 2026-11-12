-- V058: share links — per-target link access + revocable share grants.
--
-- `link_access` on `mm_streams` / `mm_recordings` is the owner's standing
-- posture for link-based access: 'members' (default; the pre-V058 behavior
-- — a link alone grants nothing) or 'public' (a live share grant for the
-- target is honored for anonymous viewers). Existing rows backfill to
-- 'members' via the column default, so nothing becomes public by applying
-- this migration.
--
-- `mm_share_grants` is one row per issued share link: `kind` is 'stream' |
-- 'recording', `target_id` is the mm_streams/mm_recordings id the link
-- points at (TEXT, no FK — the two target tables can't share one FK, and a
-- dangling grant for a deleted target simply resolves to nothing).
-- `created_by` is the Matrix user id that minted the link. A grant is live
-- while `revoked_at IS NULL AND expires_at > now()`; revocation is a
-- timestamp (not a delete) so a revoked link stays distinguishable from a
-- never-issued one, and `last_used_at` records the most recent redemption.
-- Expired rows are deleted by the reaper (the partial index below serves
-- both the liveness check and the reaper sweep).
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests). V058 creates the `mm_share_links_migrated`
-- marker table in its LAST statement, so a V058 that died partway —
-- marker table not yet created — is not adopted.
ALTER TABLE mm_streams
    ADD COLUMN IF NOT EXISTS link_access TEXT NOT NULL DEFAULT 'members'
        CONSTRAINT streams_link_access_valid
        CHECK (link_access IN ('members', 'public'));

ALTER TABLE mm_recordings
    ADD COLUMN IF NOT EXISTS link_access TEXT NOT NULL DEFAULT 'members'
        CONSTRAINT recordings_link_access_valid
        CHECK (link_access IN ('members', 'public'));

CREATE TABLE IF NOT EXISTS mm_share_grants (
    id           UUID PRIMARY KEY,
    kind         TEXT NOT NULL
        CONSTRAINT share_grants_kind_valid
        CHECK (kind IN ('stream', 'recording')),
    target_id    TEXT NOT NULL,
    created_by   TEXT NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL,
    revoked_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_share_grants_target
    ON mm_share_grants(kind, target_id);

-- Liveness lookups and the expiry reaper only touch unrevoked rows.
CREATE INDEX IF NOT EXISTS idx_share_grants_live_expires
    ON mm_share_grants(expires_at)
    WHERE revoked_at IS NULL;

CREATE TABLE IF NOT EXISTS mm_share_links_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
