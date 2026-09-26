-- V037: the demotion ladder's state and its audit trail (WS-D, design §17.4).
--
-- `mm-core::fleet::ladder` decides which rung a broadcast is on; V037 is where that
-- decision is remembered between evaluations and where every transition is recorded
-- with the statement of reasons CR-604 requires.
--
-- Two tables, because they answer different questions and have different lifetimes:
-- the current rung is one mutable row per broadcast, and the transitions are an
-- append-only history that outlives the broadcast (a statement of reasons that can be
-- overwritten is not a statement of reasons).
--
-- Idempotent throughout: `mm-db/src/lib.rs` heals a partial migration by re-running
-- every migration, so non-idempotent DDL wedges boot permanently. TEXT + CHECK rather
-- than CREATE TYPE … AS ENUM for the same reason.

-- ---------------------------------------------------------------------------
-- Where each broadcast currently sits.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS mm_broadcast_demotion (
    broadcast_id            TEXT        PRIMARY KEY,

    step                    TEXT        NOT NULL
        CHECK (step IN ('healthy', 'stop_provisioning', 'reduce_quality',
                        'drain_to_origin', 'end_with_slate', 'overrun')),

    -- When this rung was entered, as opposed to when it was last confirmed. The
    -- difference is what says "degraded for 40 minutes" rather than "degraded".
    entered_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    evaluated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- The numbers that produced the rung, kept because the watermarks are
    -- placeholders (design §17.7) and these are the evidence for setting them.
    balance_minor           BIGINT      NOT NULL,
    projected_cost_minor    BIGINT      NOT NULL,

    -- Hysteresis. Consecutive evaluations that wanted a MILDER rung than the current
    -- one. Demotion is immediate; recovery waits for this to reach the configured
    -- dwell, because a balance hovering at a watermark would otherwise flip the rung
    -- every tick — and a viewer is better served by one degradation than by quality
    -- that oscillates (FR-311).
    milder_streak           INTEGER     NOT NULL DEFAULT 0 CHECK (milder_streak >= 0),

    -- Whether the rung was ACTED ON or merely observed. In the default mode the
    -- ladder evaluates and records and changes nothing, so a row saying
    -- `drain_to_origin` does not mean anyone was drained. Without this column the
    -- table would read as a log of actions that never happened.
    actuated                BOOLEAN     NOT NULL DEFAULT FALSE
);

-- The operator console's question is "who is degraded right now", which is a scan of
-- everything not healthy — a small set, and one that stays small.
CREATE INDEX IF NOT EXISTS mm_broadcast_demotion_degraded_idx
    ON mm_broadcast_demotion (step, entered_at)
    WHERE step <> 'healthy';

-- ---------------------------------------------------------------------------
-- Every transition, with its statement of reasons (CR-604).
-- ---------------------------------------------------------------------------
--
-- CR-604 requires a statement of reasons for every demotion, issued by the time the
-- restriction takes effect. It is written here in the same pass that applies the
-- restriction, so a restriction without its statement is not a state the code can
-- reach by forgetting.
CREATE TABLE IF NOT EXISTS mm_demotion_events (
    id                      BIGSERIAL   PRIMARY KEY,
    broadcast_id            TEXT        NOT NULL,
    user_id                 TEXT        NOT NULL,

    from_step               TEXT        NOT NULL,
    to_step                 TEXT        NOT NULL,

    balance_minor           BIGINT      NOT NULL,
    projected_cost_minor    BIGINT      NOT NULL,

    -- Addressed to the broadcaster, in their terms, not ours.
    statement               TEXT        NOT NULL CHECK (length(statement) > 0),

    -- False when the ladder was in observe-only mode: the statement describes what
    -- WOULD have happened. Sending that to a broadcaster as though it had happened
    -- would be a false statement of reasons, so the flag travels with it.
    actuated                BOOLEAN     NOT NULL,

    -- Whether the statement reached the broadcaster. CR-604 is about issuing, and a
    -- row nobody has read is not obviously issued; the delivery channel is not built,
    -- so this is FALSE and honest rather than absent and assumed.
    delivered_at            TIMESTAMPTZ,

    occurred_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS mm_demotion_events_broadcast_idx
    ON mm_demotion_events (broadcast_id, occurred_at DESC);

-- The undelivered queue, for whatever eventually delivers these.
CREATE INDEX IF NOT EXISTS mm_demotion_events_undelivered_idx
    ON mm_demotion_events (occurred_at)
    WHERE delivered_at IS NULL;
