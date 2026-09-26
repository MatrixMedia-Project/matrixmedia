//! PG-gated coverage for the prepaid wallet schema (WS-D, design §17.5).
//!
//! These test **money**, so the bar is different from the rest of the suite: every
//! invariant here is one the application must be unable to violate even by mistake,
//! because the failure is either unbilled revenue or a customer charged twice
//! (§17.2). Where an invariant can be pushed into the database it has been, and
//! these assert that it really is enforced there rather than by convention.

use std::sync::OnceLock;

use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_db::test_support::require_or_try_pool as try_pool;

async fn ensure_migrations(pool: &PgPool) {
    static MIGRATIONS: OnceLock<Mutex<bool>> = OnceLock::new();
    let cell = MIGRATIONS.get_or_init(|| Mutex::new(false));
    let mut applied = cell.lock().await;
    if !*applied {
        mm_db::run_pg_migrations(pool)
            .await
            .expect("migrations should apply cleanly");
        *applied = true;
    }
}

fn wallet_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// The ledger is append-only, so a test fixture has to use the same documented
/// maintenance door a retention job would — which is a small proof that the door
/// works and that nothing else can open it.
async fn wipe(pool: &PgPool) {
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL mm.ledger_maintenance = 'on'")
        .execute(&mut *tx)
        .await
        .expect("open the maintenance door");
    for t in [
        "mm_usage_events",
        "mm_wallet_transactions",
        "mm_rate_card",
        "mm_broadcaster_wallet",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("wipe {t}: {e}"));
    }
    tx.commit().await.expect("commit wipe");
}

async fn new_wallet(pool: &PgPool, user: &str, currency: &str, credit_limit: i64) {
    sqlx::query(
        "INSERT INTO mm_broadcaster_wallet (user_id, currency, credit_limit_minor)
         VALUES ($1, $2, $3)",
    )
    .bind(user)
    .bind(currency)
    .bind(credit_limit)
    .execute(pool)
    .await
    .expect("create wallet");
}

async fn tx(
    pool: &PgPool,
    user: &str,
    currency: &str,
    kind: &str,
    amount: i64,
    key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO mm_wallet_transactions
             (user_id, currency, kind, amount_minor, idempotency_key)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(user)
    .bind(currency)
    .bind(kind)
    .bind(amount)
    .bind(key)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn balance(pool: &PgPool, user: &str) -> i64 {
    sqlx::query_scalar("SELECT balance_minor FROM mm_broadcaster_wallet WHERE user_id = $1")
        .bind(user)
        .fetch_one(pool)
        .await
        .expect("read balance")
}

async fn ledger_sum(pool: &PgPool, user: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_minor), 0)::BIGINT FROM mm_wallet_transactions WHERE user_id = $1",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("sum ledger")
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!("MM_DATABASE_URL not set — skipping ", stringify!($name)));
                return;
            };
            let _guard = wallet_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

// ── The invariant everything else rests on ───────────────────────────────────

// THE ONE THAT MATTERS. The balance is not a cached number that application code
// keeps in step — it is maintained by the database in the same statement as the
// transaction, so it cannot drift from the ledger no matter what the application
// does or fails to do.
pg_test!(the_balance_is_always_the_sum_of_the_ledger, pool, {
    new_wallet(&pool, "@a:hs", "eur", 0).await;

    tx(&pool, "@a:hs", "eur", "deposit", 10_000, "d1").await.expect("deposit");
    assert_eq!(balance(&pool, "@a:hs").await, 10_000);

    tx(&pool, "@a:hs", "eur", "charge", -2_500, "c1").await.expect("charge");
    tx(&pool, "@a:hs", "eur", "charge", -1_000, "c2").await.expect("charge");
    tx(&pool, "@a:hs", "eur", "refund", 500, "r1").await.expect("refund");

    assert_eq!(balance(&pool, "@a:hs").await, 7_000);
    assert_eq!(
        balance(&pool, "@a:hs").await,
        ledger_sum(&pool, "@a:hs").await,
        "the balance and the ledger disagree — one of them is a lie about someone's money"
    );
});

