-- V021: signups audit trail
-- Tracks every successful mm-core-proxied signup for compliance + abuse forensics.
-- ip_hash is a SHA256 of (raw_ip + per-instance pepper); the raw IP is never persisted.
CREATE TABLE IF NOT EXISTS mm_signups (
    id           BIGSERIAL PRIMARY KEY,
    username     TEXT      NOT NULL,
    mxid         TEXT      NOT NULL,
    tos_version  TEXT      NOT NULL,
    ip_hash      BYTEA     NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_mm_signups_mxid ON mm_signups (mxid);
CREATE INDEX IF NOT EXISTS idx_mm_signups_created_at ON mm_signups (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_mm_signups_ip_hash_created_at ON mm_signups (ip_hash, created_at);
