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

-- How a lease was released: NULL (not released), 'drain' (stop placement, keep extending
-- while viewers remain — spec §8.2 R5 keep-rule), or 'force' (operator eviction: the
-- extension pass must never extend it again, whatever its viewers say). Durable here, on
-- the desired row, so a forced release survives every later extension tick.
ALTER TABLE mm_fleet_desired
    ADD COLUMN IF NOT EXISTS lease_release TEXT
        CHECK (lease_release IS NULL OR lease_release IN ('drain', 'force'));

-- Composite partial unique index for lease idempotency (Review Focus 4). One request may
-- write SEVERAL rows (count 1..=5), so uniqueness is per (request, node), not per request:
-- the node ids are derived deterministically from the request id, so a replayed create
-- computes the same (lease_request_id, mm_node_id) pairs and conflicts instead of
-- duplicating. A NULL lease_request_id (not a lease) does not participate.
DROP INDEX IF EXISTS mm_fleet_desired_lease_request_id_unique;
CREATE UNIQUE INDEX IF NOT EXISTS mm_fleet_desired_lease_request_node_unique
    ON mm_fleet_desired (lease_request_id, mm_node_id) WHERE lease_request_id IS NOT NULL;

-- Usage samples: sampled at the relay, one row per (mm_node_id, broadcast_id, sampled_at).
-- Deleted after metering (Task 9).
CREATE TABLE IF NOT EXISTS mm_relay_usage_samples (
    mm_node_id TEXT NOT NULL,
    broadcast_id TEXT NOT NULL,
    sampled_at TIMESTAMPTZ NOT NULL,
    viewers INT NOT NULL,
    PRIMARY KEY (mm_node_id, broadcast_id, sampled_at)
);