// The overdraft limit is a CHECK on the wallet, and the trigger applies the
// transaction in the same statement — so a charge past the limit fails the INSERT
// and leaves the balance untouched. "Cannot spend past the limit" is a database
// guarantee, not a check in a code path somebody could bypass.
pg_test!(a_charge_past_the_credit_limit_fails_and_changes_nothing, pool, {
    new_wallet(&pool, "@b:hs", "eur", 0).await;
    tx(&pool, "@b:hs", "eur", "deposit", 1_000, "d1").await.expect("deposit");

    let err = tx(&pool, "@b:hs", "eur", "charge", -1_001, "c1")
        .await
        .expect_err("spending past a zero credit limit must fail");
    assert!(
        format!("{err}").contains("wallet_within_credit_limit"),
        "expected the credit-limit constraint to refuse it, got: {err}"
    );

    assert_eq!(balance(&pool, "@b:hs").await, 1_000, "the balance must be untouched");
    assert_eq!(ledger_sum(&pool, "@b:hs").await, 1_000, "and no ledger row written");
});

// A non-zero credit limit is a per-broadcaster commercial decision, and it must
// actually permit the overdraft it names — to exactly that point and no further.
pg_test!(a_credit_limit_permits_exactly_that_much_overdraft, pool, {
    new_wallet(&pool, "@c:hs", "eur", 500).await;

    tx(&pool, "@c:hs", "eur", "charge", -500, "c1")
        .await
        .expect("down to -500 is within a 500 limit");
    assert_eq!(balance(&pool, "@c:hs").await, -500);

    tx(&pool, "@c:hs", "eur", "charge", -1, "c2")
        .await
        .expect_err("one minor unit past the limit must fail");
    assert_eq!(balance(&pool, "@c:hs").await, -500);
});

// ── Idempotency: the difference between a retry and a double charge ───────────

// §17.2: a double-counted usage event is an angry customer. The idempotency key is
// a uniqueness constraint rather than a de-duplication pass, so a retried charge
// cannot succeed twice even if two workers race.
pg_test!(the_same_idempotency_key_cannot_charge_twice, pool, {
    new_wallet(&pool, "@d:hs", "eur", 0).await;
    tx(&pool, "@d:hs", "eur", "deposit", 10_000, "d1").await.expect("deposit");

    tx(&pool, "@d:hs", "eur", "charge", -1_000, "same-key").await.expect("first");
    let err = tx(&pool, "@d:hs", "eur", "charge", -1_000, "same-key")
        .await
        .expect_err("a replay must be refused");
    assert!(
        format!("{err}").contains("wallet_tx_idempotency_unique"),
        "got: {err}"
    );
    assert_eq!(
        balance(&pool, "@d:hs").await,
        9_000,
        "the retry must not have moved the balance a second time"
    );
});

// ── Structural correctness the application cannot get wrong ──────────────────

// Without this, a "charge" could credit the wallet — a sign error that reads as a
// gift and bills as nothing.
pg_test!(a_charge_must_be_negative_and_a_deposit_positive, pool, {
    new_wallet(&pool, "@e:hs", "eur", 0).await;

    assert!(
        tx(&pool, "@e:hs", "eur", "charge", 1_000, "wrong-sign-charge").await.is_err(),
        "a positive charge would credit the wallet"
    );
    assert!(
        tx(&pool, "@e:hs", "eur", "deposit", -1_000, "wrong-sign-deposit").await.is_err(),
        "a negative deposit would silently bill a customer for paying us"
    );
    // An adjustment may go either way — that is what makes it an adjustment — but
    // it may not be zero, which would be a ledger row that means nothing.
    //
    // A balance first: a negative adjustment against zero with a zero credit limit
    // is correctly refused by the overdraft constraint, which is what the first
    // draft of this test got wrong.
    tx(&pool, "@e:hs", "eur", "deposit", 1_000, "e-deposit").await.expect("deposit");
    tx(&pool, "@e:hs", "eur", "adjustment", -5, "adj-neg").await.expect("negative adjustment");
    tx(&pool, "@e:hs", "eur", "adjustment", 5, "adj-pos").await.expect("positive adjustment");
    assert!(
        tx(&pool, "@e:hs", "eur", "adjustment", 0, "adj-zero").await.is_err(),
        "a zero-value ledger row records nothing and hides the fact that nothing happened"
    );
});

