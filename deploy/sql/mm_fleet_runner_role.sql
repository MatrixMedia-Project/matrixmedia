-- deploy/sql/mm_fleet_runner_role.sql
-- The fleet runner's database identity. Idempotent: safe to re-run on every deploy.
-- Re-run after every upgrade, once mm-core has migrated: a release may add fleet tables or columns the runner needs.
-- The operator sets the password afterwards: ALTER ROLE mm_fleet_runner PASSWORD '...';
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mm_fleet_runner') THEN
        CREATE ROLE mm_fleet_runner LOGIN;
    END IF;
END $$;

GRANT USAGE ON SCHEMA public TO mm_fleet_runner;
GRANT SELECT, INSERT, UPDATE, DELETE ON
    mm_fleet_providers, mm_fleet_provider_zones, mm_fleet_provider_sizes,
    mm_fleet_provider_status, mm_fleet_zone_cooldown, mm_fleet_requests,
    mm_fleet_control, mm_fleet_boot_tokens, mm_fleet_ops_audit,
    mm_fleet_desired, mm_fleet_nodes, mm_fleet_generation
    TO mm_fleet_runner;
-- Credentials: read to open, update only for rotate-key. Never insert or delete.
GRANT SELECT, UPDATE ON mm_fleet_provider_credentials TO mm_fleet_runner;
GRANT USAGE, SELECT ON SEQUENCE mm_fleet_ops_audit_id_seq TO mm_fleet_runner;
-- Settings: the runner reads fleet.mode and the revision; it never writes a setting.
GRANT SELECT ON mm_settings TO mm_fleet_runner;
-- The runner never reads the migration ledger (its start-up probe selects mm_fleet_requests.params).
-- An earlier version of this script granted it; revoking on every run keeps a re-run idempotent.
REVOKE ALL ON mm_schema_migrations FROM mm_fleet_runner;
