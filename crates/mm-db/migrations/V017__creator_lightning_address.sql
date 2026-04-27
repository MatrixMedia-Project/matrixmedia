-- V017 — Creator Lightning Address (M1 true-P2P pivot).
--
-- Lets a creator publish a Lightning Address (LUD-16: name@domain.tld) so the
-- LNURL-pay client in mm-payment can resolve it into a BOLT11 invoice on each
-- tip. With this column populated, donations bypass any operator-custodial
-- Lightning provider (LNBits) and settle wallet-to-wallet — the operator
-- never holds funds, which keeps MiCA out of scope.
--
-- Column is nullable: creators that do not provide a Lightning Address fall
-- back to whatever provider the operator has configured (LNBits if opted in,
-- Stripe otherwise).

ALTER TABLE mm_creator_profiles
    ADD COLUMN IF NOT EXISTS lightning_address TEXT;

-- Partial index so reverse lookups (e.g. "which creators publish this LN
-- address?") stay cheap without bloating the index for the common NULL case.
CREATE INDEX IF NOT EXISTS idx_creator_profiles_lightning_address
    ON mm_creator_profiles(lightning_address)
    WHERE lightning_address IS NOT NULL;
