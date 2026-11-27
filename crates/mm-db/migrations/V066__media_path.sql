-- V066: media path states, module 1 (the relay tree).
--
-- `mm_streams.media_path` is the state a stream was fixed in when it started
-- ('core' until the stream-start code writes it). `mm_fleet_relay_feeds` is the
-- relay tree: per stream, which relay is fed by which. A NULL feeder means the
-- relay pulls the stream from the stream's entry; `depth` is hops from the
-- entry (the relay chain is at most 3 long).
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
