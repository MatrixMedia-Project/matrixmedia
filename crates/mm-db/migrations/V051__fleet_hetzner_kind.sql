-- V051: Hetzner Cloud joins the provider kinds (fan-out relay renting; read-only
-- checks for transcode sizes it does not offer).
--
-- The kind list MUST match mm_fleet::providers_db::KINDS. Postgres cannot amend a CHECK
-- expression in place, so the old constraint is dropped and re-added with 'hetzner'. The
-- default inline name is mm_fleet_providers_kind_check ({table}_{column}_check).
-- Re-runnable like every migration here: DROP IF EXISTS, then ADD.
ALTER TABLE mm_fleet_providers DROP CONSTRAINT IF EXISTS mm_fleet_providers_kind_check;
ALTER TABLE mm_fleet_providers ADD CONSTRAINT mm_fleet_providers_kind_check
    CHECK (kind IN ('scaleway','runpod','akamai','ovh','gcp','vast','hetzner'));
