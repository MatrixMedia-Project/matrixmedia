-- V004: Monetization - Donations (Phase 7a)
-- Applied to PostgreSQL only (not SQLite).

CREATE TABLE IF NOT EXISTS mm_creator_profiles (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id             TEXT NOT NULL UNIQUE,
    display_name        TEXT NOT NULL,
    stripe_account_id   TEXT,
    onboarding_complete BOOLEAN NOT NULL DEFAULT false,
    platform_fee_pct    NUMERIC(5,4) NOT NULL DEFAULT 0.10,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_creator_profiles_user_id ON mm_creator_profiles(user_id);

CREATE TABLE IF NOT EXISTS mm_donations (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stream_id                TEXT NOT NULL,
    donor_user_id            TEXT NOT NULL,
    recipient_user_id        TEXT NOT NULL,
    amount_cents             BIGINT NOT NULL CHECK (amount_cents >= 100),
    currency                 TEXT NOT NULL DEFAULT 'usd',
    message                  TEXT CHECK (length(message) <= 150),
    tier                     TEXT NOT NULL,
    pin_duration_secs        INT NOT NULL,
    stripe_session_id        TEXT,
    stripe_payment_intent_id TEXT,
    status                   TEXT NOT NULL DEFAULT 'pending'
                             CHECK (status IN ('pending','succeeded','failed','refunded')),
    idempotency_key          TEXT NOT NULL UNIQUE,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_donations_stream_id ON mm_donations(stream_id);
CREATE INDEX IF NOT EXISTS idx_donations_donor ON mm_donations(donor_user_id);
CREATE INDEX IF NOT EXISTS idx_donations_recipient ON mm_donations(recipient_user_id);
CREATE INDEX IF NOT EXISTS idx_donations_status ON mm_donations(status);
CREATE INDEX IF NOT EXISTS idx_donations_created_at ON mm_donations(created_at DESC);

CREATE TABLE IF NOT EXISTS mm_webhook_log (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stripe_event_id TEXT NOT NULL UNIQUE,
    event_type      TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_webhook_log_event_id ON mm_webhook_log(stripe_event_id);
