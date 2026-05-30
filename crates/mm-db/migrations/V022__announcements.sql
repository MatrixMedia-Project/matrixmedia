-- V022: server-announcement banners
-- Operators publish server-wide banners; clients poll the active endpoint
-- (`GET /mm/v1/announcements/active`) every ~60s and surface the highest-
-- severity active row as a glass-strip banner above all screens.
CREATE TABLE IF NOT EXISTS mm_announcements (
    id           BIGSERIAL PRIMARY KEY,
    severity     TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    body         TEXT NOT NULL CHECK (length(body) <= 280),
    cta_label    TEXT,
    cta_url      TEXT,
    starts_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    dismissible  BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by   TEXT
);

-- Index supporting the active-banner query
-- (`WHERE starts_at <= now() AND expires_at > now() ORDER BY ... starts_at DESC`).
-- A true partial index keyed on `WHERE expires_at > now()` would be ideal,
-- but PostgreSQL only allows IMMUTABLE functions in index predicates and
-- `now()` is STABLE, so we keep this as a plain composite index. The
-- announcements table is small (operator-curated, typically <100 rows in
-- production), so the cost of scanning a few expired rows is negligible.
CREATE INDEX IF NOT EXISTS idx_mm_announcements_active
    ON mm_announcements (expires_at, starts_at);
