//! The demotion ladder's hold on recording, against a real database (§17.4).
//!
//! Two things, both money: that the ladder's "stop recording" actually stops a LiveKit
//! recording (the first extraction marked the row `ready` and left the egress running
//! and uploading), and that `start_recording`'s gate refuses a new one while a
//! broadcast is demoted — and only then.
//!
//! Factored over narrow contexts rather than `AppState`, like
//! `stream_lifecycle_test.rs`, because `AppState` has 30+ fields.

use std::sync::{Mutex as StdMutex, OnceLock};

use mm_api::ladder_actuator::{recording_refusal, RECORDING_REFUSED};
use mm_api::stream_lifecycle::finalise_open_recordings_with;
use mm_sfu::{
    CreateRoomRequest, ParticipantInfo, ParticipantPermissions, RoomStats, SfuAdapter, SfuError,
    SfuRoom, SfuToken,
};
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_db::test_support::require_or_try_pool as try_pool;

async fn setup(pool: &PgPool) {
    static M: OnceLock<Mutex<bool>> = OnceLock::new();
    let mut done = M.get_or_init(|| Mutex::new(false)).lock().await;
    if !*done {
        mm_db::run_pg_migrations(pool).await.expect("migrate");
        *done = true;
    }
}

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// An SFU that records which egresses it was asked to stop.
#[derive(Default)]
struct EgressSfu {
    stopped: StdMutex<Vec<String>>,
}

#[async_trait::async_trait]
impl SfuAdapter for EgressSfu {
    fn name(&self) -> &str {
        "egress-stub"
    }
    async fn health_check(&self) -> Result<(), SfuError> {
        Ok(())
    }
    async fn create_room(&self, req: CreateRoomRequest) -> Result<SfuRoom, SfuError> {
        Ok(SfuRoom { sfu_room_id: req.name.clone(), name: req.name, num_participants: 0 })
    }
    async fn delete_room(&self, _: &str) -> Result<(), SfuError> {
        Ok(())
    }
    async fn generate_token(
        &self,
        _: &SfuRoom,
        _: &ParticipantInfo,
        _: ParticipantPermissions,
    ) -> Result<SfuToken, SfuError> {
        Ok(SfuToken { token: "t".into(), url: "ws://stub".into() })
    }
    async fn remove_participant(&self, _: &str, _: &str) -> Result<(), SfuError> {
        Ok(())
    }
    async fn list_participants(&self, _: &str) -> Result<Vec<ParticipantInfo>, SfuError> {
        Ok(vec![])
    }
    async fn room_stats(&self, id: &str) -> Result<RoomStats, SfuError> {
        Ok(RoomStats {
            sfu_room_id: id.to_string(),
            num_participants: 0,
            num_publishers: 0,
            num_subscribers: 0,
        })
    }
    async fn stop_egress(&self, egress_id: &str) -> Result<(), SfuError> {
        self.stopped.lock().unwrap().push(egress_id.to_string());
        Ok(())
    }
    fn supports_egress(&self) -> bool {
        true
    }
}

/// A unique stream id per test, so the shared database needs no wiping.
fn fresh(tag: &str) -> String {
    format!("{tag}-{}", uuid::Uuid::new_v4())
}

async fn recording(pool: &PgPool, stream: &str, id: &str, egress: Option<&str>, status: &str) {
    sqlx::query(
        "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, storage_key, egress_id)
         VALUES ($1, $2, 1, '@h:hs', $3, 'video', 'k', $4)",
    )
    .bind(id)
    .bind(stream)
    .bind(status)
    .bind(egress)
    .execute(pool)
    .await
    .expect("recording");
}

async fn status_of(pool: &PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM mm_recordings WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("status")
}

async fn rung(pool: &PgPool, broadcast: &str, target: &str, applied: &str) {
    sqlx::query(
        "INSERT INTO mm_broadcast_demotion
             (broadcast_id, target_step, applied_step, balance_minor, projected_cost_minor)
         VALUES ($1, $2, $3, 0, 0)",
    )
    .bind(broadcast)
    .bind(target)
    .bind(applied)
    .execute(pool)
    .await
    .expect("rung");
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!("MM_DATABASE_URL not set — skipping ", stringify!($name)));
                return;
            };
            let _g = lock().lock().await;
            setup(&$pool).await;
            $body
        }
    };
}

// ── The stop ─────────────────────────────────────────────────────────────────

