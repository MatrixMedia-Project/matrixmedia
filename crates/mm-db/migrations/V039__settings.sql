-- V039: dashboard-managed settings (WorkingDirectory spec 2026-09-27 §5.1).
--
-- Numbered V039 because V034..V038 belong to the broadcast-fleet branch.
-- Idempotent on purpose: the runner re-runs migrations to heal a partially
-- migrated database. TEXT + CHECK rather than enum types for the same reason.

CREATE SEQUENCE IF NOT EXISTS mm_settings_rev_seq;

CREATE TABLE IF NOT EXISTS mm_settings (
    key         TEXT        PRIMARY KEY,
    value_json  JSONB       NULL,     -- non-secret value
    value_enc   BYTEA       NULL,     -- secret value, AES-256-GCM envelope
    rev         BIGINT      NOT NULL, -- from mm_settings_rev_seq
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by  TEXT        NOT NULL,
    CONSTRAINT mm_settings_one_value CHECK ((value_json IS NULL) <> (value_enc IS NULL))
);

CREATE TABLE IF NOT EXISTS mm_settings_audit (
    id              BIGSERIAL   PRIMARY KEY,
    key             TEXT        NOT NULL,
    action          TEXT        NOT NULL
                    CHECK (action IN ('import', 'set', 'restart_requested', 'reencrypt')),
    old_json        JSONB       NULL,  -- NULL for secrets
    new_json        JSONB       NULL,  -- NULL for secrets
    secret_changed  BOOLEAN     NOT NULL DEFAULT false,
    actor           TEXT        NOT NULL,
    rev             BIGINT      NOT NULL,
    at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS mm_settings_audit_key_id ON mm_settings_audit (key, id DESC);

CREATE TABLE IF NOT EXISTS mm_settings_meta (
    id                    SMALLINT    PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    imported_at           TIMESTAMPTZ NULL,
    restart_requested_rev BIGINT      NOT NULL DEFAULT 0
);

INSERT INTO mm_settings_meta (id) VALUES (1) ON CONFLICT (id) DO NOTHING;
