//! Rating, against a real database (WS-D, design §17.5).
//!
//! The arithmetic is unit-tested in the module. What needs a database is the part
//! that decides whether anyone is charged correctly **once**: that a re-run charges
//! nothing twice, that an empty wallet leaves the debt standing rather than forgiving
//! it, and that a charge which landed without its mark is recovered rather than
//! stuck.

use std::sync::OnceLock;

use chrono::Utc;
use mm_db::metering_db::PgMeteringDb;
use mm_db::wallet_db::PgWalletDb;
use mm_fleet::rating::{charge_idempotency_key, rate_pending};
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

fn rating_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn wipe(pool: &PgPool) {
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL mm.ledger_maintenance = 'on'")
        .execute(&mut *tx)
        .await
        .expect("maintenance door");
    for t in [
        "mm_egress_baseline",
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
    tx.commit().await.expect("commit");
}

async fn wallet_with(pool: &PgPool, user: &str, deposit: i64) {
    let db = PgWalletDb::new(pool.clone());
    db.create_wallet(user, "eur", 0).await.expect("wallet");
    if deposit > 0 {
        db.deposit(user, "eur", deposit, &format!("dep-{user}"))
            .await
            .expect("deposit");
    }
}

async fn rate_card(pool: &PgPool, version: i32, unit: &str, price: i64) {
    sqlx::query(
        "INSERT INTO mm_rate_card (version, currency, unit, price_minor)
         VALUES ($1, 'eur', $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(version)
    .bind(unit)
    .bind(price)
    .execute(pool)
    .await
    .expect("rate card");
}

async fn usage(pool: &PgPool, user: &str, broadcast: &str, milli: i64, key: &str) {
    sqlx::query(
        "INSERT INTO mm_usage_events
             (user_id, broadcast_id, unit, quantity_milli, idempotency_key, occurred_at)
         VALUES ($1, $2, 'egress_gb', $3, $4, now())",
    )
    .bind(user)
    .bind(broadcast)
    .bind(milli)
    .bind(key)
    .execute(pool)
    .await
    .expect("usage");
}

async fn balance(pool: &PgPool, user: &str) -> i64 {
    PgWalletDb::new(pool.clone())
        .get(user)
        .await
        .expect("get")
        .expect("wallet")
        .balance_minor
}

async fn unrated_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mm_usage_events WHERE rated_at IS NULL")
        .fetch_one(pool)
        .await
        .expect("count")
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!("MM_DATABASE_URL not set — skipping ", stringify!($name)));
                return;
            };
            let _guard = rating_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

// ── The ordinary case ────────────────────────────────────────────────────────

pg_test!(a_pending_event_is_priced_charged_and_marked, pool, {
    wallet_with(&pool, "@a:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage(&pool, "@a:hs", "b1", 2_500, "u1").await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 1);
    assert_eq!(r.charged_minor, 23, "2.5 GB at 9 rounds half-up to 23");
    assert_eq!(balance(&pool, "@a:hs").await, 100_000 - 23);
    assert_eq!(unrated_count(&pool).await, 0, "the queue must drain");

    // And the event records which card priced it, so history is not re-priceable.
    let (ver, tx): (Option<i32>, Option<i64>) = sqlx::query_as(
        "SELECT rate_card_version, transaction_id FROM mm_usage_events WHERE idempotency_key = 'u1'",
    )
    .fetch_one(&pool)
    .await
    .expect("read back");
    assert_eq!(ver, Some(1));
    assert!(tx.is_some(), "the charge must be traceable from the event");
});

// ── Idempotency ──────────────────────────────────────────────────────────────

// A second pass must charge nothing: the queue is empty, and even if the same event
// were re-queued the charge key would collide.
pg_test!(a_second_pass_charges_nothing, pool, {
    wallet_with(&pool, "@b:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage(&pool, "@b:hs", "b1", 1_000, "u1").await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("first");
    let after_first = balance(&pool, "@b:hs").await;

    let second = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("second");
    assert_eq!(second.rated, 0);
    assert_eq!(second.charged_minor, 0);
    assert_eq!(balance(&pool, "@b:hs").await, after_first);
});

// THE RECOVERY PATH. If a charge lands and the mark does not — a crash between two
// statements — the next pass must recover the transaction id and mark it, not leave
// the event unrated with its money already gone.
pg_test!(a_charge_that_landed_without_its_mark_is_recovered, pool, {
    wallet_with(&pool, "@c:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage(&pool, "@c:hs", "b1", 1_000, "u1").await;

    // Simulate the crash: charge with the key the rater would use, and do not mark.
    let wallet = PgWalletDb::new(pool.clone());
    wallet
        .charge("@c:hs", "eur", 9, &charge_idempotency_key("u1"), Some("b1"))
        .await
        .expect("pre-charge");
    let after_charge = balance(&pool, "@c:hs").await;
    assert_eq!(unrated_count(&pool).await, 1, "still unrated");

    let meter = PgMeteringDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.recovered, 1, "the replay must be recognised, not re-charged");
    assert_eq!(r.rated, 1);
    assert_eq!(
        balance(&pool, "@c:hs").await,
        after_charge,
        "the money must not move a second time"
    );
    assert_eq!(unrated_count(&pool).await, 0, "and the event must leave the queue");
});

// ── An empty wallet ──────────────────────────────────────────────────────────

// Left UNRATED on purpose. The debt is real, the demotion ladder is what acts on an
// empty wallet, and marking it rated would forgive the charge.
pg_test!(an_unaffordable_charge_leaves_the_debt_standing, pool, {
    wallet_with(&pool, "@d:hs", 5).await; // 5 minor units
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage(&pool, "@d:hs", "b1", 10_000, "u1").await; // 10 GB = 90

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 0);
    assert_eq!(r.insufficient_funds, vec!["b1"]);
    assert_eq!(balance(&pool, "@d:hs").await, 5, "nothing charged");
    assert_eq!(
        unrated_count(&pool).await,
        1,
        "the event must STAY in the queue: marking it rated would forgive a real debt"
    );
});

// And it rates once the wallet is topped up — which is the point of leaving it.
pg_test!(a_topped_up_wallet_rates_the_event_it_could_not_afford, pool, {
    wallet_with(&pool, "@e:hs", 5).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage(&pool, "@e:hs", "b1", 10_000, "u1").await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("first");

    wallet.deposit("@e:hs", "eur", 1_000, "top-up").await.expect("top up");
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("second");

    assert_eq!(r.rated, 1);
    assert_eq!(r.charged_minor, 90);
    assert_eq!(unrated_count(&pool).await, 0);
});

// One broadcaster's empty wallet must not stop another's usage being billed.
pg_test!(one_empty_wallet_does_not_block_the_queue, pool, {
    wallet_with(&pool, "@poor:hs", 1).await;
    wallet_with(&pool, "@rich:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage(&pool, "@poor:hs", "b-poor", 10_000, "u-poor").await;
    usage(&pool, "@rich:hs", "b-rich", 1_000, "u-rich").await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 1);
    assert_eq!(r.insufficient_funds, vec!["b-poor"]);
    assert_eq!(balance(&pool, "@rich:hs").await, 100_000 - 9);
});

// ── Things that cannot be priced ─────────────────────────────────────────────

// No card means no price, and a price of zero would silently forgive the charge.
pg_test!(usage_with_no_rate_card_is_reported_and_left_unrated, pool, {
    wallet_with(&pool, "@f:hs", 100_000).await;
    usage(&pool, "@f:hs", "b1", 1_000, "u1").await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 0);
    assert_eq!(r.unpriceable.len(), 1);
    assert!(r.unpriceable[0].1.contains("no rate card"), "{:?}", r.unpriceable);
    assert_eq!(unrated_count(&pool).await, 1, "it must bill once a card exists");
});

pg_test!(usage_for_a_unit_the_card_does_not_price_is_reported, pool, {
    wallet_with(&pool, "@g:hs", 100_000).await;
    // A card exists, but only prices GPU minutes.
    rate_card(&pool, 1, "gpu_minute", 4).await;
    usage(&pool, "@g:hs", "b1", 1_000, "u1").await; // egress_gb

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 0);
    assert!(r.unpriceable[0].1.contains("no price for unit"), "{:?}", r.unpriceable);
});

