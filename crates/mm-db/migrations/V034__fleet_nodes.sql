-- V034: broadcast fleet inventory (WS-A Task 1).
--
-- PostgreSQL-only by construction: SQLite deployments are single-node and have
-- no fleet (see crates/mm-db/src/migrations.rs, which is deliberately NOT
-- touched by this change).
--
-- EVERY statement here MUST be idempotent. run_pg_migrations heals a
-- partially-migrated database by re-running every migration, so a statement
-- that fails on a second execution wedges boot permanently. That is why state
-- columns are TEXT + CHECK rather than enums: CREATE TYPE has no IF NOT EXISTS
-- in any PostgreSQL version, including the 16.13 running in production.

-- ── Generation counter (FR-212) ──────────────────────────────────────────────
--
-- ONE global row, not one per node. The fleet runner polls this to learn that
-- the desired set changed. A per-row counter cannot express "the last row was
-- deleted", so teardown would be invisible and nothing would ever be reaped.
CREATE TABLE IF NOT EXISTS mm_fleet_generation (
    id          BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    generation  BIGINT      NOT NULL DEFAULT 1,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO mm_fleet_generation (id) VALUES (TRUE) ON CONFLICT (id) DO NOTHING;

-- ── Desired set: what mm-core wants to exist ─────────────────────────────────
--
-- destroy_deadline lives HERE, not only on mm_fleet_nodes, because it is
-- written BEFORE any provider call. On mm_fleet_nodes alone the cost-safety
-- invariant would not exist during the exact window it is meant to cover: the
-- gap between "we asked a provider for a paid machine" and "we recorded that we
-- own one".
CREATE TABLE IF NOT EXISTS mm_fleet_desired (
    mm_node_id       TEXT PRIMARY KEY,
    flavor           TEXT NOT NULL CHECK (flavor    IN ('origin','fanout','edge','transcode')),
    ownership        TEXT NOT NULL CHECK (ownership IN ('owned','leased','rented')),
    region           TEXT NOT NULL,
    size             TEXT NOT NULL,
    broadcast_id     TEXT,
    requested_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    destroy_deadline TIMESTAMPTZ,
    CONSTRAINT desired_rented_needs_deadline
        CHECK (ownership <> 'rented' OR destroy_deadline IS NOT NULL),
    CONSTRAINT desired_nonrented_has_no_deadline
        CHECK (ownership = 'rented' OR destroy_deadline IS NULL)
);

-- ── Observed set: what actually exists ───────────────────────────────────────
--
-- Only ownership = 'rented' is reapable. An owned or leased node that somehow
-- carried a destroy_deadline would be destroyed by the sweeper, which is why
-- the negative constraint is as important as the positive one.
CREATE TABLE IF NOT EXISTS mm_fleet_nodes (
    mm_node_id         TEXT PRIMARY KEY,
    flavor             TEXT NOT NULL CHECK (flavor    IN ('origin','fanout','edge','transcode')),
    ownership          TEXT NOT NULL CHECK (ownership IN ('owned','leased','rented')),
    provider           TEXT NOT NULL,
    provider_id        TEXT,
    public_ip          INET,
    state              TEXT NOT NULL DEFAULT 'requested'
        CHECK (state IN ('requested','booting','healthy','draining','destroying','gone')),
    hmac_secret_enc    BYTEA,
    viewer_capacity    INTEGER,
    viewers_current    INTEGER NOT NULL DEFAULT 0,
    billing_started_at TIMESTAMPTZ,
    renewal_due_at     TIMESTAMPTZ,
    destroy_deadline   TIMESTAMPTZ,
    CONSTRAINT rented_needs_deadline
        CHECK (ownership <> 'rented' OR destroy_deadline IS NOT NULL),
    CONSTRAINT nonrented_has_no_deadline
        CHECK (ownership = 'rented' OR destroy_deadline IS NULL),
    CONSTRAINT leased_needs_renewal
        CHECK (ownership <> 'leased' OR renewal_due_at IS NOT NULL)
);

-- Partial indexes on the sweeper predicate: the hot path of the cost-safety
-- invariant, and the only query that runs on a timer forever.
CREATE INDEX IF NOT EXISTS mm_fleet_nodes_reap
    ON mm_fleet_nodes (destroy_deadline) WHERE ownership = 'rented';
CREATE INDEX IF NOT EXISTS mm_fleet_desired_reap
    ON mm_fleet_desired (destroy_deadline) WHERE ownership = 'rented';
CREATE INDEX IF NOT EXISTS mm_fleet_nodes_alloc
    ON mm_fleet_nodes (flavor, state);

-- ── Generation trigger (FR-212) ──────────────────────────────────────────────
--
-- FOR EACH STATEMENT, so a multi-row DELETE bumps once. Needs PG >= 11 for
-- EXECUTE FUNCTION; production is 16.13.
CREATE OR REPLACE FUNCTION mm_fleet_bump_generation() RETURNS trigger AS $$
BEGIN
    UPDATE mm_fleet_generation SET generation = generation + 1, updated_at = now();
    RETURN NULL;
END $$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS mm_fleet_desired_generation ON mm_fleet_desired;
CREATE TRIGGER mm_fleet_desired_generation
    AFTER INSERT OR UPDATE OR DELETE ON mm_fleet_desired
    FOR EACH STATEMENT EXECUTE FUNCTION mm_fleet_bump_generation();
