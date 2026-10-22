-- V053: relay-hour meter, viewer-share apportionment to wallets, house fallback
-- (spec §8.2 R4, Task 9).
--
-- Two widenings of existing CHECK constraints, done by DROP + ADD (the columns and
-- constraints already exist from V035/V043; there is nothing to add-if-not-exists):
--
-- 1. A new billable unit, 'relay_hour': the manual-lease meter's usage rows.
-- 2. A new payer, 'house': an apportioned slice with no wallet to bill (an uncovered
--    broadcast's share, or the whole period when a relay reported zero samples) —
--    mm_core::fleet::funding::Payer::House. The string forms MUST match that enum;
--    ddl_agreement_with_v053 in mm-core reads this file to prove it.
--
-- Idempotent: DROP CONSTRAINT IF EXISTS before each ADD, so a re-run never errors on
-- "constraint already exists".

ALTER TABLE mm_rate_card DROP CONSTRAINT IF EXISTS mm_rate_card_unit_check;
ALTER TABLE mm_rate_card
    ADD CONSTRAINT mm_rate_card_unit_check
    CHECK (unit IN ('egress_gb', 'gpu_minute', 'node_minute', 'storage_gb_month', 'relay_hour'));

ALTER TABLE mm_usage_events DROP CONSTRAINT IF EXISTS mm_usage_events_unit_check;
ALTER TABLE mm_usage_events
    ADD CONSTRAINT mm_usage_events_unit_check
    CHECK (unit IN ('egress_gb', 'gpu_minute', 'node_minute', 'storage_gb_month', 'relay_hour'));

ALTER TABLE mm_usage_events DROP CONSTRAINT IF EXISTS usage_payer_valid;
ALTER TABLE mm_usage_events
    ADD CONSTRAINT usage_payer_valid
    CHECK (payer IN ('wallet', 'platform_ads', 'platform_free', 'operator', 'house'));

-- LAST on purpose: mm-db/src/lib.rs's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (migration_registry_tests
-- enforces this). V053 has nothing to create on its own — it only widens existing
-- CHECK constraints — so this marker table's sole job is to BE that last statement,
-- proving the ALTERs above actually ran rather than dying partway and being adopted
-- as complete anyway.
CREATE TABLE IF NOT EXISTS mm_relay_hour_meter_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
