-- deploy/sql/mm_media_worker_role.sql
-- mm-media-worker's database identity (ad conversion, internal spec 2026-10-11 §5.3).
-- Idempotent: re-run after every upgrade, once mm-core has migrated.
-- The operator sets the password afterwards: ALTER ROLE mm_media_worker PASSWORD '...';
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mm_media_worker') THEN
        CREATE ROLE mm_media_worker LOGIN;
    END IF;
END $$;

GRANT USAGE ON SCHEMA public TO mm_media_worker;
-- The queue: claim, heartbeat, finish; INSERT for the legacy import. Never DELETE.
GRANT SELECT, INSERT, UPDATE ON mm_ad_conversion_jobs TO mm_media_worker;
GRANT SELECT, INSERT, UPDATE, DELETE ON mm_ad_renditions TO mm_media_worker;
GRANT SELECT, INSERT, UPDATE ON mm_media_worker_status TO mm_media_worker;
-- Creatives: read all, write only the media columns the publish/fail paths set.
GRANT SELECT ON mm_ad_creatives TO mm_media_worker;
GRANT UPDATE (status, active_job_id, media_warnings, cdn_url, duration_secs, file_size_bytes,
              mime_type, updated_at, media_purge_after)
    ON mm_ad_creatives TO mm_media_worker;
-- The worker never reads the migration ledger. Revoking on every run keeps a re-run idempotent
-- even if an earlier version of this script, or an operator, granted it.
REVOKE ALL ON mm_schema_migrations FROM mm_media_worker;