// The main query joins the wallet, so usage belonging to a user without one would sit
// in the queue invisibly. It is asked about separately.
pg_test!(usage_from_a_user_with_no_wallet_is_surfaced, pool, {
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage(&pool, "@nowallet:hs", "b1", 1_000, "u1").await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 0);
    assert_eq!(
        r.without_wallet,
        vec!["@nowallet:hs"],
        "a join that silently skips usage is unbilled revenue no counter shows"
    );
});

// ── Zero-priced usage must still leave the queue ──────────────────────────────

// Otherwise every pass re-reads it forever and the queue never drains.
pg_test!(usage_priced_at_zero_leaves_the_queue, pool, {
    wallet_with(&pool, "@h:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    // 0.001 GB at 9 = 0.009 minor units -> 0.
    usage(&pool, "@h:hs", "b1", 1, "u1").await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 1);
    assert_eq!(r.charged_minor, 0);
    assert_eq!(balance(&pool, "@h:hs").await, 100_000, "no money moved");
    assert_eq!(
        unrated_count(&pool).await,
        0,
        "a zero-priced event must leave the queue or every pass re-reads it forever"
    );
});

// ── History is not re-priced ─────────────────────────────────────────────────

// §17.5: a new rate card must not alter what was already billed. The rated event
// keeps the version that priced it.
pg_test!(a_new_rate_card_does_not_reprice_history, pool, {
    wallet_with(&pool, "@i:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage(&pool, "@i:hs", "b1", 1_000, "u1").await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("first");
    let after_first = balance(&pool, "@i:hs").await;

    // Price doubles, and new usage arrives.
    rate_card(&pool, 2, "egress_gb", 18).await;
    usage(&pool, "@i:hs", "b1", 1_000, "u2").await;
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("second");

    assert_eq!(r.charged_minor, 18, "new usage is priced at the new card");
    assert_eq!(balance(&pool, "@i:hs").await, after_first - 18);

    let versions: Vec<(String, Option<i32>)> = sqlx::query_as(
        "SELECT idempotency_key, rate_card_version FROM mm_usage_events ORDER BY idempotency_key",
    )
    .fetch_all(&pool)
    .await
    .expect("versions");
    assert_eq!(versions[0], ("u1".to_string(), Some(1)), "history keeps its card");
    assert_eq!(versions[1], ("u2".to_string(), Some(2)));
});

// Oldest first: a broadcaster whose balance runs out mid-queue should be charged for
// what they used earliest, not for whichever event happened to be read first.
pg_test!(the_batch_limit_takes_the_oldest_events, pool, {
    wallet_with(&pool, "@j:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 1_000).await;
    for i in 0..5 {
        sqlx::query(
            "INSERT INTO mm_usage_events
                 (user_id, broadcast_id, unit, quantity_milli, idempotency_key, occurred_at)
             VALUES ($1, 'b1', 'egress_gb', 1000, $2, now() - ($3 || ' minutes')::interval)",
        )
        .bind("@j:hs")
        .bind(format!("u{i}"))
        .bind((5 - i).to_string())
        .execute(&pool)
        .await
        .expect("usage");
    }

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 2, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 2, "the batch limit must be honoured");
    assert_eq!(unrated_count(&pool).await, 3);

    // u0 is the oldest (now - 5 minutes).
    let rated: Vec<(String,)> = sqlx::query_as(
        "SELECT idempotency_key FROM mm_usage_events WHERE rated_at IS NOT NULL ORDER BY idempotency_key",
    )
    .fetch_all(&pool)
    .await
    .expect("rated");
    assert_eq!(
        rated,
        vec![("u0".to_string(),), ("u1".to_string(),)],
        "oldest first, so a wallet running dry charges for the earliest usage"
    );
});

// ── Starvation (review 2026-09-25, P1; V038) ─────────────────────────────────

async fn usage_aged(pool: &PgPool, user: &str, milli: i64, key: &str, minutes_ago: i64) {
    sqlx::query(
        "INSERT INTO mm_usage_events
             (user_id, broadcast_id, unit, quantity_milli, idempotency_key, occurred_at)
         VALUES ($1, 'b', 'egress_gb', $2, $3, now() - ($4 || ' minutes')::interval)",
    )
    .bind(user)
    .bind(milli)
    .bind(key)
    .bind(minutes_ago.to_string())
    .execute(pool)
    .await
    .expect("usage");
}

async fn rated(pool: &PgPool, user: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM mm_usage_events WHERE user_id = $1 AND rated_at IS NOT NULL",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("count")
}

// REGRESSION. Unaffordable events stay unrated on purpose AND stay oldest, so plain
// oldest-first let them fill every batch: after five passes a funded broadcaster
// behind two unaffordable events, in a batch of two, had still not been billed.
pg_test!(unaffordable_usage_does_not_starve_a_funded_wallet, pool, {
    wallet_with(&pool, "@broke:hs", 0).await;
    wallet_with(&pool, "@rich:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage_aged(&pool, "@broke:hs", 10_000, "a", 30).await;
    usage_aged(&pool, "@broke:hs", 10_000, "b", 20).await;
    usage_aged(&pool, "@rich:hs", 10_000, "c", 10).await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();
    for i in 0..2 {
        rate_pending(&meter, &wallet, 2, t0 + chrono::Duration::seconds(i)).await.expect("rate");
    }
    assert_eq!(rated(&pool, "@rich:hs").await, 1, "the funded wallet must be billed");
    assert_eq!(rated(&pool, "@broke:hs").await, 0, "and the debt still stands");
});

// The same shape through unpriceable usage: a wallet in a currency with no rate card
// must not block the currency that has one.
pg_test!(unpriceable_usage_does_not_starve_priceable_usage, pool, {
    let w = PgWalletDb::new(pool.clone());
    w.create_wallet("@usd:hs", "usd", 0).await.expect("usd wallet");
    w.deposit("@usd:hs", "usd", 100_000, "dep-usd").await.expect("deposit");
    wallet_with(&pool, "@eur:hs", 100_000).await;
    rate_card(&pool, 1, "egress_gb", 9).await; // eur only
    usage_aged(&pool, "@usd:hs", 1_000, "old-usd", 30).await;
    usage_aged(&pool, "@eur:hs", 1_000, "new-eur", 10).await;

    let meter = PgMeteringDb::new(pool.clone());
    let t0 = Utc::now();
    for i in 0..2 {
        rate_pending(&meter, &w, 1, t0 + chrono::Duration::seconds(i)).await.expect("rate");
    }
    assert_eq!(rated(&pool, "@eur:hs").await, 1);
});

// Oldest first WITHIN a wallet: once an event cannot be afforded, that wallet's newer
// events are not charged in the same pass — even an affordable one — or the
// broadcaster pays for recent usage while older usage is unpaid.
pg_test!(a_wallet_is_not_charged_for_newer_usage_while_older_is_unpaid, pool, {
    wallet_with(&pool, "@w:hs", 50).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage_aged(&pool, "@w:hs", 10_000, "big-old", 20).await; // 90: unaffordable
    usage_aged(&pool, "@w:hs", 1_000, "small-new", 10).await; // 9: affordable

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let r = rate_pending(&meter, &wallet, 100, Utc::now()).await.expect("rate");

    assert_eq!(r.rated, 0);
    assert_eq!(r.deferred, 1, "the newer event waits behind the unpaid older one");
    assert_eq!(balance(&pool, "@w:hs").await, 50);
});

// Blocked wallets must ROTATE. Each failure re-stamps the block, so the longest-
// blocked leads next time; without that, the first wallet ever blocked would lead
// every batch forever and a topped-up wallet behind it would never be retried.
pg_test!(blocked_wallets_rotate_so_a_topped_up_one_is_retried, pool, {
    wallet_with(&pool, "@a:hs", 0).await;
    wallet_with(&pool, "@b:hs", 0).await;
    rate_card(&pool, 1, "egress_gb", 9).await;
    usage_aged(&pool, "@a:hs", 10_000, "ua", 30).await;
    usage_aged(&pool, "@b:hs", 10_000, "ub", 20).await;

    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();
    // Two passes with a batch of one: A is tried and blocked, then B.
    for i in 0..2 {
        rate_pending(&meter, &wallet, 1, t0 + chrono::Duration::seconds(i)).await.expect("rate");
    }

    wallet.deposit("@b:hs", "eur", 1_000, "top-up-b").await.expect("top up");
    for i in 2..4 {
        rate_pending(&meter, &wallet, 1, t0 + chrono::Duration::seconds(i)).await.expect("rate");
    }
    assert_eq!(rated(&pool, "@b:hs").await, 1, "B topped up and must be billed within the rotation");

    let blocked: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT rating_blocked_at FROM mm_broadcaster_wallet WHERE user_id = '@b:hs'")
            .fetch_one(&pool)
            .await
            .expect("blocked_at");
    assert!(blocked.is_none(), "a successful charge brings the wallet back to the front");
});