// A EUR charge must not land on a USD wallet. Enforced by a composite foreign key
// rather than a comparison, because a comparison is something a new code path can
// forget and a foreign key is not.
pg_test!(a_transaction_cannot_be_in_a_different_currency_than_its_wallet, pool, {
    new_wallet(&pool, "@f:hs", "eur", 0).await;
    tx(&pool, "@f:hs", "eur", "deposit", 10_000, "d1").await.expect("deposit");

    let err = tx(&pool, "@f:hs", "usd", "charge", -1_000, "x-currency")
        .await
        .expect_err("cross-currency must be structurally impossible");
    assert!(
        format!("{err}").contains("wallet_tx_currency_matches_wallet"),
        "expected the composite FK to refuse it, got: {err}"
    );
});

// A ledger you can edit is not a ledger. Corrections are compensating entries, so
// the audit trail shows what happened AND what was done about it.
pg_test!(the_ledger_is_append_only, pool, {
    new_wallet(&pool, "@g:hs", "eur", 0).await;
    tx(&pool, "@g:hs", "eur", "deposit", 10_000, "d1").await.expect("deposit");

    let upd = sqlx::query("UPDATE mm_wallet_transactions SET amount_minor = 1 WHERE user_id = $1")
        .bind("@g:hs")
        .execute(&pool)
        .await;
    assert!(upd.is_err(), "editing a ledger row must raise");

    let del = sqlx::query("DELETE FROM mm_wallet_transactions WHERE user_id = $1")
        .bind("@g:hs")
        .execute(&pool)
        .await;
    assert!(del.is_err(), "deleting a ledger row must raise");

    assert_eq!(balance(&pool, "@g:hs").await, 10_000, "and nothing changed");
});

pg_test!(a_transaction_for_a_nonexistent_wallet_is_refused, pool, {
    // The FK catches it before the trigger does, but either way: no wallet, no
    // money movement, and no orphaned ledger row to reconcile later.
    assert!(
        tx(&pool, "@nobody:hs", "eur", "deposit", 100, "orphan").await.is_err(),
        "a ledger row with no wallet is money that belongs to no one"
    );
});

pg_test!(a_currency_must_be_three_lowercase_letters, pool, {
    for bad in ["EUR", "eu", "euro", "e1r", ""] {
        let r = sqlx::query(
            "INSERT INTO mm_broadcaster_wallet (user_id, currency) VALUES ($1, $2)",
        )
        .bind(format!("@cur-{bad}:hs"))
        .bind(bad)
        .execute(&pool)
        .await;
        assert!(r.is_err(), "currency {bad:?} must be refused");
    }
    new_wallet(&pool, "@ok:hs", "jpy", 0).await;
});

// ── Usage events and rating ──────────────────────────────────────────────────

async fn usage(
    pool: &PgPool,
    user: &str,
    broadcast: &str,
    unit: &str,
    qty_milli: i64,
    key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO mm_usage_events
             (user_id, broadcast_id, unit, quantity_milli, idempotency_key, occurred_at)
         VALUES ($1, $2, $3, $4, $5, now())",
    )
    .bind(user)
    .bind(broadcast)
    .bind(unit)
    .bind(qty_milli)
    .bind(key)
    .execute(pool)
    .await
    .map(|_| ())
}

// A lost event is unbilled revenue; a duplicate is an angry customer. The key is
// unique, so the metering side can retry freely — which it must, because §17.5
// requires events to survive node loss by buffering and re-sending.
pg_test!(a_usage_event_cannot_be_recorded_twice, pool, {
    new_wallet(&pool, "@h:hs", "eur", 0).await;
    usage(&pool, "@h:hs", "b1", "egress_gb", 1_234, "node1-b1-egress-0001")
        .await
        .expect("first");
    let err = usage(&pool, "@h:hs", "b1", "egress_gb", 1_234, "node1-b1-egress-0001")
        .await
        .expect_err("a replayed event must be refused");
    assert!(format!("{err}").contains("usage_idempotency_unique"), "got: {err}");
});

// Rating is all-or-nothing. A half-rated row would either bill twice or never
// bill, depending on which half the rater happened to trust.
pg_test!(a_usage_event_cannot_be_half_rated, pool, {
    new_wallet(&pool, "@i:hs", "eur", 0).await;
    usage(&pool, "@i:hs", "b1", "gpu_minute", 30_000, "u1").await.expect("event");

    let half = sqlx::query(
        "UPDATE mm_usage_events SET rated_at = now() WHERE idempotency_key = 'u1'",
    )
    .execute(&pool)
    .await;
    assert!(
        half.is_err(),
        "marking an event rated without recording WHICH rate card rated it makes the \
         charge unexplainable and unreproducible"
    );
});

