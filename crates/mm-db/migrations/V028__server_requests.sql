-- V028: server-request intake
-- Stores requests from dashboard users who want their own MatrixMedia server.
-- Admins review and update status; a webhook notification fires on creation.
CREATE TABLE IF NOT EXISTS mm_server_requests (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    org_name       TEXT        NOT NULL,
    contact_email  TEXT        NOT NULL,
    region         TEXT        NOT NULL,
    instance_size  TEXT        NOT NULL,
    domain         TEXT,
    notes          TEXT,
    status         TEXT        NOT NULL DEFAULT 'new',
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Index for admin list (newest first) and status-filtered list.
CREATE INDEX IF NOT EXISTS idx_mm_server_requests_status_created
    ON mm_server_requests (status, created_at DESC);
