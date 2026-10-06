//! The live viewer gate: `mm_api::client::authorize_viewer`, behind
//! `POST /streams/{id}/join`, and `mm_api::switch_proxy::admit_offer`, behind
//! the fleet viewer proxy's `POST /_mm/fleet/v1/streams/{id}/api/viewers/offer`.
//! The proxy used to mint a viewer token for any member's offer with none of
//! `/join`'s gates, so it was a way around the paywall. Each test asks both
//! paths the same question and expects the same answer.
//!
//! `AppState` is impractical to build in tests, so the gate is factored over
//! `ViewerGate` and these drive it against a real PostgreSQL (env-gated on
//! `MM_DATABASE_URL`), real subscription and tier rows read by the real
//! `EntitlementService` and tier resolver, and an in-test stub of Synapse's
//! admin `joined_rooms` endpoint. Every test uses its own room, creator and
//! MXIDs, so they run in parallel.
//!
//! The entitlement source is the real service over seeded rows, not a stub,
//! because the tier gate's `can_join_live` check reads the same subscription
//! rows directly: a stub would let the two halves of the gate disagree.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_api::client::{ViewerGate, authorize_viewer};
use mm_api::error::ApiError;
use mm_api::switch_proxy::admit_offer;
use mm_core::permissions::TierPermissions;
use mm_core::switch_client::switch_source_id;
use mm_core::types::{ParticipantRole, RoomId, StreamId, UserId};
use mm_db::models::Stream;
use mm_db::{Database, PgDatabase};
use mm_payment::EntitlementService;

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

#[derive(Default)]
struct StubSynapse {
    /// MXID → the rooms Synapse says it has joined.
    joined: StdMutex<HashMap<String, Vec<String>>>,
    requests: AtomicUsize,
}

async fn joined_rooms(State(stub): State<Arc<StubSynapse>>, Path(user_id): Path<String>) -> Response {
    stub.requests.fetch_add(1, Ordering::SeqCst);
    let rooms = stub.joined.lock().unwrap().get(&user_id).cloned().unwrap_or_default();
    axum::Json(json!({ "total": rooms.len(), "joined_rooms": rooms })).into_response()
}

/// What a caller gets from either path: in, or the response a refusal becomes.
#[derive(Debug, PartialEq)]
enum Outcome {
    Admitted,
    Refused { status: StatusCode, code: String, message: String },
}

async fn outcome(result: Result<Stream, ApiError>) -> Outcome {
    match result {
        Ok(_) => Outcome::Admitted,
        Err(err) => {
            let response = err.into_response();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read the error body");
            let body: Value = serde_json::from_slice(&body).expect("error body is JSON");
            Outcome::Refused {
                status,
                code: body["error"].as_str().unwrap_or_default().to_string(),
                message: body["message"].as_str().unwrap_or_default().to_string(),
            }
        }
    }
}

fn refused(outcome: &Outcome) -> (StatusCode, &str) {
    match outcome {
        Outcome::Refused { status, code, .. } => (*status, code.as_str()),
        Outcome::Admitted => panic!("expected a refusal, the caller was admitted"),
    }
}

struct Fixture {
    pool: PgPool,
    db: PgDatabase,
    entitlements: EntitlementService,
    permissions_cache: moka::future::Cache<(String, String), TierPermissions>,
    stub: Arc<StubSynapse>,
    synapse_url: String,
    /// The Matrix room id, which is also what tiers and subscriptions scope to.
    room: String,
    room_row: i64,
    host: UserId,
}

async fn fixture(pool: &PgPool, test: &str) -> Fixture {
    let tag = format!("{test}-{}", uuid::Uuid::new_v4().simple());
    let db = PgDatabase::from_pool(pool.clone());
    let room = format!("!room-{tag}:hs");
    let host = UserId(format!("@host-{tag}:hs"));
    let row = db.get_or_create_room(&RoomId(room.clone())).await.expect("create room");
    // The room's free tier, as the creator flow seeds it: read + tip, no
    // can_join_live.
    db.ensure_spectator_tier(&host.0, &room).await.expect("seed spectator tier");

    let stub = Arc::new(StubSynapse::default());
    let app = Router::new()
        .route("/_synapse/admin/v1/users/{user_id}/joined_rooms", get(joined_rooms))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Fixture {
        pool: pool.clone(),
        db,
        entitlements: EntitlementService::new(pool.clone(), None, None),
        permissions_cache: moka::future::Cache::new(1_000),
        stub,
        synapse_url: format!("http://{addr}"),
        room,
        room_row: row.id,
        host,
    }
}

