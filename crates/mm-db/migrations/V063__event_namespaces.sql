-- V063: ADR-0011 event namespace (dev.matrixmedia.*).
--
-- While the legacy wire contract is active the server writes each stream state
-- event under two names. `state_event_id` and `ended_event_id` keep pointing at
-- the LEGACY events (comment threads are rooted there and the apps in the
-- stores find the root by the legacy type); the ids of the current-name events
-- go into the two new nullable columns, which stay NULL when that write
-- failed (the legacy write decides the outcome). For a stream started after
-- the contract is retired both pairs hold the current-name ids, so a stream
-- whose `state_event_id` differs from `current_state_event_id` (or has none)
-- is one that started with a legacy write; it keeps getting legacy writes
-- (resume, key, end) until it ends.
--
-- `mm_event_namespace_history` records every namespace the server has ever
-- written; the token-exchange announcement lists them all, so changing the
-- namespace setting never hides events already in rooms.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests).
ALTER TABLE mm_streams ADD COLUMN IF NOT EXISTS current_state_event_id TEXT;
ALTER TABLE mm_streams ADD COLUMN IF NOT EXISTS current_ended_event_id TEXT;

CREATE TABLE IF NOT EXISTS mm_event_namespace_history (
    namespace        TEXT        PRIMARY KEY,
    profile          TEXT        NOT NULL DEFAULT 'v1',
    first_written_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
