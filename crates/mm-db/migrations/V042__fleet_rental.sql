-- GPU rental (spec 2026-10-06, phase P-B). Idempotent on purpose: run_pg_migrations heals a
-- partially migrated database by re-running every file, so every statement is IF NOT EXISTS.

-- The size a node was created with. The desired row also has it, but teardown deletes the
-- desired row before the destroy, and the cost estimate and the GPU servers card need it after.
ALTER TABLE mm_fleet_nodes ADD COLUMN IF NOT EXISTS size TEXT;

-- Live nodes per provider: counted by every rental (caps) and every provider delete.
CREATE INDEX IF NOT EXISTS mm_fleet_nodes_provider_live
    ON mm_fleet_nodes (provider_ref) WHERE state <> 'gone';

-- A boot report is matched by its token's hash: one hash, one node.
CREATE UNIQUE INDEX IF NOT EXISTS mm_fleet_boot_tokens_hash
    ON mm_fleet_boot_tokens (token_hash);

-- When the runner wrote a node's row: the clock a create of unknown outcome settles on. A row is
-- written BEFORE its create call, so this is when that call was (about to be) sent. Stamped by the
-- database (its transaction start); the runner's settle check compares it with the tick's `now`,
-- which the fleet loop reads from the database's own clock, so both sides are one clock.
ALTER TABLE mm_fleet_nodes ADD COLUMN IF NOT EXISTS requested_at TIMESTAMPTZ NOT NULL DEFAULT now();

-- What mm-core fixes when it queues a request (a test boot's report URL).
-- LAST on purpose: the adoption probe keys on this column.
ALTER TABLE mm_fleet_requests ADD COLUMN IF NOT EXISTS params JSONB NOT NULL DEFAULT '{}'::jsonb;
