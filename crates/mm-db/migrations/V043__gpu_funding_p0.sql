-- V043: GPU funding, P0 — who pays for a node and for a usage row, and when a node
-- stopped billing (design 2026-10-11-gpu-funding-design.md §4.2, §4.3, §5).
--
-- Idempotent throughout (run_pg_migrations re-runs every migration on an unadopted
-- database): ADD COLUMN IF NOT EXISTS, TEXT + CHECK rather than CREATE TYPE.
-- The string forms MUST match mm_core::fleet::funding::{Funding, Payer} — the
-- ddl_agreement test there reads this file.

-- Who pays for a node, written when the planner orders it and copied to the node
-- row by the runner at create. One payer per node for life (§3.4 "sticky payer").
ALTER TABLE mm_fleet_desired
    ADD COLUMN IF NOT EXISTS funding TEXT NOT NULL DEFAULT 'wallet'
        CONSTRAINT desired_funding_valid CHECK (funding IN ('wallet','platform_ads','operator'));
ALTER TABLE mm_fleet_nodes
    ADD COLUMN IF NOT EXISTS funding TEXT NOT NULL DEFAULT 'wallet'
        CONSTRAINT nodes_funding_valid CHECK (funding IN ('wallet','platform_ads','operator'));

-- Test boots were always the operator's cost (provider spec §6.3); say so in the column.
UPDATE mm_fleet_desired SET funding = 'operator' WHERE purpose = 'test_boot' AND funding <> 'operator';
UPDATE mm_fleet_nodes   SET funding = 'operator' WHERE purpose = 'test_boot' AND funding <> 'operator';

-- The GPU-minute meter's own clock, separate from the egress baselines (§4.2).
ALTER TABLE mm_fleet_nodes ADD COLUMN IF NOT EXISTS last_metered_at TIMESTAMPTZ;

-- Who pays for a usage row. Non-wallet rows are inserted already rated and never
-- enter the rating queue (§4.3); the partial index is for the platform cost report.
ALTER TABLE mm_usage_events
    ADD COLUMN IF NOT EXISTS payer TEXT NOT NULL DEFAULT 'wallet'
        CONSTRAINT usage_payer_valid CHECK (payer IN ('wallet','platform_ads','platform_free','operator'));
CREATE INDEX IF NOT EXISTS mm_usage_platform_by_day
    ON mm_usage_events (payer, occurred_at) WHERE payer <> 'wallet';

-- LAST on purpose: the adoption probe in mm-db/src/lib.rs keys on this column.
-- Stamped by DesiredStore::complete_teardown when the node goes `gone`; the meter
-- closes the node's last interval at it (§4.2).
ALTER TABLE mm_fleet_nodes ADD COLUMN IF NOT EXISTS gone_at TIMESTAMPTZ;