// REGRESSION. The ladder's stop marked a LiveKit recording `ready` and left its egress
// recording and uploading — the storage charge ReduceQuality exists to stop.
pg_test!(the_ladders_stop_stops_a_livekit_recording_at_the_sfu, pool, {
    let s = fresh("lk");
    let rec = format!("{s}-rec");
    recording(&pool, &s, &rec, Some("EG_livekit_1"), "recording").await;
    let sfu = EgressSfu::default();

    let closed = finalise_open_recordings_with(&pool, None, Some(&sfu), &s).await;

    assert_eq!(closed, 1);
    assert_eq!(*sfu.stopped.lock().unwrap(), vec!["EG_livekit_1".to_string()]);
    assert_eq!(status_of(&pool, &rec).await, "ready");
});

// end_stream has already stopped every egress in the room (HLS included), so it passes
// no SFU — and nothing is stopped a second time.
pg_test!(without_an_sfu_the_rows_close_and_no_egress_is_touched, pool, {
    let s = fresh("nosfu");
    let rec = format!("{s}-rec");
    recording(&pool, &s, &rec, Some("EG_livekit_2"), "paused").await;

    let closed = finalise_open_recordings_with(&pool, None, None, &s).await;
    assert_eq!(closed, 1);
    assert_eq!(status_of(&pool, &rec).await, "ready");
});

// An mm-switch recording is finalised by the switch, never sent to the SFU (which
// would reject an id it has never heard of).
pg_test!(an_mm_switch_recording_is_not_sent_to_the_sfu, pool, {
    let s = fresh("sw");
    let rec = format!("{s}-rec");
    recording(&pool, &s, &rec, Some(&format!("mm-switch:stream-{s}")), "recording").await;
    let sfu = EgressSfu::default();

    finalise_open_recordings_with(&pool, None, Some(&sfu), &s).await;
    assert!(sfu.stopped.lock().unwrap().is_empty());
    assert_eq!(status_of(&pool, &rec).await, "ready");
});

// egress_id is nullable. The first version fetched it as a non-null String, so one
// NULL row failed the whole fetch — and `unwrap_or_default` then finalised NOTHING,
// leaving every other recording of the stream running.
pg_test!(a_recording_with_no_egress_id_does_not_stop_the_others_being_stopped, pool, {
    let s = fresh("null");
    let a = format!("{s}-a");
    let b = format!("{s}-b");
    recording(&pool, &s, &a, None, "recording").await;
    recording(&pool, &s, &b, Some("EG_livekit_3"), "recording").await;
    let sfu = EgressSfu::default();

    let closed = finalise_open_recordings_with(&pool, None, Some(&sfu), &s).await;
    assert_eq!(closed, 2);
    assert_eq!(*sfu.stopped.lock().unwrap(), vec!["EG_livekit_3".to_string()]);
});

// Already-finished recordings are left alone.
pg_test!(a_finished_recording_is_not_stopped_again, pool, {
    let s = fresh("done");
    let rec = format!("{s}-rec");
    recording(&pool, &s, &rec, Some("EG_old"), "ready").await;
    let sfu = EgressSfu::default();

    assert_eq!(finalise_open_recordings_with(&pool, None, Some(&sfu), &s).await, 0);
    assert!(sfu.stopped.lock().unwrap().is_empty());
});

// ── The gate ─────────────────────────────────────────────────────────────────

// THE GATE. Without it a recording the ladder stopped came back with one tap.
pg_test!(recording_is_refused_while_the_stop_is_in_force, pool, {
    let s = fresh("gate");
    rung(&pool, &s, "reduce_quality", "reduce_quality").await;
    assert_eq!(recording_refusal(Some(&pool), &s).await.as_deref(), Some(RECORDING_REFUSED));

    let deeper = fresh("gate-drain");
    rung(&pool, &deeper, "drain_to_origin", "drain_to_origin").await;
    assert!(recording_refusal(Some(&pool), &deeper).await.is_some(), "and on every harsher rung");
});

// It reads what is APPLIED, not what was decided — which is what makes observe mode,
// where nothing is ever applied, refuse nothing.
pg_test!(a_decided_but_unapplied_rung_does_not_refuse, pool, {
    let s = fresh("observe");
    rung(&pool, &s, "reduce_quality", "healthy").await;
    assert_eq!(recording_refusal(Some(&pool), &s).await, None);
});

pg_test!(a_milder_rung_or_no_rung_does_not_refuse, pool, {
    let s = fresh("mild");
    rung(&pool, &s, "stop_provisioning", "stop_provisioning").await;
    assert_eq!(recording_refusal(Some(&pool), &s).await, None, "stop_provisioning allows recording");

    assert_eq!(recording_refusal(Some(&pool), &fresh("none")).await, None, "never evaluated");
});

// SQLite installs have no ladder.
#[tokio::test]
async fn without_postgres_nothing_is_refused() {
    assert_eq!(recording_refusal(None, "anything").await, None);
}
