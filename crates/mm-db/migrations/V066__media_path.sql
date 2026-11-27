-- V066: media path states, module 1 (the relay tree).
--
-- `mm_streams.media_path` is the state a stream was fixed in when it started
-- ('core' until the stream-start code writes it). `mm_fleet_relay_feeds` is the
-- relay tree: per stream, which relay pulls from which, as last confirmed by the
-- relay's switch. `depth` is an upper bound on hops from the stream's entry (the
-- relay chain is at most 3 long). A NULL feeder at depth 1 pulls from the entry;
-- a NULL feeder at depth 3 is an adopted leaf: it carries the stream, where it
-- pulls from is unknown, and it never feeds another relay.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests).
ALTER TABLE mm_streams ADD COLUMN IF NOT EXISTS media_path TEXT NOT NULL DEFAULT 'core'
    CHECK (media_path IN ('core','relay_after_threshold','relay_entry'));

CREATE TABLE IF NOT EXISTS mm_fleet_relay_feeds (
    stream_id      TEXT NOT NULL,
    node_id        TEXT NOT NULL,
    feeder_node_id TEXT,                       -- NULL = fed by the stream's entry
    depth          SMALLINT NOT NULL CHECK (depth BETWEEN 1 AND 3),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (stream_id, node_id)
);