// §17.5: historical usage is never re-rated against a new card, so a rated event
// records the version that rated it.
pg_test!(a_rated_event_records_the_rate_card_version_that_rated_it, pool, {
    new_wallet(&pool, "@j:hs", "eur", 0).await;
    tx(&pool, "@j:hs", "eur", "deposit", 100_000, "d1").await.expect("deposit");
    sqlx::query(
        "INSERT INTO mm_rate_card (version, currency, unit, price_minor)
         VALUES (1, 'eur', 'egress_gb', 9)",
    )
    .execute(&pool)
    .await
    .expect("rate card");
    usage(&pool, "@j:hs", "b1", "egress_gb", 2_000, "u1").await.expect("event");

    // 2.000 GB at 9 minor units per GB = 18.
    tx(&pool, "@j:hs", "eur", "charge", -18, "rate-u1").await.expect("charge");
    let txid: i64 = sqlx::query_scalar(
        "SELECT id FROM mm_wallet_transactions WHERE idempotency_key = 'rate-u1'",
    )
    .fetch_one(&pool)
    .await
    .expect("tx id");

    sqlx::query(
        "UPDATE mm_usage_events
            SET rated_at = now(), rate_card_version = 1, transaction_id = $1
          WHERE idempotency_key = 'u1'",
    )
    .bind(txid)
    .execute(&pool)
    .await
    .expect("a complete rating must be allowed");

    let (ver, tid): (Option<i32>, Option<i64>) = sqlx::query_as(
        "SELECT rate_card_version, transaction_id FROM mm_usage_events WHERE idempotency_key = 'u1'",
    )
    .fetch_one(&pool)
    .await
    .expect("read back");
    assert_eq!(ver, Some(1));
    assert_eq!(tid, Some(txid), "the charge must be traceable from the event");
});

// The rating queue. A partial index on `rated_at IS NULL` is the work list, and it
// must actually shrink as work is done or the rater re-reads the whole history
// every tick.
pg_test!(the_unrated_queue_shrinks_as_events_are_rated, pool, {
    new_wallet(&pool, "@k:hs", "eur", 0).await;
    for i in 0..3 {
        usage(&pool, "@k:hs", "b1", "node_minute", 60_000, &format!("u{i}"))
            .await
            .expect("event");
    }
    let unrated = |p: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM mm_usage_events WHERE rated_at IS NULL",
        )
        .fetch_one(&p)
        .await
        .expect("count")
    };
    assert_eq!(unrated(pool.clone()).await, 3);

    sqlx::query(
        "UPDATE mm_usage_events SET rated_at = now(), rate_card_version = 1
          WHERE idempotency_key = 'u0'",
    )
    .execute(&pool)
    .await
    .expect("rate one");
    assert_eq!(unrated(pool.clone()).await, 2);
});

// Tier-1 shared infrastructure is deliberately not a billable unit (§17.2): it is
// not cleanly attributable to one broadcast, so it goes into a flat fee or into
// margin, never a line item a customer could dispute.
pg_test!(shared_infrastructure_is_not_a_billable_unit, pool, {
    new_wallet(&pool, "@l:hs", "eur", 0).await;
    assert!(
        usage(&pool, "@l:hs", "b1", "tier1_share", 1, "u1").await.is_err(),
        "adding a unit for shared infrastructure would put an unattributable, \
         disputable line item on a customer's invoice"
    );
});

// ── The store over the schema (wallet_db.rs) ─────────────────────────────────

use mm_db::wallet_db::{treat_replay_as_success, ChargeOutcome, PgWalletDb, WalletError};

// A replayed charge is a SUCCESS, not a failure. This is the one thing the store
// adds over the schema, and it matters: a biller that treats its own retry as an
// error either stops retrying — losing revenue — or charges twice.
pg_test!(a_replayed_charge_reports_success_not_failure, pool, {
    let db = PgWalletDb::new(pool.clone());
    db.create_wallet("@m:hs", "eur", 0).await.expect("wallet");
    db.deposit("@m:hs", "eur", 10_000, "dep-1").await.expect("deposit");

    let first = db.charge("@m:hs", "eur", 1_000, "usage-42", Some("b1")).await;
    assert_eq!(treat_replay_as_success(first).expect("first"), ChargeOutcome::Applied);

    let second = db.charge("@m:hs", "eur", 1_000, "usage-42", Some("b1")).await;
    assert_eq!(
        treat_replay_as_success(second).expect("a replay is success"),
        ChargeOutcome::AlreadyApplied
    );

    assert_eq!(
        db.get("@m:hs").await.expect("get").expect("wallet").balance_minor,
        9_000,
        "the retry must not have moved the balance twice"
    );
});

