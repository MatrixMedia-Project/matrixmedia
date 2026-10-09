//! PG-gated coverage for the transcode opt-in storage (V040; FR-314a/c).
//!
//! The opt-in decides whether a broadcaster's wallet pays for a GPU, so what is
//! tested here is who can change it, what a change does to an operator release,
//! and that the schema refuses values the code cannot read.

use std::sync::OnceLock;

use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_core::fleet::transcode::{TranscodeOptIn, TranscodeOverride};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_db::transcode_db::{self, OverrideRefused};

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

/// Every test in this file holds this for its whole run.
///
/// Unique rows per test are not enough isolation. `v040_is_idempotent` re-runs
/// V040, whose `ALTER TABLE`s take ACCESS EXCLUSIVE on `mm_creator_defaults` and
/// then `mm_streams`, while `set_broadcast_override` updates `mm_streams` and then
/// reads `mm_creator_defaults` in its `RETURNING`. Run in parallel, the two take
/// the same locks in opposite orders and Postgres aborts one of them with
/// `deadlock detected` (40P01) — about 1 run in 20 before this lock existed.
fn db_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A fresh host + active broadcast, unique to the calling test so tests do not
/// share rows.
async fn broadcast(pool: &PgPool, tag: &str) -> (String, String) {
    let host = format!("@tx-{tag}:hs");
    let stream = format!("tx-{tag}");
    let matrix_room_id = format!("!tx-{tag}:hs");
    for sql in [
        "DELETE FROM mm_streams WHERE id = $1",
        "DELETE FROM mm_creator_defaults WHERE creator_user_id = $1",
    ] {
        let bind = if sql.contains("mm_streams") {
            &stream
        } else {
            &host
        };
        sqlx::query(sql)
            .bind(bind)
            .execute(pool)
            .await
            .expect("cleanup");
    }
    sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(&matrix_room_id)
        .execute(pool)
        .await
        .expect("room");
    let room_id: i64 = sqlx::query_scalar("SELECT id FROM mm_rooms WHERE matrix_room_id = $1")
        .bind(&matrix_room_id)
        .fetch_one(pool)
        .await
        .expect("room id");
    sqlx::query(
        "INSERT INTO mm_streams (id, room_id, host_user_id, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(&stream)
    .bind(room_id)
    .bind(&host)
    .execute(pool)
    .await
    .expect("stream");
    (host, stream)
}

/// The operator release, as the admin API performs it.
async fn operator_release(pool: &PgPool, stream: &str) {
    assert!(
        transcode_db::release(pool, stream).await.expect("release"),
        "the broadcast exists, so the release finds it"
    );
}

async fn stored(pool: &PgPool, stream: &str) -> TranscodeOptIn {
    transcode_db::for_broadcast(pool, stream)
        .await
        .expect("read")
        .expect("broadcast exists")
        .opt_in
}

#[tokio::test]
async fn a_new_broadcast_is_not_opted_in() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream) = broadcast(&pool, "fresh").await;

    let b = transcode_db::for_broadcast(&pool, &stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b.host_user_id, host);
    assert!(b.active);
    assert_eq!(b.opt_in, TranscodeOptIn::default());
    assert!(
        !b.opt_in.wants_transcoder(),
        "nobody chose a GPU, so nobody gets one"
    );

    assert!(
        !transcode_db::broadcaster_default(&pool, &host)
            .await
            .unwrap()
    );
    assert!(
        transcode_db::for_broadcast(&pool, "tx-no-such")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_broadcaster_default_reaches_their_broadcasts_and_touches_no_other_default() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream) = broadcast(&pool, "default").await;

    // Another default the broadcaster already chose must survive the write.
    sqlx::query(
        "INSERT INTO mm_creator_defaults (creator_user_id, default_stream_min_tier, ads_enabled)
         VALUES ($1, 3, false)",
    )
    .bind(&host)
    .execute(&pool)
    .await
    .expect("existing defaults");

    transcode_db::set_broadcaster_default(&pool, &host, true)
        .await
        .unwrap();
    assert!(
        transcode_db::broadcaster_default(&pool, &host)
            .await
            .unwrap()
    );
    assert!(
        stored(&pool, &stream).await.wants_transcoder(),
        "inherit follows the default"
    );

    let (tier, ads): (i32, bool) = sqlx::query_as(
        "SELECT default_stream_min_tier, ads_enabled FROM mm_creator_defaults
          WHERE creator_user_id = $1",
    )
    .bind(&host)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (tier, ads),
        (3, false),
        "setting the transcode default clobbered another default"
    );

    transcode_db::set_broadcaster_default(&pool, &host, false)
        .await
        .unwrap();
    assert!(!stored(&pool, &stream).await.wants_transcoder());
}

