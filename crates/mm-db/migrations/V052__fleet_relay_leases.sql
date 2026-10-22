-- V052: Relay lease metadata + usage sampling
--
-- Two new features:
-- 1. mm_fleet_desired tracks lease source (manual/autoscale) and lease request ID for
--    idempotency (Review Focus 4).
-- 2. mm_relay_usage_samples records per-broadcast viewer counts, sampled at the relay;
--    deleted after metering (Task 9).
--
-- Re-runnable like every migration here: ADD COLUMN IF NOT EXISTS, CREATE TABLE IF NOT EXISTS.

-- Lease metadata: source ('manual' or 'autoscale', NULL = not a lease) and request ID (idempotency).
ALTER TABLE mm_fleet_desired
    ADD COLUMN IF NOT EXISTS lease_source TEXT
        CHECK (lease_source IS NULL OR lease_source IN ('manual', 'autoscale'));

ALTER TABLE mm_fleet_desired
    ADD COLUMN IF NOT EXISTS lease_request_id UUID;

-- Partial unique index on lease_request_id for idempotency (Review Focus 4).
-- A NULL lease_request_id does not participate in uniqueness — this allows non-leased
-- nodes and leases without explicit request IDs without creating index conflicts.
CREATE UNIQUE INDEX IF NOT EXISTS mm_fleet_desired_lease_request_id_unique
    ON mm_fleet_desired (lease_request_id) WHERE lease_request_id IS NOT NULL;

-- Usage samples: sampled at the relay, one row per (mm_node_id, broadcast_id, sampled_at).
-- Deleted after metering (Task 9).
CREATE TABLE IF NOT EXISTS mm_relay_usage_samples (
    mm_node_id TEXT NOT NULL,
    broadcast_id TEXT NOT NULL,
    sampled_at TIMESTAMPTZ NOT NULL,
    viewers INT NOT NULL,
    PRIMARY KEY (mm_node_id, broadcast_id, sampled_at)
);