// Insufficient funds is not an error in the system — it is the answer to "can this
// broadcast afford to continue", and the demotion ladder acts on it. So it has its
// own variant rather than arriving as an opaque database string.
pg_test!(insufficient_funds_is_a_distinct_actionable_outcome, pool, {
    let db = PgWalletDb::new(pool.clone());
    db.create_wallet("@n:hs", "eur", 0).await.expect("wallet");
    db.deposit("@n:hs", "eur", 500, "dep-1").await.expect("deposit");

    let err = db
        .charge("@n:hs", "eur", 501, "too-much", None)
        .await
        .expect_err("must refuse");
    assert!(
        matches!(err, WalletError::InsufficientFunds { .. }),
        "the ladder needs to tell 'no money' from 'database down', got: {err}"
    );
});

// `spendable_minor`, not `balance_minor`, is what the gate and the ladder compare
// against: a wallet at zero with a credit limit can still pay.
pg_test!(spendable_accounts_for_the_credit_limit, pool, {
    let db = PgWalletDb::new(pool.clone());
    db.create_wallet("@o:hs", "eur", 2_000).await.expect("wallet");
    let w = db.get("@o:hs").await.expect("get").expect("wallet");
    assert_eq!(w.balance_minor, 0);
    assert_eq!(
        w.spendable_minor(),
        2_000,
        "a zero balance with a 2000 credit limit can still pay for 2000"
    );

    db.charge("@o:hs", "eur", 2_000, "c1", None).await.expect("within limit");
    let w = db.get("@o:hs").await.expect("get").expect("wallet");
    assert_eq!(w.balance_minor, -2_000);
    assert_eq!(w.spendable_minor(), 0, "and now it cannot");
});

// A caller passing a negative "charge" is a bug, and letting it through would
// credit the wallet. Refused before the database sees it, so the error names the
// caller's mistake rather than a constraint.
pg_test!(a_negative_charge_is_refused_as_a_caller_bug, pool, {
    let db = PgWalletDb::new(pool.clone());
    db.create_wallet("@p:hs", "eur", 0).await.expect("wallet");
    assert!(db.charge("@p:hs", "eur", -100, "neg", None).await.is_err());
    assert!(db.charge("@p:hs", "eur", 0, "zero", None).await.is_err());
});

// A currency mismatch is always a bug — the rate card and the wallet must agree
// before a charge is attempted — so the message names both currencies rather than
// sending someone to the database to find out which is which.
pg_test!(a_currency_mismatch_names_both_currencies, pool, {
    let db = PgWalletDb::new(pool.clone());
    db.create_wallet("@q:hs", "eur", 0).await.expect("wallet");
    db.deposit("@q:hs", "eur", 1_000, "dep").await.expect("deposit");

    let err = db
        .charge("@q:hs", "usd", 100, "x", None)
        .await
        .expect_err("cross-currency must fail");
    let msg = err.to_string();
    assert!(matches!(err, WalletError::CurrencyMismatch { .. }), "got {msg}");
    assert!(msg.contains("usd") && msg.contains("eur"), "got {msg}");
});

pg_test!(charging_a_wallet_that_does_not_exist_says_so, pool, {
    let db = PgWalletDb::new(pool.clone());
    let err = db
        .charge("@nobody:hs", "eur", 100, "k", None)
        .await
        .expect_err("must fail");
    assert!(matches!(err, WalletError::NoWallet { .. }), "got {err}");
});

pg_test!(creating_a_wallet_twice_is_idempotent, pool, {
    let db = PgWalletDb::new(pool.clone());
    db.create_wallet("@r:hs", "eur", 0).await.expect("first");
    db.deposit("@r:hs", "eur", 5_000, "dep").await.expect("deposit");
    // A second create must not reset the balance — an ON CONFLICT DO UPDATE here
    // would silently zero a wallet on a retried signup.
    db.create_wallet("@r:hs", "eur", 999).await.expect("second");
    let w = db.get("@r:hs").await.expect("get").expect("wallet");
    assert_eq!(w.balance_minor, 5_000, "a repeated create must not touch the money");
    assert_eq!(w.credit_limit_minor, 0, "nor silently change the credit limit");
});
