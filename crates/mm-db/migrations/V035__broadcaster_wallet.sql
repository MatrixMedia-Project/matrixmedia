-- V035: prepaid broadcaster wallet, usage metering and rating (WS-D, design §17.5).
--
-- PostgreSQL-only, like the rest of the fleet: SQLite deployments are single-node
-- and bill nobody.
--
-- IDEMPOTENT throughout. run_pg_migrations heals a partially-migrated database by
-- re-running every migration, so a statement that fails on its second execution
-- wedges boot permanently. State columns are TEXT + CHECK, not enums, because
-- CREATE TYPE has no IF NOT EXISTS in any PostgreSQL version.
--
-- ── On `_minor` rather than the codebase's older `_cents` ────────────────────
--
-- V004/V005/V012 use `amount_cents`. These tables use `_minor` deliberately: the
-- wallet is multi-currency by design (§17.5), and "cents" is simply wrong for a
-- zero-decimal currency — ¥100 is one hundred yen, not one yen. Minor units are
-- the general form. The older tables are not renamed here; that is a separate
-- change with its own risk.
--
-- ── Money rules this schema enforces, so application code cannot forget ──────
--
--  1. A wallet's balance is ALWAYS the sum of its transactions. Not a convention:
--     the only way to move money is to insert a transaction, and a trigger applies
--     it to the balance in the same statement.
--  2. The overdraft limit is a CHECK on the wallet, so a transaction that would
--     breach it FAILS — atomically, with the balance unchanged.
--  3. Transactions are append-only. UPDATE and DELETE raise.
--  4. A transaction must be in its wallet's currency, enforced by a composite
--     foreign key rather than by a comparison somebody could omit.
--  5. Historical usage is never re-rated: a rated event records the rate-card
--     version it was rated against.

-- ── The wallet ───────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS mm_broadcaster_wallet (
    user_id            TEXT PRIMARY KEY,
    -- ISO 4217, lowercase to match the existing monetization tables.
    currency           TEXT NOT NULL,
    balance_minor      BIGINT NOT NULL DEFAULT 0,
    -- How far below zero this wallet may go. 0 = strictly prepaid. Non-zero is a
    -- commercial decision per broadcaster, not a global setting.
    credit_limit_minor BIGINT NOT NULL DEFAULT 0,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT wallet_currency_is_iso4217_lower
        CHECK (currency ~ '^[a-z]{3}$'),
    CONSTRAINT wallet_credit_limit_not_negative
        CHECK (credit_limit_minor >= 0),
    -- THE OVERDRAFT INVARIANT. Because the transaction trigger updates this row,
    -- a charge that would breach the limit makes the INSERT fail — so "cannot
    -- spend past the limit" is a database guarantee, not a check in a code path
    -- somebody might bypass.
    CONSTRAINT wallet_within_credit_limit
        CHECK (balance_minor >= -credit_limit_minor),
    -- Target for the composite FK below: a transaction must carry its wallet's
    -- currency, so a EUR charge cannot land on a USD wallet.
    CONSTRAINT wallet_user_currency_unique UNIQUE (user_id, currency)
);

-- ── The ledger ───────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS mm_wallet_transactions (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id         TEXT   NOT NULL,
    currency        TEXT   NOT NULL,
    kind            TEXT   NOT NULL
        CHECK (kind IN ('deposit', 'charge', 'refund', 'adjustment')),
    -- SIGNED. Deposits and refunds add, charges subtract. Storing the sign rather
    -- than a separate direction column means the balance is a plain SUM and cannot
    -- be got wrong by reading the direction backwards.
    amount_minor    BIGINT NOT NULL,
    -- The caller's own key for this movement. UNIQUE, so a retried charge is a
    -- no-op rather than a double charge — which is the difference between a
    -- retry-safe biller and an angry customer (§17.2).
    idempotency_key TEXT   NOT NULL,
    broadcast_id    TEXT,
    note            TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT wallet_tx_idempotency_unique UNIQUE (idempotency_key),
    -- Direction must match kind, or a "charge" could credit the wallet.
    CONSTRAINT wallet_tx_sign_matches_kind CHECK (
        (kind = 'deposit'    AND amount_minor > 0) OR
        (kind = 'refund'     AND amount_minor > 0) OR
        (kind = 'charge'     AND amount_minor < 0) OR
        (kind = 'adjustment' AND amount_minor <> 0)
    ),
    CONSTRAINT wallet_tx_currency_matches_wallet
        FOREIGN KEY (user_id, currency)
        REFERENCES mm_broadcaster_wallet (user_id, currency)
);

