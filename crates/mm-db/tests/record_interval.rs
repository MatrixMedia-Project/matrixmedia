//! `record_interval`'s accounting of what it actually wrote (WS-D, FR-303b/305c).
//!
//! The sweep's own tests cover deriving intervals. This covers the narrower question
//! of what the write **reports** when some of its rows are de-duplicated — which is
//! what the egress-bytes metric is built from, and therefore what the per-packet
//! overhead calibration is built from.

use std::sync::OnceLock;

use chrono::Utc;
use mm_db::metering_db::{EgressInterval, PgMeteringDb};
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

fn record_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn wipe(pool: &PgPool) {
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL mm.ledger_maintenance = 'on'")
        .execute(&mut *tx)
        .await
        .expect("maintenance door");
    // Order matters: mm_wallet_transactions has a composite FK onto the wallet
    // (currency included, so a charge can never be in a currency the wallet is not
    // held in), so the ledger goes before the wallets.
    for t in [
        "mm_egress_baseline",
        "mm_usage_events",
        "mm_wallet_transactions",
        "mm_broadcaster_wallet",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("wipe {t}: {e}"));
    }
    tx.commit().await.expect("commit");
}

async fn wallet(pool: &PgPool, user: &str) {
    sqlx::query("INSERT INTO mm_broadcaster_wallet (user_id, currency) VALUES ($1, 'eur')")
        .bind(user)
        .execute(pool)
        .await
        .expect("wallet");
}

fn interval(source: &str, user: &str, bytes: i64, key: &str) -> EgressInterval {
    EgressInterval {
        source: source.into(),
        user_id: user.into(),
        broadcast_id: source.trim_start_matches("stream-").into(),
        bytes,
        // A whole gigabyte per 1e9 bytes, which is all these tests need.
        quantity_milli: bytes / 1_000_000,
        cumulative_bytes: bytes,
        epoch: "e1".into(),
        idempotency_key: key.into(),
        occurred_at: Utc::now(),
    }
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!("MM_DATABASE_URL not set — skipping ", stringify!($name)));
                return;
            };
            let _guard = record_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

pg_test!(a_fresh_write_reports_every_row_and_every_byte, pool, {
    wallet(&pool, "@a:hs").await;
    let db = PgMeteringDb::new(pool.clone());
    let ivs = vec![
        interval("stream-b1", "@a:hs", 1_000_000_000, "k1"),
        interval("stream-b2", "@a:hs", 2_000_000_000, "k2"),
    ];
    let r = db
        .record_interval("n1", "e1", Utc::now(), &ivs, &[])
        .await
        .expect("record");
    assert_eq!(r.events, 2);
    assert_eq!(r.bytes, 3_000_000_000);
});

// THE GUARD. A row whose idempotency key is already present is de-duplicated at
// V035's UNIQUE constraint, and its bytes must not be counted again — they were
// counted by whatever wrote the row. Reported bytes feed the one metric that can tell
// us whether the per-packet overhead estimate is right; inflating them there makes a
// correct estimate look wrong.
pg_test!(a_deduplicated_row_contributes_neither_an_event_nor_its_bytes, pool, {
    wallet(&pool, "@a:hs").await;
    let db = PgMeteringDb::new(pool.clone());

    let first = vec![interval("stream-b1", "@a:hs", 1_000_000_000, "k1")];
    db.record_interval("n1", "e1", Utc::now(), &first, &[])
        .await
        .expect("first");

    // The same key again, alongside a genuinely new row.
    let replay = vec![
        interval("stream-b1", "@a:hs", 1_000_000_000, "k1"),
        interval("stream-b2", "@a:hs", 5_000_000_000, "k2"),
    ];
    let r = db
        .record_interval("n1", "e1", Utc::now(), &replay, &[])
        .await
        .expect("replay");

    assert_eq!(r.events, 1, "only the new row");
    assert_eq!(
        r.bytes, 5_000_000_000,
        "only the NEW row's bytes — and note this is the second interval, so a count \
         that took the first `events` intervals would report the wrong 1 GB here"
    );

    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_usage_events")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(total, 2, "and the replay wrote no duplicate");
});

// FR-305c: usage and the advanced baseline are one transaction. A baseline advanced
// without its usage loses that interval's revenue permanently; usage without the
// baseline is re-derived and de-duplicated. Only one of those is recoverable, so a
// failure must leave neither.
pg_test!(a_failed_write_advances_no_baseline, pool, {
    let db = PgMeteringDb::new(pool.clone());
    // A negative quantity, which V035's sign CHECK rejects. (The first draft of this
    // test used a user with no wallet, on the assumption that `mm_usage_events.user_id`
    // has a foreign key onto the wallet. It does not, deliberately — usage is metered
    // from what the switch reports, and a broadcaster having no wallet must not make
    // their bytes unrecordable. That is precisely why `unrated_without_wallet()` has
    // to exist as a separate probe.)
    let mut ivs = vec![interval("stream-b1", "@nobody:hs", 1_000_000_000, "k1")];
    ivs[0].quantity_milli = -1;
    let err = db
        .record_interval("n1", "e1", Utc::now(), &ivs, &[("stream-b1".into(), 1_000_000_000)])
        .await;
    assert!(err.is_err(), "the usage insert must fail");

    let baselines: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_egress_baseline")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(
        baselines, 0,
        "the baseline must NOT have advanced: a baseline past unwritten usage loses \
         those bytes permanently and silently"
    );
});
