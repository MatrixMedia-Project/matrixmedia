//! `Database::list_active_streams_visible_to` against a live PostgreSQL — the
//! query behind `GET /_mm/client/v1/streams/active-mine`.
//!
//! Every test uses its own room ids and MXIDs, so rows from other tests or
//! earlier runs can never match the filter, and the file-level lock only
//! guards the `started_at` rewrites.

use std::sync::OnceLock;

use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_core::types::{RoomId, UserId};
use mm_db::models::Stream;
use mm_db::{Database, PgDatabase};

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

fn visible_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A suffix unique to this test run, so room ids and MXIDs never collide.
fn run_tag(test: &str) -> String {
    format!("{test}-{}", uuid::Uuid::new_v4().simple())
}

async fn live_stream(db: &PgDatabase, matrix_room_id: &str, host: &str) -> Stream {
    let room = db
        .get_or_create_room(&RoomId(matrix_room_id.to_string()))
        .await
        .expect("create room");
    db.create_stream(room.id, &UserId(host.to_string()), Some("live"), "video", None, None)
        .await
        .expect("create stream")
}

/// Backdate a stream so the ordering under test does not depend on insert timing.
async fn set_started_minutes_ago(pool: &PgPool, stream: &Stream, minutes: i32) {
    sqlx::query("UPDATE mm_streams SET started_at = now() - make_interval(mins => $2) WHERE id = $1")
        .bind(&stream.id)
        .bind(minutes)
        .execute(pool)
        .await
        .expect("backdate stream");
}

fn ids(visible: &[(Stream, String)]) -> Vec<&str> {
    visible.iter().map(|(s, _)| s.id.as_str()).collect()
}

#[tokio::test]
async fn a_stream_in_a_room_the_caller_has_not_joined_is_not_returned() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_stream_in_a_room_the_caller_has_not_joined_is_not_returned");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = visible_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());
    let tag = run_tag("not-joined");

    let caller = UserId(format!("@caller-{tag}:hs"));
    let joined_room = format!("!joined-{tag}:hs");
    let private_room = format!("!private-{tag}:hs");
    let in_joined = live_stream(&db, &joined_room, &format!("@host-a-{tag}:hs")).await;
    let in_private = live_stream(&db, &private_room, &format!("@host-b-{tag}:hs")).await;

    let visible = db
        .list_active_streams_visible_to(&caller, &[joined_room.clone()], 100)
        .await
        .expect("query");

    assert_eq!(ids(&visible), vec![in_joined.id.as_str()]);
    assert_eq!(visible[0].1, joined_room, "entry carries the Matrix room id");
    assert!(
        !visible.iter().any(|(s, room)| s.id == in_private.id || *room == private_room),
        "a room the caller is not in must not leak its stream or its room id"
    );

    // No membership at all (e.g. the Synapse lookup failed): nothing of anyone else's.
    let none = db
        .list_active_streams_visible_to(&caller, &[], 100)
        .await
        .expect("query");
    assert!(none.is_empty(), "no joined rooms and no own streams means an empty list");
}

#[tokio::test]
async fn the_callers_own_stream_is_returned_without_membership() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping the_callers_own_stream_is_returned_without_membership");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = visible_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());
    let tag = run_tag("own");

    let caller = format!("@caller-{tag}:hs");
    let own_room = format!("!own-{tag}:hs");
    let own = live_stream(&db, &own_room, &caller).await;

    // The caller's joined-room list doesn't name the room — membership lookup
    // failed, or the host left the room while still live.
    let visible = db
        .list_active_streams_visible_to(&UserId(caller.clone()), &[], 100)
        .await
        .expect("query");

    assert_eq!(ids(&visible), vec![own.id.as_str()]);
    assert_eq!(visible[0].1, own_room);
    assert_eq!(visible[0].0.host_user_id, caller);
}

#[tokio::test]
async fn the_limit_never_cuts_the_callers_own_stream() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping the_limit_never_cuts_the_callers_own_stream");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = visible_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());
    let tag = run_tag("cap");

    let caller = format!("@caller-{tag}:hs");
    // The caller's own stream is the OLDEST, so a plain `started_at DESC LIMIT`
    // (the old behaviour) would drop it first.
    let own = live_stream(&db, &format!("!own-{tag}:hs"), &caller).await;
    set_started_minutes_ago(&pool, &own, 60).await;

    let mut joined = Vec::new();
    let mut others = Vec::new();
    for i in 0..4 {
        let room = format!("!joined-{i}-{tag}:hs");
        let s = live_stream(&db, &room, &format!("@host-{i}-{tag}:hs")).await;
        set_started_minutes_ago(&pool, &s, 10 - i).await;
        joined.push(room);
        others.push(s);
    }

    let visible = db
        .list_active_streams_visible_to(&UserId(caller), &joined, 3)
        .await
        .expect("query");

    assert_eq!(visible.len(), 3, "the limit still bounds the response");
    assert_eq!(visible[0].0.id, own.id, "own stream first, whatever its age");
    // The rest are the newest of the joined rooms' streams.
    assert_eq!(
        ids(&visible)[1..],
        [others[3].id.as_str(), others[2].id.as_str()],
        "other hosts' streams follow, newest first"
    );
}

#[tokio::test]
async fn ended_streams_are_not_returned() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping ended_streams_are_not_returned");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = visible_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());
    let tag = run_tag("ended");

    let caller = format!("@caller-{tag}:hs");
    let room = format!("!room-{tag}:hs");
    let own_ended = live_stream(&db, &format!("!own-{tag}:hs"), &caller).await;
    let other_ended = live_stream(&db, &room, &format!("@host-{tag}:hs")).await;
    for s in [&own_ended, &other_ended] {
        assert!(db.end_stream_if_active(&mm_core::types::StreamId(s.id.clone())).await.unwrap());
    }

    let visible = db
        .list_active_streams_visible_to(&UserId(caller), &[room], 100)
        .await
        .expect("query");
    assert!(visible.is_empty(), "only live streams are listed");
}