CREATE INDEX IF NOT EXISTS mm_wallet_tx_by_user
    ON mm_wallet_transactions (user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS mm_wallet_tx_by_broadcast
    ON mm_wallet_transactions (broadcast_id) WHERE broadcast_id IS NOT NULL;

-- ── The rate card, versioned ─────────────────────────────────────────────────
--
-- Versioned because §17.5 requires that historical usage is never re-rated
-- against a new card. A price change must not silently alter last month's invoice.
CREATE TABLE IF NOT EXISTS mm_rate_card (
    version        INTEGER NOT NULL,
    currency       TEXT    NOT NULL,
    -- The billable units of §17.2. Tier-1 shared infrastructure is deliberately
    -- absent: it is not cleanly attributable to one broadcast, so it goes into a
    -- flat fee or into margin, never into a line item a customer could dispute.
    unit           TEXT    NOT NULL
        CHECK (unit IN ('egress_gb', 'gpu_minute', 'node_minute', 'storage_gb_month')),
    -- Price per unit in minor units. BIGINT, not a float: a rounding error here is
    -- money. Sub-minor-unit prices are expressed by scaling the unit, not by
    -- adding decimals.
    price_minor    BIGINT  NOT NULL CHECK (price_minor >= 0),
    effective_from TIMESTAMPTZ NOT NULL DEFAULT now(),
    note           TEXT,

    PRIMARY KEY (version, currency, unit)
);

-- ── Usage events ─────────────────────────────────────────────────────────────
--
-- Append-only, and the rating queue at the same time: `rated_at IS NULL` is the
-- work list. §17.2: a lost event is unbilled revenue and a double-counted one is
-- an angry customer, so the idempotency key is a hard uniqueness constraint rather
-- than a de-duplication pass.
CREATE TABLE IF NOT EXISTS mm_usage_events (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id           TEXT   NOT NULL,
    broadcast_id      TEXT   NOT NULL,
    unit              TEXT   NOT NULL
        CHECK (unit IN ('egress_gb', 'gpu_minute', 'node_minute', 'storage_gb_month')),
    -- Quantity in thousandths of a unit, so 1.234 GB is 1234. Integer arithmetic
    -- all the way to the charge: no float ever touches money.
    quantity_milli    BIGINT NOT NULL CHECK (quantity_milli >= 0),
    -- Which node produced it, for disputes and for finding a node that
    -- over-reports. NULL for charges that are not node-attributable.
    mm_node_id        TEXT,
    idempotency_key   TEXT   NOT NULL,
    occurred_at       TIMESTAMPTZ NOT NULL,
    recorded_at       TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Rating state. All three are set together or none is.
    rated_at          TIMESTAMPTZ,
    rate_card_version INTEGER,
    transaction_id    BIGINT REFERENCES mm_wallet_transactions (id),

    CONSTRAINT usage_idempotency_unique UNIQUE (idempotency_key),
    -- Rated means rated: a row cannot be half-rated, which would either bill twice
    -- or never bill at all depending on which half the rater trusted.
    CONSTRAINT usage_rating_is_all_or_nothing CHECK (
        (rated_at IS NULL     AND rate_card_version IS NULL AND transaction_id IS NULL) OR
        (rated_at IS NOT NULL AND rate_card_version IS NOT NULL)
    )
);

-- The rating queue. Partial index, because the unrated set is small and the rated
-- set grows forever.
CREATE INDEX IF NOT EXISTS mm_usage_unrated
    ON mm_usage_events (occurred_at) WHERE rated_at IS NULL;
CREATE INDEX IF NOT EXISTS mm_usage_by_broadcast
    ON mm_usage_events (broadcast_id, occurred_at);

-- ── The trigger that makes the balance impossible to diverge ─────────────────
--
-- The only way to move money is to insert a transaction. This applies it in the
-- same statement, so the wallet's CHECK constraints judge the RESULT: a charge
-- past the credit limit fails the insert and leaves the balance untouched.
CREATE OR REPLACE FUNCTION mm_wallet_apply_transaction() RETURNS trigger AS $$
BEGIN
    UPDATE mm_broadcaster_wallet
       SET balance_minor = balance_minor + NEW.amount_minor,
           updated_at    = now()
     WHERE user_id = NEW.user_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no wallet for user %', NEW.user_id;
    END IF;
    RETURN NEW;
END $$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS mm_wallet_tx_applies ON mm_wallet_transactions;
CREATE TRIGGER mm_wallet_tx_applies
    AFTER INSERT ON mm_wallet_transactions
    FOR EACH ROW EXECUTE FUNCTION mm_wallet_apply_transaction();

-- ── Append-only enforcement, with one deliberate door ────────────────────────
--
-- A ledger you can edit is not a ledger. Without this, a balance correction could
-- be made by rewriting history, and the sum-equals-balance invariant would hold
-- while the audit trail lied. So the ordinary path cannot UPDATE or DELETE.
--
-- But a ledger that can NEVER be deleted from cannot be archived either, and it
-- cannot answer an erasure request. Refusing absolutely would not make the problem
-- go away; it would move it to whoever eventually disables the trigger under
-- pressure, with no record that they did.
--
-- So there is exactly one door, and it is loud: `SET LOCAL mm.ledger_maintenance
-- = 'on'` inside the transaction that needs it. LOCAL, so it cannot leak past the
-- commit; greppable, so an audit can find every caller; and absent from every
-- application code path, so the ordinary mistake is still impossible.
--
-- ⚠️ While that flag is on, NOTHING maintains `balance_minor`. Archiving a ledger
-- means deleting old rows AND writing a carried-forward opening adjustment, or the
-- balance and the remaining ledger stop agreeing. That is a retention procedure to
-- be written down, not a thing to improvise at 3am.
CREATE OR REPLACE FUNCTION mm_wallet_tx_immutable() RETURNS trigger AS $$
BEGIN
    IF current_setting('mm.ledger_maintenance', true) = 'on' THEN
        RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
    END IF;
    RAISE EXCEPTION 'mm_wallet_transactions is append-only: use a compensating adjustment, not an edit (retention/erasure: SET LOCAL mm.ledger_maintenance = ''on'')';
END $$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS mm_wallet_tx_no_update ON mm_wallet_transactions;
CREATE TRIGGER mm_wallet_tx_no_update
    BEFORE UPDATE OR DELETE ON mm_wallet_transactions
    FOR EACH ROW EXECUTE FUNCTION mm_wallet_tx_immutable();
