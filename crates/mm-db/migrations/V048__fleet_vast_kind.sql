-- V048: Vast.ai joins the provider kinds (read-only checks; no renting).
--
-- The kind list MUST match mm_fleet::providers_db::KINDS. Postgres cannot amend a CHECK
-- expression in place, so the old constraint is dropped and re-added with 'vast'. The
-- default inline name is mm_fleet_providers_kind_check ({table}_{column}_check).
-- Re-runnable like every migration here: DROP IF EXISTS, then ADD.
ALTER TABLE mm_fleet_providers DROP CONSTRAINT IF EXISTS mm_fleet_providers_kind_check;
ALTER TABLE mm_fleet_providers ADD CONSTRAINT mm_fleet_providers_kind_check
    CHECK (kind IN ('scaleway','runpod','akamai','ovh','gcp','vast'));