impl Fixture {
    fn gate(&self) -> ViewerGate<'_> {
        ViewerGate {
            db: &self.db,
            http: mm_core::http::shared(),
            homeserver_url: &self.synapse_url,
            synapse_admin_token: "admin-tok",
            monetization_enabled: true,
            entitlements: Some(&self.entitlements),
            permissions_cache: &self.permissions_cache,
            pg_pool: Some(&self.pool),
        }
    }

    async fn stream(&self, min_tier_level: Option<i32>) -> Stream {
        self.db
            .create_stream(self.room_row, &self.host, Some("broadcast"), "video", None, min_tier_level)
            .await
            .expect("create stream")
    }

    /// Someone Synapse says has joined the stream's room.
    fn member(&self, role: &str) -> UserId {
        let user = someone(role);
        self.stub
            .joined
            .lock()
            .unwrap()
            .entry(user.0.clone())
            .or_default()
            .push(self.room.clone());
        user
    }

    /// A tier of this creator's room ladder at `level`, carrying `perms`.
    async fn tier(&self, level: i32, perms: TierPermissions) -> uuid::Uuid {
        sqlx::query_scalar(
            "INSERT INTO mm_subscription_tiers
                (creator_user_id, room_id, tier_level, name, price_cents, currency,
                 perks_json, permissions, is_active)
             VALUES ($1, $2, $3, $4, 500, 'usd', '[]'::jsonb, $5, true)
             RETURNING id",
        )
        .bind(&self.host.0)
        .bind(&self.room)
        .bind(level)
        .bind(format!("Tier {level}"))
        .bind(serde_json::to_value(perms).unwrap())
        .fetch_one(&self.pool)
        .await
        .expect("create tier")
    }

    async fn subscribe(&self, subscriber: &UserId, tier: uuid::Uuid) {
        sqlx::query(
            "INSERT INTO mm_subscriptions
                (subscriber_user_id, creator_user_id, room_id, tier_id, status, current_period_end)
             VALUES ($1, $2, $3, $4, 'active', now() + interval '30 days')",
        )
        .bind(&subscriber.0)
        .bind(&self.host.0)
        .bind(&self.room)
        .bind(tier)
        .execute(&self.pool)
        .await
        .expect("create subscription");
    }

    async fn via_join(&self, stream: &Stream, caller: &UserId) -> Outcome {
        outcome(authorize_viewer(&self.gate(), &stream.id, caller).await).await
    }

    /// The proxy with the flag on, sending the source the join response names,
    /// as the shipped apps do.
    async fn via_proxy(&self, stream: &Stream, caller: &UserId) -> Outcome {
        let source = switch_source_id(&stream.id);
        outcome(admit_offer(&self.gate(), true, &stream.id, caller, Some(&source)).await).await
    }

    /// Both paths, asserted to agree, and their shared answer.
    async fn both(&self, stream: &Stream, caller: &UserId) -> Outcome {
        let join = self.via_join(stream, caller).await;
        let proxy = self.via_proxy(stream, caller).await;
        assert_eq!(proxy, join, "the proxy must answer exactly as /join does");
        join
    }

    fn requests(&self) -> usize {
        self.stub.requests.load(Ordering::SeqCst)
    }
}

fn someone(role: &str) -> UserId {
    UserId(format!("@{role}-{}:hs", uuid::Uuid::new_v4().simple()))
}

macro_rules! pool_or_skip {
    ($name:literal) => {{
        let Some(pool) = try_pool().await else {
            eprintln!(concat!("MM_DATABASE_URL not set — skipping ", $name));
            return;
        };
        ensure_migrations(&pool).await;
        pool
    }};
}

#[tokio::test]
async fn an_unentitled_member_is_refused_a_tier_gated_stream_by_the_proxy_as_by_join() {
    let pool = pool_or_skip!("an_unentitled_member_is_refused_a_tier_gated_stream_by_the_proxy_as_by_join");
    let f = fixture(&pool, "tier-gated").await;
    let stream = f.stream(Some(2)).await;
    let member = f.member("viewer");

    let proxy = f.via_proxy(&stream, &member).await;
    // The Spectator tier lacks can_join_live: the 403 the apps read as a paywall.
    assert_eq!(refused(&proxy), (StatusCode::FORBIDDEN, "MM_PERMISSION_DENIED"));
    assert_eq!(f.via_join(&stream, &member).await, proxy, "identical refusal on /join");
}

