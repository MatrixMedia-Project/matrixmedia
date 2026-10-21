-- V050: the dashboard's zone/GPU-size dropdowns read the live provider catalog through
-- a runner-mediated `catalog` request (answered like a Test connection, from the stored
-- token — a loaded catalog proves the token).
--
-- The kind list MUST match the match in `admin_fleet_providers::create_request`.
-- Postgres cannot amend a CHECK expression in place, so the old constraint is dropped
-- and re-added with 'catalog'. The default inline name is
-- mm_fleet_requests_kind_check ({table}_{column}_check).
-- Re-runnable like every migration here: DROP IF EXISTS, then ADD.
ALTER TABLE mm_fleet_requests DROP CONSTRAINT IF EXISTS mm_fleet_requests_kind_check;
ALTER TABLE mm_fleet_requests ADD CONSTRAINT mm_fleet_requests_kind_check
    CHECK (kind IN ('test_connection','test_boot','catalog'));
