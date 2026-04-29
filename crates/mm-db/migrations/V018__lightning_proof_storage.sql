-- V018 — Store BOLT11 + payment_hash on Lightning donations so the
-- preimage-proof verification endpoint can validate
-- `SHA256(preimage) == payment_hash` without trusting any client-supplied
-- BOLT11.
--
-- Both columns are nullable: existing rows (Stripe + pre-V018 Lightning
-- invoices) won't have them. The verification endpoint refuses any
-- /lightning-proof call against a row missing payment_hash.

ALTER TABLE mm_donations
    ADD COLUMN IF NOT EXISTS bolt11        TEXT,
    ADD COLUMN IF NOT EXISTS payment_hash  TEXT;

-- Partial index — only Lightning donations populate payment_hash, so the
-- index stays small and lookups (during proof verification) are O(1).
CREATE INDEX IF NOT EXISTS idx_donations_payment_hash
    ON mm_donations(payment_hash)
    WHERE payment_hash IS NOT NULL;
