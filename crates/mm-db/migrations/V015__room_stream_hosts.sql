-- V015: Per-room permissions for who can host a stream
--
-- Default mode is 'open' (back-compat: anyone in the room can host). The room
-- owner can switch to 'restricted' and maintain an allowlist.

CREATE TABLE IF NOT EXISTS mm_room_stream_hosts (
    matrix_room_id    TEXT PRIMARY KEY,
    mode              TEXT NOT NULL DEFAULT 'open'
                      CHECK (mode IN ('open', 'restricted')),
    owner_user_id     TEXT,
    allowed_user_ids  TEXT[] NOT NULL DEFAULT '{}',
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
