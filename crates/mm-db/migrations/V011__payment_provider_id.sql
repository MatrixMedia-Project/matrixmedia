-- Add generic payment provider ID column for non-Stripe providers (LNBits, etc.)
ALTER TABLE mm_donations ADD COLUMN IF NOT EXISTS provider_payment_id TEXT;
CREATE INDEX IF NOT EXISTS idx_donations_provider_payment_id ON mm_donations(provider_payment_id);
