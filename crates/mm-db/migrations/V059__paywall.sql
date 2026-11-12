-- V059: Lightning anonymous paywall — per-target price + paywall claims.
--
-- `paywall_price_sats` on `mm_streams` / `mm_recordings` is the owner's
-- anonymous-paywall price for share-link viewers: NULL (default; the
-- pre-V059 behavior — no price set, nothing is paywalled by applying this
-- migration) or a positive amount in sats. The CHECK rejects zero and
-- negative prices at the schema level.
--
-- `mm_paywall_claims` is one row per paywall payment attempt against a
-- share grant: `share_grant_id` is the `mm_share_grants` id the viewer
-- presented, `kind` is 'stream' | 'recording', `target_id` is the
-- mm_streams/mm_recordings id (TEXT, no FK — same rationale as
-- `mm_share_grants`). `recipient_user_id` is the Matrix user id that
-- gets paid, `amount_sats` the invoiced amount, `bolt11` the invoice
-- handed to the viewer, and `payment_hash` its hex payment hash
-- (UNIQUE — one claim per invoice; settlement keys on it). `status`
-- moves 'pending' -> 'succeeded' (settled) or 'expired' (swept past
-- `expires_at`); on success `access_token_hash` stores ONLY the SHA-256
-- hex of the bearer token handed to the viewer (the token itself is
-- never persisted) and `paid_at` is stamped. `is_mock` marks claims
-- settled through the mock payment provider.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests). V059 creates the `mm_paywall_migrated`
-- marker table in its LAST statement, so a V059 that died partway —
-- marker table not yet created — is not adopted.
ALTER TABLE mm_streams
    ADD COLUMN IF NOT EXISTS paywall_price_sats BIGINT
        CONSTRAINT streams_paywall_price_positive
        CHECK (paywall_price_sats > 0);

ALTER TABLE mm_recordings
    ADD COLUMN IF NOT EXISTS paywall_price_sats BIGINT
        CONSTRAINT recordings_paywall_price_positive
        CHECK (paywall_price_sats > 0);

CREATE TABLE IF NOT EXISTS mm_paywall_claims (
    id                 UUID PRIMARY KEY,
    share_grant_id     UUID NOT NULL,
    kind               TEXT NOT NULL
        CONSTRAINT paywall_claims_kind_valid
        CHECK (kind IN ('stream', 'recording')),
    target_id          TEXT NOT NULL,
    recipient_user_id  TEXT NOT NULL,
    amount_sats        BIGINT NOT NULL,
    bolt11             TEXT NOT NULL,
    payment_hash       TEXT NOT NULL UNIQUE,
    status             TEXT NOT NULL DEFAULT 'pending'
        CONSTRAINT paywall_claims_status_valid
        CHECK (status IN ('pending', 'succeeded', 'expired')),
    access_token_hash  TEXT,
    expires_at         TIMESTAMPTZ NOT NULL,
    paid_at            TIMESTAMPTZ,
    is_mock            BOOLEAN NOT NULL DEFAULT false,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_paywall_claims_grant_target
    ON mm_paywall_claims(share_grant_id, kind, target_id);

-- The expiry sweep only touches pending rows.
CREATE INDEX IF NOT EXISTS idx_paywall_claims_pending_expires
    ON mm_paywall_claims(expires_at)
    WHERE status = 'pending';

CREATE TABLE IF NOT EXISTS mm_paywall_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
