-- Per-room MatrixMedia opt-out toggle.
--
-- Default = enabled (no row → mm_enabled=true). The channel admin can flip
-- it to false from Channel Settings to hide all MM features (Tip/Subscribe
-- /LIVE/Lightning) for every viewer of that room. Stored server-side so
-- the choice propagates across clients/devices.
--
-- The Matrix room id is opaque text (`!opaque:server`) — keep TEXT PK.

CREATE TABLE IF NOT EXISTS mm_room_config (
    matrix_room_id  TEXT        PRIMARY KEY,
    mm_enabled      BOOLEAN     NOT NULL DEFAULT TRUE,
    updated_by      TEXT        NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