#[tokio::test]
async fn only_the_host_may_change_an_active_broadcast() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream) = broadcast(&pool, "authz").await;

    let r = transcode_db::set_broadcast_override(
        &pool,
        &stream,
        "@someone-else:hs",
        TranscodeOverride::On,
    )
    .await
    .unwrap();
    assert_eq!(r, Err(OverrideRefused::NotHost));
    assert_eq!(
        stored(&pool, &stream).await,
        TranscodeOptIn::default(),
        "refused, yet written"
    );

    let r = transcode_db::set_broadcast_override(&pool, "tx-no-such", &host, TranscodeOverride::On)
        .await
        .unwrap();
    assert_eq!(r, Err(OverrideRefused::NotFound));

    let r = transcode_db::set_broadcast_override(&pool, &stream, &host, TranscodeOverride::On)
        .await
        .unwrap()
        .expect("host may set it");
    assert!(r.wants_transcoder());

    sqlx::query("UPDATE mm_streams SET status = 'ended', ended_at = now() WHERE id = $1")
        .bind(&stream)
        .execute(&pool)
        .await
        .unwrap();
    let r = transcode_db::set_broadcast_override(&pool, &stream, &host, TranscodeOverride::Off)
        .await
        .unwrap();
    assert_eq!(r, Err(OverrideRefused::Ended));
    assert_eq!(
        stored(&pool, &stream).await.broadcast_override,
        TranscodeOverride::On,
        "an ended broadcast's record changed"
    );
}

/// FR-314c: an operator release sticks until the broadcaster opts THIS broadcast
/// in again — an explicit `on`. Nothing else clears it.
#[tokio::test]
async fn a_release_sticks_until_the_broadcaster_opts_in_again() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream) = broadcast(&pool, "release").await;

    transcode_db::set_broadcast_override(&pool, &stream, &host, TranscodeOverride::On)
        .await
        .unwrap()
        .unwrap();
    operator_release(&pool, &stream).await;
    assert!(
        !stored(&pool, &stream).await.wants_transcoder(),
        "release must veto"
    );

    // Not by changing the default...
    transcode_db::set_broadcaster_default(&pool, &host, true)
        .await
        .unwrap();
    // ...nor by deferring to it, nor by turning it off.
    for not_a_re_opt_in in [TranscodeOverride::Inherit, TranscodeOverride::Off] {
        let r = transcode_db::set_broadcast_override(&pool, &stream, &host, not_a_re_opt_in)
            .await
            .unwrap()
            .unwrap();
        assert!(r.released, "{not_a_re_opt_in} cleared the release");
        assert!(!r.wants_transcoder());
    }

    let r = transcode_db::set_broadcast_override(&pool, &stream, &host, TranscodeOverride::On)
        .await
        .unwrap()
        .unwrap();
    assert!(!r.released, "an explicit 'on' is the re-opt-in");
    assert!(r.wants_transcoder());
    assert_eq!(
        stored(&pool, &stream).await,
        r,
        "the returned choice is the stored one"
    );
}

/// FR-314c: `release` vetoes exactly the broadcast it names, says whether that broadcast
/// exists, and is not undone by releasing twice.
#[tokio::test]
async fn release_vetoes_one_broadcast_and_says_whether_it_exists() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream) = broadcast(&pool, "release-one").await;
    let (other_host, other) = broadcast(&pool, "release-other").await;
    for (h, s) in [(&host, &stream), (&other_host, &other)] {
        transcode_db::set_broadcast_override(&pool, s, h, TranscodeOverride::On)
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        stored(&pool, &stream).await.wants_transcoder(),
        "the host opted this broadcast in"
    );

    assert!(transcode_db::release(&pool, &stream).await.unwrap());
    assert!(
        stored(&pool, &stream).await.released,
        "the named broadcast is released"
    );
    assert!(
        !stored(&pool, &other).await.released,
        "no other broadcast is touched"
    );

    assert!(
        transcode_db::release(&pool, &stream).await.unwrap(),
        "releasing again finds the broadcast and leaves it released"
    );
    assert!(stored(&pool, &stream).await.released);

    assert!(
        !transcode_db::release(&pool, "tx-no-such-broadcast")
            .await
            .unwrap(),
        "an unknown broadcast is reported, not an error"
    );
}

#[tokio::test]
async fn the_schema_refuses_an_override_the_code_cannot_read() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (_, stream) = broadcast(&pool, "check").await;

    let err = sqlx::query("UPDATE mm_streams SET transcode_opt_in = 'maybe' WHERE id = $1")
        .bind(&stream)
        .execute(&pool)
        .await
        .expect_err("CHECK must refuse an unknown value");
    assert!(
        err.to_string().contains("streams_transcode_opt_in_valid"),
        "refused for the wrong reason: {err}"
    );
}

/// Every migration may re-execute against a database that predates the apply-once
/// ledger, so V040 must be a no-op the second time.
#[tokio::test]
async fn v040_is_idempotent() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let sql = include_str!("../migrations/V040__transcode_opt_in.sql");
    for _ in 0..2 {
        sqlx::raw_sql(sql)
            .execute(&pool)
            .await
            .expect("V040 re-run");
    }
}
