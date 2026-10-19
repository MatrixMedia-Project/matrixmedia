-- V047: per-user payments sandbox (mock provider allowlist).
--
-- Design (docs/2026-10-19-per-user-payments-mock.md): listed users' payment
-- flows run against the mock provider; everyone else uses the real provider
-- on the same server. The mock exercises the real entitlement path — only
-- money movement is simulated. Fail-closed: an absent flag row means REAL.
--
-- All statements are re-runnable (the runner re-applies migrations to heal
-- a partially migrated database).
CREATE TABLE IF NOT EXISTS mm_mock_users (
    user_id      TEXT PRIMARY KEY,
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    auto_settle  BOOLEAN NOT NULL DEFAULT FALSE,
    updated_by   TEXT,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Mock tagging at WRITE time (never presentation time): settlement, audit
-- and entitlement rows carry the marker from the insert on.
ALTER TABLE mm_donations ADD COLUMN IF NOT EXISTS is_mock BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE mm_subscriptions ADD COLUMN IF NOT EXISTS is_mock BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_donations_mock
    ON mm_donations(is_mock) WHERE is_mock;
CREATE INDEX IF NOT EXISTS idx_subscriptions_mock
    ON mm_subscriptions(is_mock) WHERE is_mock;
