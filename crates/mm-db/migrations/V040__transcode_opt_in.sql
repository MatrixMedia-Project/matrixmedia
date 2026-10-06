-- V040: Transcode opt-in (FR-314a/b/c; ops-page design §14, owner decisions D12/D13).
--
-- The GPU ladder is ~88% of a small broadcast's bill and it spends the
-- broadcaster's wallet, so whether a broadcast gets one is the broadcaster's
-- stored choice, never inferred. Before this migration the fleet inferred it from
-- "the wallet has spendable balance", which would have given every promoted,
-- funded broadcast a GPU the moment the runner was wired.
--
-- Three facts:
--   1. a broadcaster DEFAULT, next to the other per-creator defaults (V013/V014);
--   2. a per-broadcast OVERRIDE: inherit the default, or force on / off;
--   3. a sticky RELEASED flag (FR-314c): an operator released this broadcast's
--      transcoder. While it is set the planner provisions no transcoder for the
--      broadcast; only the broadcaster explicitly opting this broadcast in again
--      ('on') clears it. Changing the broadcaster DEFAULT does not — that is not a
--      decision about this broadcast.
--
-- "Paying broadcasters only" is deliberately NOT stored here. It is a separate
-- condition the planner ANDs with this choice (FR-314a): a stored choice says what
-- the broadcaster wants, not what they can pay for.
--
-- TEXT + CHECK rather than an enum type: CREATE TYPE has no IF NOT EXISTS. The
-- string forms MUST match mm_core::fleet::transcode::TranscodeOverride — the
-- ddl_agreement test there reads this file.
--
-- Idempotent: ADD COLUMN IF NOT EXISTS skips the whole column clause, constraint
-- included, when the column already exists. `transcode_released` is created LAST
-- because the adoption probe in run_pg_migrations keys on it.

ALTER TABLE mm_creator_defaults
    ADD COLUMN IF NOT EXISTS transcode_opt_in_default BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE mm_streams
    ADD COLUMN IF NOT EXISTS transcode_opt_in TEXT NOT NULL DEFAULT 'inherit'
        CONSTRAINT streams_transcode_opt_in_valid
        CHECK (transcode_opt_in IN ('inherit', 'on', 'off'));

ALTER TABLE mm_streams
    ADD COLUMN IF NOT EXISTS transcode_released BOOLEAN NOT NULL DEFAULT false;