#[tokio::test]
async fn a_member_below_the_streams_level_gets_tier_too_low_from_both() {
    let pool = pool_or_skip!("a_member_below_the_streams_level_gets_tier_too_low_from_both");
    let f = fixture(&pool, "too-low").await;
    let stream = f.stream(Some(2)).await;
    let member = f.member("viewer");
    let level_one = f.tier(1, TierPermissions::full_size_user_default()).await;
    f.subscribe(&member, level_one).await;

    let answer = f.both(&stream, &member).await;
    assert_eq!(refused(&answer), (StatusCode::PAYMENT_REQUIRED, "MM_TIER_TOO_LOW"));
}

#[tokio::test]
async fn a_content_gated_stream_refuses_an_unsubscribed_member_on_both() {
    let pool = pool_or_skip!("a_content_gated_stream_refuses_an_unsubscribed_member_on_both");
    let f = fixture(&pool, "content-gated").await;
    // Gated only through mm_content_gates (the legacy gate), not min_tier_level.
    let stream = f.stream(None).await;
    f.db.create_content_gate("stream", &stream.id, &f.host.0, 1, 0)
        .await
        .expect("create content gate");
    let member = f.member("viewer");

    let answer = f.both(&stream, &member).await;
    assert_eq!(refused(&answer), (StatusCode::PAYMENT_REQUIRED, "MM_CONTENT_GATED"));
}

#[tokio::test]
async fn an_entitled_member_is_admitted_by_both() {
    let pool = pool_or_skip!("an_entitled_member_is_admitted_by_both");
    let f = fixture(&pool, "entitled").await;
    let stream = f.stream(Some(2)).await;
    f.db.create_content_gate("stream", &stream.id, &f.host.0, 2, 0)
        .await
        .expect("create content gate");
    let member = f.member("subscriber");
    let level_two = f.tier(2, TierPermissions::full_size_user_default()).await;
    f.subscribe(&member, level_two).await;

    assert_eq!(f.both(&stream, &member).await, Outcome::Admitted);
}

#[tokio::test]
async fn the_host_watches_their_own_gated_stream_on_both() {
    let pool = pool_or_skip!("the_host_watches_their_own_gated_stream_on_both");
    let f = fixture(&pool, "host").await;
    // Both gates at once, and no subscription: a host has none to themselves.
    let stream = f.stream(Some(2)).await;
    f.db.create_content_gate("stream", &stream.id, &f.host.0, 2, 0)
        .await
        .expect("create content gate");

    assert_eq!(f.both(&stream, &f.host).await, Outcome::Admitted);
    assert_eq!(f.requests(), 0, "the host needs no membership lookup");
}

#[tokio::test]
async fn a_free_stream_admits_a_member_on_both() {
    let pool = pool_or_skip!("a_free_stream_admits_a_member_on_both");
    let f = fixture(&pool, "free").await;
    let member = f.member("viewer");
    // NULL and 0 both mean free: a Spectator lacks can_join_live, and that must
    // not paywall a broadcast for everyone.
    for min in [None, Some(0)] {
        let stream = f.stream(min).await;
        assert_eq!(f.both(&stream, &member).await, Outcome::Admitted, "min_tier_level {min:?}");
    }
}

#[tokio::test]
async fn a_non_member_gets_404_from_both_never_a_paywall() {
    let pool = pool_or_skip!("a_non_member_gets_404_from_both_never_a_paywall");
    let f = fixture(&pool, "non-member").await;
    let stream = f.stream(Some(2)).await;
    let stranger = someone("stranger");

    let answer = f.both(&stream, &stranger).await;
    // A 402/403 here would show a paywall no purchase can lift.
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
}

#[tokio::test]
async fn the_proxy_refuses_every_offer_while_it_is_off() {
    let pool = pool_or_skip!("the_proxy_refuses_every_offer_while_it_is_off");
    let f = fixture(&pool, "proxy-off").await;
    let stream = f.stream(None).await;
    let member = f.member("viewer");
    let source = switch_source_id(&stream.id);

    for caller in [&member, &f.host] {
        let answer = outcome(admit_offer(&f.gate(), false, &stream.id, caller, Some(&source)).await).await;
        assert_eq!(refused(&answer), (StatusCode::NOT_IMPLEMENTED, "MM_FEATURE_DISABLED"));
    }
    // Refused before any lookup, so a disabled route says nothing about the stream.
    assert_eq!(f.requests(), 0);
    // /join is not the proxy and is unaffected by its flag.
    assert_eq!(f.via_join(&stream, &member).await, Outcome::Admitted);
}

