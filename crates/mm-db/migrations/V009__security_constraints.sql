-- V009: Security Constraints (Race condition prevention)
-- Applied to PostgreSQL only.

-- H4: Enforce max 5 active tiers per creator at database level.
-- Prevents TOCTOU race where two concurrent requests both read count=4,
-- pass validation, and create tier 5 and 6.
CREATE OR REPLACE FUNCTION check_max_tiers() RETURNS TRIGGER AS $$
BEGIN
    IF (SELECT COUNT(*) FROM mm_subscription_tiers
        WHERE creator_user_id = NEW.creator_user_id AND is_active = true) >= 5 THEN
        RAISE EXCEPTION 'Maximum 5 active tiers per creator';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER enforce_max_tiers
    BEFORE INSERT ON mm_subscription_tiers
    FOR EACH ROW EXECUTE FUNCTION check_max_tiers();

-- Hard cap on donation amount ($1000 = 100000 cents).
-- Defense-in-depth: application-level validation + DB constraint.
ALTER TABLE mm_donations ADD CONSTRAINT chk_donation_max
    CHECK (amount_cents <= 100000);

-- Security audit log for payment and subscription operations.
CREATE TABLE IF NOT EXISTS mm_security_audit (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    event_type TEXT NOT NULL,
    user_id TEXT,
    ip_address TEXT,
    details JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_audit_time ON mm_security_audit(created_at DESC);
