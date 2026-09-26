-- V036: the egress meter's own state (FR-302a/b, WS-D).
--
-- PostgreSQL-only. IDEMPOTENT: run_pg_migrations heals a partially-migrated
-- database by re-running every migration.
--
-- ── Why this is a table and not derived from mm_usage_events ─────────────────
--
-- The meter subtracts a stored baseline from each new reading, so the baseline has
-- to survive an mm-core restart: without it every restart re-baselines and silently
-- loses whatever accrued since the last poll.
--
-- It could in principle be derived — MAX(cumulative) per node and source — if the
-- usage events carried the cumulative value. That is rejected for one reason:
-- **V035's ledger-maintenance door permits deleting usage events for retention and
-- erasure.** A derived baseline would then be destroyed by a retention job, and the
-- next poll would re-bill a node's entire history. The meter's state is the meter's
-- own, and it is small: one row per (node, source), not per event.
CREATE TABLE IF NOT EXISTS mm_egress_baseline (
    mm_node_id      TEXT   NOT NULL,
    -- The source as mm-switch reports it (`stream-{broadcast_id}`), kept verbatim
    -- rather than reduced to a broadcast id, so a source shape we do not recognise
    -- is still metered rather than silently dropped.
    source          TEXT   NOT NULL,
    -- The node process the counter belongs to. A CHANGE here means the node
    -- restarted and its in-memory counters went to zero: the next reading must be
    -- treated as usage in full rather than subtracted from this row
    -- (mm_fleet::metering::egress_delta).
    epoch           TEXT   NOT NULL,
    -- Cumulative bytes at the last poll, within `epoch`.
    cumulative_bytes BIGINT NOT NULL CHECK (cumulative_bytes >= 0),
    -- When that reading was taken, so a stalled meter is visible.
    observed_at     TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (mm_node_id, source)
);

-- Finding a meter that has stopped reporting is an alerting question, and a stalled
-- meter is unbilled revenue, so it gets an index rather than a sequential scan.
CREATE INDEX IF NOT EXISTS mm_egress_baseline_stale
    ON mm_egress_baseline (observed_at);

-- ── What the meter writes, on mm_usage_events ────────────────────────────────
--
-- `egress_epoch` and `cumulative_bytes` are recorded on the event too. Not as the
-- baseline — that is the table above — but so a disputed invoice can be traced back
-- to the exact counter reading that produced it, which is the whole reason a
-- customer would accept the number.
ALTER TABLE mm_usage_events ADD COLUMN IF NOT EXISTS egress_epoch TEXT;
ALTER TABLE mm_usage_events ADD COLUMN IF NOT EXISTS cumulative_bytes BIGINT;