#[tokio::test]
async fn the_proxy_refuses_another_streams_source() {
    let pool = pool_or_skip!("the_proxy_refuses_another_streams_source");
    let f = fixture(&pool, "source-pin").await;
    let free = f.stream(None).await;
    let gated = f.stream(Some(2)).await;
    let member = f.member("viewer");

    // Admitted to the free stream, asking for the gated one's media.
    let wrong = switch_source_id(&gated.id);
    let answer = outcome(admit_offer(&f.gate(), true, &free.id, &member, Some(&wrong)).await).await;
    assert_eq!(refused(&answer), (StatusCode::BAD_REQUEST, "MM_INVALID_REQUEST"));

    // Its own source, or none, is fine.
    let own = switch_source_id(&free.id);
    for source in [Some(own.as_str()), None] {
        let answer = outcome(admit_offer(&f.gate(), true, &free.id, &member, source).await).await;
        assert_eq!(answer, Outcome::Admitted, "source {source:?}");
    }
}

#[tokio::test]
async fn a_full_room_refuses_a_new_viewer_on_both_but_not_one_already_seated() {
    let pool = pool_or_skip!("a_full_room_refuses_a_new_viewer_on_both_but_not_one_already_seated");
    let f = fixture(&pool, "capacity").await;
    sqlx::query("UPDATE mm_rooms SET max_participants = 2 WHERE id = $1")
        .bind(f.room_row)
        .execute(&f.pool)
        .await
        .expect("shrink the room");
    let stream = f.stream(None).await;
    let seated = f.member("seated");
    let other = f.member("other");
    for user in [&seated, &other] {
        f.db.add_participant(&StreamId(stream.id.clone()), user, ParticipantRole::Viewer, None)
            .await
            .expect("seat a viewer");
    }

    let latecomer = f.member("latecomer");
    let answer = f.both(&stream, &latecomer).await;
    assert_eq!(refused(&answer), (StatusCode::CONFLICT, "MM_ROOM_FULL"));

    // The offer that follows a join, or a re-join, takes no new seat.
    assert_eq!(f.both(&stream, &seated).await, Outcome::Admitted);
}

#[tokio::test]
async fn without_a_monetization_backend_no_gate_applies_but_membership_does() {
    let pool = pool_or_skip!("without_a_monetization_backend_no_gate_applies_but_membership_does");
    let f = fixture(&pool, "no-backend").await;
    let stream = f.stream(Some(2)).await;
    f.db.create_content_gate("stream", &stream.id, &f.host.0, 2, 0)
        .await
        .expect("create content gate");
    let member = f.member("viewer");
    let stranger = someone("stranger");
    let gate = ViewerGate { entitlements: None, monetization_enabled: false, pg_pool: None, ..f.gate() };
    let source = switch_source_id(&stream.id);

    for (caller, expected_status) in [(&member, None), (&stranger, Some(StatusCode::NOT_FOUND))] {
        let join = outcome(authorize_viewer(&gate, &stream.id, caller).await).await;
        let proxy = outcome(admit_offer(&gate, true, &stream.id, caller, Some(&source)).await).await;
        assert_eq!(proxy, join);
        match expected_status {
            None => assert_eq!(join, Outcome::Admitted),
            Some(status) => assert_eq!(refused(&join).0, status),
        }
    }
}

#[tokio::test]
async fn an_ended_stream_is_gone_on_both() {
    let pool = pool_or_skip!("an_ended_stream_is_gone_on_both");
    let f = fixture(&pool, "ended").await;
    let stream = f.stream(None).await;
    sqlx::query("UPDATE mm_streams SET status = 'ended', ended_at = now() WHERE id = $1")
        .bind(&stream.id)
        .execute(&f.pool)
        .await
        .expect("end the stream");
    let member = f.member("viewer");

    let answer = f.both(&stream, &member).await;
    assert_eq!(refused(&answer), (StatusCode::GONE, "MM_STREAM_ENDED"));
}
