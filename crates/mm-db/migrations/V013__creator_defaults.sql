-- V013: Per-creator default settings
--
-- Used as fallback when a stream/recording is created without an explicit
-- min_tier. tier_level 0 means "open / no subscription required".

CREATE TABLE IF NOT EXISTS mm_creator_defaults (
    creator_user_id              TEXT PRIMARY KEY,
    default_stream_min_tier      INT NOT NULL DEFAULT 0
                                 CHECK (default_stream_min_tier >= 0
                                        AND default_stream_min_tier <= 5),
    default_recording_min_tier   INT NOT NULL DEFAULT 0
                                 CHECK (default_recording_min_tier >= 0
                                        AND default_recording_min_tier <= 5),
    created_at                   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                   TIMESTAMPTZ NOT NULL DEFAULT now()
);
