-- V065: go-live follow rooms (Twitch parity #3, Matrix-path design).
--
-- One hidden personal Matrix room per (follower, creator) pair. The user
-- never sees the room: follow = bot invites the follower into it, unfollow
-- = the bot kicks them (leave), and the streamer (via the bot) can remove
-- or block a follower. Only the bot posts (feed.* events); followers are
-- read-only by power level. Tiered updates are delivered by writing only
-- into qualifying followers' rooms.
--
-- `mm_creator_blocks` is the streamer's block list: a blocked user cannot
-- follow (checked at follow time) and receives no updates.
--
-- `mm_creator_notify_prefs` holds streamer-level notification preferences.
-- `history_visible` (default TRUE) maps to the room creation-time
-- m.room.history_visibility: shared (past notices readable) vs joined.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout. The
-- marker table is the LAST statement on purpose (adoption probe keys on
-- it, see mm-db/src/lib.rs).
CREATE TABLE IF NOT EXISTS mm_follow_rooms (
    follower_user_id TEXT NOT NULL,
    creator_user_id  TEXT NOT NULL,
    room_id          TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (follower_user_id, creator_user_id)
);

CREATE INDEX IF NOT EXISTS idx_mm_follow_rooms_creator ON mm_follow_rooms (creator_user_id);

CREATE TABLE IF NOT EXISTS mm_creator_blocks (
    creator_user_id TEXT NOT NULL,
    blocked_user_id TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (creator_user_id, blocked_user_id)
);

CREATE TABLE IF NOT EXISTS mm_creator_notify_prefs (
    creator_user_id TEXT PRIMARY KEY,
    history_visible BOOLEAN NOT NULL DEFAULT TRUE,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS mm_follow_rooms_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
