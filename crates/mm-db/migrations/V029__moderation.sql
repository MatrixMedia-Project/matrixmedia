-- V029: content moderation (E3)
-- Unified report queue (Matrix-synced + MM-native), append-only action audit,
-- MM-side user suspension flag, and a reversible hide flag on recordings.

CREATE TABLE IF NOT EXISTS mm_moderation_reports (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source             TEXT        NOT NULL,           -- 'matrix' | 'mm'
    target_type        TEXT        NOT NULL,           -- event|room|user|stream|recording
    target_id          TEXT        NOT NULL,
    room_id            TEXT,
    reported_user_id   TEXT,
    reporter_id        TEXT,
    reason             TEXT        NOT NULL,
    details            TEXT,
    status             TEXT        NOT NULL DEFAULT 'open',  -- open|actioned|dismissed
    synapse_report_id  BIGINT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_by        TEXT,
    resolved_at        TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_mod_reports_synapse
    ON mm_moderation_reports (source, synapse_report_id)
    WHERE synapse_report_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_mod_reports_status   ON mm_moderation_reports (status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_mod_reports_target   ON mm_moderation_reports (target_type, target_id);
CREATE INDEX IF NOT EXISTS idx_mod_reports_subject  ON mm_moderation_reports (reported_user_id);

CREATE TABLE IF NOT EXISTS mm_moderation_actions (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    report_id    UUID,
    action_type  TEXT        NOT NULL,
    target_type  TEXT        NOT NULL,
    target_id    TEXT        NOT NULL,
    operator_id  TEXT        NOT NULL,
    reason       TEXT        NOT NULL,
    metadata     JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_mod_actions_target ON mm_moderation_actions (target_type, target_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_mod_actions_time   ON mm_moderation_actions (created_at DESC);

CREATE TABLE IF NOT EXISTS mm_user_moderation (
    user_id       TEXT PRIMARY KEY,
    suspended     BOOLEAN     NOT NULL DEFAULT false,
    suspended_at  TIMESTAMPTZ,
    suspended_by  TEXT,
    reason        TEXT
);

ALTER TABLE mm_recordings ADD COLUMN IF NOT EXISTS hidden     BOOLEAN     NOT NULL DEFAULT false;
ALTER TABLE mm_recordings ADD COLUMN IF NOT EXISTS hidden_at  TIMESTAMPTZ;
