//! The public share-link viewer gates: `mm_api::client::authorize_public_viewer`,
//! behind `POST /streams/{id}/public-join`, its info-only counterpart
//! `authorize_public_stream_info`, behind `POST /streams/{id}/public-view`,
//! and the recording gate `authorize_public_recording`, behind
//! `POST /recordings/{id}/public-view`; plus the fleet viewer proxy's
//! anonymous offer gate `authorize_anonymous_offer`, behind
//! `POST /_mm/fleet/v1/streams/{id}/api/viewers/offer` without a member
//! JWT. Every public join mints a fresh `guest:{grant}:{join}` seat, so
//! two browsers on one link are two seats with two switch viewer ids —
//! the offer gate re-verifies the join-minted viewer token against a live
//! seat.
//! An anonymous caller is admitted only when the target's standing
//! `link_access` is `"public"` AND the share grant they present is live
//! (unrevoked, unexpired), of the matching kind, and targets that stream or
//! recording. Subscription-gated targets refuse anonymous viewers with 402
//! `MM_CONTENT_GATED`; Lightning-priced targets (slice 2) refuse with 402
//! `MM_PAYMENT_REQUIRED` unless the caller presents the access token a
//! settled paywall claim minted for that grant. The member gate
//! `authorize_viewer` is covered unchanged by `viewer_gate_test.rs`.
//!
//! Same harness shape as `viewer_gate_test.rs`: the gate is factored over
//! `ViewerGate`, driven here against a real PostgreSQL (env-gated on
//! `MM_DATABASE_URL` — without it every test skips, which is NOT a pass) with
//! real stream and share-grant rows. No Synapse stub is needed: the public
//! gate never looks up room membership. Every test uses its own room, host
//! and stream, so they run in parallel.

use std::sync::OnceLock;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_api::client::{
    ViewerGate, authorize_anonymous_offer, authorize_public_recording, authorize_public_stream_info,
    authorize_public_viewer, guest_seat_user_id,
};
use mm_api::error::ApiError;
use mm_core::permissions::TierPermissions;
use mm_core::switch_auth::generate_switch_token;
use mm_core::switch_client::switch_viewer_id;
use mm_core::types::{ParticipantRole, RoomId, StreamId, UserId};
use mm_db::models::{Recording, Stream};
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

/// What the gate answered: in (with the stream), or the response a refusal
/// becomes.
#[derive(Debug, PartialEq)]
enum Outcome {
    Admitted(String),
    Refused { status: StatusCode, code: String },
}

async fn outcome(result: Result<Stream, ApiError>) -> Outcome {
    match result {
        Ok(stream) => Outcome::Admitted(stream.id),
        Err(err) => refusal(err).await,
    }
}

async fn recording_outcome(result: Result<Recording, ApiError>) -> Outcome {
    match result {
        Ok(recording) => Outcome::Admitted(recording.id),
        Err(err) => refusal(err).await,
    }
}

async fn refusal(err: ApiError) -> Outcome {
    let response = err.into_response();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the error body");
    let body: Value = serde_json::from_slice(&body).expect("error body is JSON");
    Outcome::Refused {
        status,
        code: body["error"].as_str().unwrap_or_default().to_string(),
    }
}

fn refused(outcome: &Outcome) -> (StatusCode, &str) {
    match outcome {
        Outcome::Refused { status, code } => (*status, code.as_str()),
        Outcome::Admitted(_) => panic!("expected a refusal, the caller was admitted"),
    }
}

struct Fixture {
    pool: PgPool,
    db: PgDatabase,
    entitlements: EntitlementService,
    permissions_cache: moka::future::Cache<(String, String), TierPermissions>,
    room_row: i64,
    host: UserId,
}

async fn fixture(pool: &PgPool, test: &str) -> Fixture {
    let tag = format!("{test}-{}", uuid::Uuid::new_v4().simple());
    let db = PgDatabase::from_pool(pool.clone());
    let room = format!("!room-{tag}:hs");
    let host = UserId(format!("@host-{tag}:hs"));
    let row = db.get_or_create_room(&RoomId(room)).await.expect("create room");

    Fixture {
        pool: pool.clone(),
        db,
        entitlements: EntitlementService::new(pool.clone(), None, None),
        permissions_cache: moka::future::Cache::new(1_000),
        room_row: row.id,
        host,
    }
}

impl Fixture {
    fn gate(&self) -> ViewerGate<'_> {
        ViewerGate {
            db: &self.db,
            // The public gate never calls the homeserver; these are inert.
            http: mm_core::http::shared(),
            homeserver_url: "http://127.0.0.1:9",
            synapse_admin_token: "admin-tok",
            monetization_enabled: true,
            entitlements: Some(&self.entitlements),
            permissions_cache: &self.permissions_cache,
            pg_pool: Some(&self.pool),
            share_pool: &self.pool,
        }
    }

    async fn stream(&self, min_tier_level: Option<i32>) -> Stream {
        self.db
            .create_stream(self.room_row, &self.host, Some("broadcast"), "video", None, min_tier_level)
            .await
            .expect("create stream")
    }

    async fn set_link_access(&self, stream: &Stream, link_access: &str) {
        sqlx::query("UPDATE mm_streams SET link_access = $1 WHERE id = $2")
            .bind(link_access)
            .bind(&stream.id)
            .execute(&self.pool)
            .await
            .expect("set link_access");
    }

    /// A stream whose standing posture is `public`.
    async fn public_stream(&self, min_tier_level: Option<i32>) -> Stream {
        let stream = self.stream(min_tier_level).await;
        self.set_link_access(&stream, "public").await;
        stream
    }

    /// Mint a grant for `stream` expiring `ttl` from now (negative = already
    /// expired). Returns the grant id.
    async fn grant(&self, stream: &Stream, ttl: chrono::Duration) -> String {
        let grant_id = mm_db::share_grants_db::new_share_grant_id();
        mm_db::share_grants_db::insert_share_grant(
            &self.pool,
            &grant_id,
            "stream",
            &stream.id,
            &self.host.0,
            chrono::Utc::now() + ttl,
        )
        .await
        .expect("insert share grant");
        grant_id
    }

    async fn public_outcome(&self, stream: &Stream, grant_id: &str) -> Outcome {
        self.public_outcome_with_token(stream, grant_id, None).await
    }

    async fn public_outcome_with_token(
        &self,
        stream: &Stream,
        grant_id: &str,
        access_token: Option<&str>,
    ) -> Outcome {
        outcome(authorize_public_viewer(&self.gate(), &stream.id, grant_id, access_token).await)
            .await
    }

    async fn public_info_outcome(&self, stream: &Stream, grant_id: &str) -> Outcome {
        outcome(authorize_public_stream_info(&self.gate(), &stream.id, grant_id).await).await
    }

    /// A ready recording hosted by the fixture host, inserted the way
    /// `room_recordings_visibility_test` inserts its rows. Its standing
    /// posture is `members` (`create_recording` does not write
    /// `link_access`, so the column default applies).
    async fn recording(&self, min_tier_level: Option<i32>) -> Recording {
        let id = format!("rec_{}", uuid::Uuid::new_v4());
        let recording = Recording {
            id: id.clone(),
            stream_id: format!("stream-{id}"),
            room_id: self.room_row,
            host_user_id: self.host.0.clone(),
            status: "ready".to_string(),
            media_type: "video".to_string(),
            storage_key: format!("recordings/{id}.mp4"),
            storage_backend: "local".to_string(),
            mxc_url: None,
            cdn_url: Some(format!("https://cdn.example/{id}.mp4")),
            duration_ms: Some(1000),
            size_bytes: Some(2048),
            mime_type: "video/mp4".to_string(),
            sha256: None,
            title: Some("a broadcast".to_string()),
            egress_id: None,
            created_at: chrono::Utc::now(),
            completed_at: None,
            min_tier_level,
            mp4_status: "none".to_string(),
            mp4_key: None,
            link_access: "members".to_string(),
            paywall_price_sats: None,
        };
        self.db
            .create_recording(&recording)
            .await
            .expect("create recording");
        recording
    }

    async fn set_recording_link_access(&self, recording: &Recording, link_access: &str) {
        sqlx::query("UPDATE mm_recordings SET link_access = $1 WHERE id = $2")
            .bind(link_access)
            .bind(&recording.id)
            .execute(&self.pool)
            .await
            .expect("set recording link_access");
    }

    /// A recording whose standing posture is `public`.
    async fn public_recording(&self, min_tier_level: Option<i32>) -> Recording {
        let recording = self.recording(min_tier_level).await;
        self.set_recording_link_access(&recording, "public").await;
        recording
    }

    /// Mint a recording grant for `recording` expiring `ttl` from now.
    async fn recording_grant(&self, recording: &Recording, ttl: chrono::Duration) -> String {
        let grant_id = mm_db::share_grants_db::new_share_grant_id();
        mm_db::share_grants_db::insert_share_grant(
            &self.pool,
            &grant_id,
            "recording",
            &recording.id,
            &self.host.0,
            chrono::Utc::now() + ttl,
        )
        .await
        .expect("insert recording share grant");
        grant_id
    }

    async fn public_recording_outcome(&self, recording: &Recording, grant_id: &str) -> Outcome {
        self.public_recording_outcome_with_token(recording, grant_id, None)
            .await
    }

    async fn public_recording_outcome_with_token(
        &self,
        recording: &Recording,
        grant_id: &str,
        access_token: Option<&str>,
    ) -> Outcome {
        recording_outcome(
            authorize_public_recording(&self.gate(), &recording.id, grant_id, access_token).await,
        )
        .await
    }

    async fn set_stream_paywall_price(&self, stream: &Stream, sats: i64) {
        sqlx::query("UPDATE mm_streams SET paywall_price_sats = $1 WHERE id = $2")
            .bind(sats)
            .bind(&stream.id)
            .execute(&self.pool)
            .await
            .expect("set stream paywall price");
    }

    async fn set_recording_paywall_price(&self, recording: &Recording, sats: i64) {
        sqlx::query("UPDATE mm_recordings SET paywall_price_sats = $1 WHERE id = $2")
            .bind(sats)
            .bind(&recording.id)
            .execute(&self.pool)
            .await
            .expect("set recording paywall price");
    }

    /// A settled paywall claim for (`grant_id`, kind, target), inserted
    /// and settled through the real `paywall_db` functions. Returns the
    /// bearer access token settlement minted (only its hash is stored).
    async fn settled_claim_token(
        &self,
        kind: &str,
        target_id: &str,
        grant_id: &str,
        amount_sats: i64,
    ) -> String {
        let claim_id = mm_db::paywall_db::new_claim_id();
        // A unique 64-hex stand-in payment hash per claim.
        let payment_hash = mm_db::paywall_db::hash_access_token(&claim_id);
        mm_db::paywall_db::insert_claim(
            &self.pool,
            &claim_id,
            grant_id,
            kind,
            target_id,
            &self.host.0,
            amount_sats,
            "lnbc-test-invoice",
            &payment_hash,
            chrono::Utc::now() + chrono::Duration::hours(1),
            true,
        )
        .await
        .expect("insert paywall claim");
        let token = mm_db::paywall_db::new_access_token();
        let rows = mm_db::paywall_db::settle_claim_with_token(
            &self.pool,
            &claim_id,
            &payment_hash,
            &mm_db::paywall_db::hash_access_token(&token),
        )
        .await
        .expect("settle paywall claim");
        assert_eq!(rows, 1, "settlement updates exactly one row");
        token
    }
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
async fn a_public_stream_with_a_live_grant_authorizes_an_anonymous_viewer() {
    let pool = pool_or_skip!("a_public_stream_with_a_live_grant_authorizes_an_anonymous_viewer");
    let f = fixture(&pool, "public-live").await;
    let stream = f.public_stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;

    let answer = f.public_outcome(&stream, &grant_id).await;
    assert_eq!(answer, Outcome::Admitted(stream.id.clone()));

    // A successful redemption stamps last_used_at.
    let grant = mm_db::share_grants_db::grant_by_id(&pool, &grant_id)
        .await
        .expect("read grant")
        .expect("grant exists");
    assert!(grant.last_used_at.is_some(), "redemption was recorded");
}

#[tokio::test]
async fn a_members_stream_refuses_a_grant_holder_with_404() {
    let pool = pool_or_skip!("a_members_stream_refuses_a_grant_holder_with_404");
    let f = fixture(&pool, "members").await;
    // Default posture is "members": even a live grant authorizes no one.
    let stream = f.stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;

    let answer = f.public_outcome(&stream, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
}

#[tokio::test]
async fn an_expired_grant_gets_404() {
    let pool = pool_or_skip!("an_expired_grant_gets_404");
    let f = fixture(&pool, "expired").await;
    let stream = f.public_stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::seconds(-60)).await;

    let answer = f.public_outcome(&stream, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
}

#[tokio::test]
async fn a_revoked_grant_gets_404() {
    let pool = pool_or_skip!("a_revoked_grant_gets_404");
    let f = fixture(&pool, "revoked").await;
    let stream = f.public_stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;
    mm_db::share_grants_db::revoke_share_grant(&pool, &grant_id)
        .await
        .expect("revoke grant");

    let answer = f.public_outcome(&stream, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
}

#[tokio::test]
async fn a_grant_for_another_stream_gets_404() {
    let pool = pool_or_skip!("a_grant_for_another_stream_gets_404");
    let f = fixture(&pool, "other-stream").await;
    let stream = f.public_stream(None).await;
    let other = f.public_stream(None).await;
    // A perfectly live grant — for the other stream.
    let foreign_grant = f.grant(&other, chrono::Duration::hours(1)).await;

    let answer = f.public_outcome(&stream, &foreign_grant).await;
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
    // And the same grant does open its own stream.
    assert_eq!(
        f.public_outcome(&other, &foreign_grant).await,
        Outcome::Admitted(other.id.clone())
    );
}

#[tokio::test]
async fn a_tier_gated_public_stream_refuses_anonymous_viewers_with_402() {
    let pool = pool_or_skip!("a_tier_gated_public_stream_refuses_anonymous_viewers_with_402");
    let f = fixture(&pool, "tier-gated").await;
    let stream = f.public_stream(Some(2)).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;

    // Anonymous viewers have no account to subscribe with.
    let answer = f.public_outcome(&stream, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::PAYMENT_REQUIRED, "MM_CONTENT_GATED"));
}

#[tokio::test]
async fn a_content_gated_public_stream_refuses_anonymous_viewers_with_402() {
    let pool = pool_or_skip!("a_content_gated_public_stream_refuses_anonymous_viewers_with_402");
    let f = fixture(&pool, "content-gated").await;
    // Gated only through the legacy mm_content_gates row, not min_tier_level.
    let stream = f.public_stream(None).await;
    f.db.create_content_gate("stream", &stream.id, &f.host.0, 1, 0)
        .await
        .expect("create content gate");
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;

    let answer = f.public_outcome(&stream, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::PAYMENT_REQUIRED, "MM_CONTENT_GATED"));
}

// ---------------------------------------------------------------------------
// Stream info: `authorize_public_stream_info`, behind
// `POST /streams/{id}/public-view`. Same 404/402 sets as the watch gate,
// but ended streams stay viewable and reading info stamps nothing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_ended_public_stream_is_viewable_via_the_info_gate() {
    let pool = pool_or_skip!("an_ended_public_stream_is_viewable_via_the_info_gate");
    let f = fixture(&pool, "info-ended").await;
    let stream = f.public_stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;
    sqlx::query("UPDATE mm_streams SET status = 'ended', ended_at = now() WHERE id = $1")
        .bind(&stream.id)
        .execute(&pool)
        .await
        .expect("end stream");

    // Info only: the holder still sees the (ended) stream...
    let answer = f.public_info_outcome(&stream, &grant_id).await;
    assert_eq!(answer, Outcome::Admitted(stream.id.clone()));
    // ...while the watch gate still refuses it as gone.
    let watch = f.public_outcome(&stream, &grant_id).await;
    assert_eq!(refused(&watch), (StatusCode::GONE, "MM_STREAM_ENDED"));

    // Reading info is not a redemption: last_used_at stays unstamped.
    let grant = mm_db::share_grants_db::grant_by_id(&pool, &grant_id)
        .await
        .expect("read grant")
        .expect("grant exists");
    assert!(grant.last_used_at.is_none(), "info read must not touch the grant");
}

#[tokio::test]
async fn a_members_stream_refuses_the_info_gate_with_404() {
    let pool = pool_or_skip!("a_members_stream_refuses_the_info_gate_with_404");
    let f = fixture(&pool, "info-members").await;
    // Default posture is "members": even a live grant reveals nothing.
    let stream = f.stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;

    let answer = f.public_info_outcome(&stream, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
}

// ---------------------------------------------------------------------------
// Recordings: `authorize_public_recording`, behind
// `POST /recordings/{recording_id}/public-view`. Mirrors the stream cases
// above; there is no capacity case (a view takes no seat) and no
// ended case (a deleted recording is just another 404).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_public_recording_with_a_live_grant_authorizes_an_anonymous_viewer() {
    let pool = pool_or_skip!("a_public_recording_with_a_live_grant_authorizes_an_anonymous_viewer");
    let f = fixture(&pool, "rec-public-live").await;
    let recording = f.public_recording(None).await;
    let grant_id = f.recording_grant(&recording, chrono::Duration::hours(1)).await;

    let answer = f.public_recording_outcome(&recording, &grant_id).await;
    assert_eq!(answer, Outcome::Admitted(recording.id.clone()));

    // A successful redemption stamps last_used_at.
    let grant = mm_db::share_grants_db::grant_by_id(&pool, &grant_id)
        .await
        .expect("read grant")
        .expect("grant exists");
    assert!(grant.last_used_at.is_some(), "redemption was recorded");
}

#[tokio::test]
async fn a_members_recording_refuses_a_grant_holder_with_404() {
    let pool = pool_or_skip!("a_members_recording_refuses_a_grant_holder_with_404");
    let f = fixture(&pool, "rec-members").await;
    // Default posture is "members": even a live grant authorizes no one.
    let recording = f.recording(None).await;
    let grant_id = f.recording_grant(&recording, chrono::Duration::hours(1)).await;

    let answer = f.public_recording_outcome(&recording, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
}

#[tokio::test]
async fn an_expired_recording_grant_gets_404() {
    let pool = pool_or_skip!("an_expired_recording_grant_gets_404");
    let f = fixture(&pool, "rec-expired").await;
    let recording = f.public_recording(None).await;
    let grant_id = f.recording_grant(&recording, chrono::Duration::seconds(-60)).await;

    let answer = f.public_recording_outcome(&recording, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
}

#[tokio::test]
async fn a_stream_grant_does_not_open_a_recording() {
    let pool = pool_or_skip!("a_stream_grant_does_not_open_a_recording");
    let f = fixture(&pool, "rec-stream-grant").await;
    let recording = f.public_recording(None).await;
    // A perfectly live grant — of the wrong kind. It even names this
    // recording as its target, so only the kind check can refuse it.
    let grant_id = mm_db::share_grants_db::new_share_grant_id();
    mm_db::share_grants_db::insert_share_grant(
        &pool,
        &grant_id,
        "stream",
        &recording.id,
        &f.host.0,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .expect("insert stream-kind grant");

    let answer = f.public_recording_outcome(&recording, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::NOT_FOUND, "MM_NOT_FOUND"));
}

#[tokio::test]
async fn a_tier_gated_public_recording_refuses_anonymous_viewers_with_402() {
    let pool = pool_or_skip!("a_tier_gated_public_recording_refuses_anonymous_viewers_with_402");
    let f = fixture(&pool, "rec-tier-gated").await;
    let recording = f.public_recording(Some(2)).await;
    let grant_id = f.recording_grant(&recording, chrono::Duration::hours(1)).await;

    // Anonymous viewers have no account to subscribe with.
    let answer = f.public_recording_outcome(&recording, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::PAYMENT_REQUIRED, "MM_CONTENT_GATED"));
}

#[tokio::test]
async fn a_content_gated_public_recording_refuses_anonymous_viewers_with_402() {
    let pool = pool_or_skip!("a_content_gated_public_recording_refuses_anonymous_viewers_with_402");
    let f = fixture(&pool, "rec-content-gated").await;
    // Gated only through the legacy mm_content_gates row, not min_tier_level.
    let recording = f.public_recording(None).await;
    f.db.create_content_gate("recording", &recording.id, &f.host.0, 1, 0)
        .await
        .expect("create content gate");
    let grant_id = f.recording_grant(&recording, chrono::Duration::hours(1)).await;

    let answer = f.public_recording_outcome(&recording, &grant_id).await;
    assert_eq!(refused(&answer), (StatusCode::PAYMENT_REQUIRED, "MM_CONTENT_GATED"));
}

// ---------------------------------------------------------------------------
// Lightning paywall (slice 2): a priced target refuses with
// MM_PAYMENT_REQUIRED until the caller presents the access token a
// settled claim minted for that grant + target. Claims are created and
// settled through the real paywall_db functions; invoice creation and
// preimage proof are covered by the bolt11/paywall_db unit tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_priced_public_stream_needs_a_settled_paywall_token() {
    let pool = pool_or_skip!("a_priced_public_stream_needs_a_settled_paywall_token");
    let f = fixture(&pool, "priced-stream").await;
    let stream = f.public_stream(None).await;
    f.set_stream_paywall_price(&stream, 250).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;

    // Info stays readable without paying (the price rides the response,
    // which is how the viewer renders the paywall sheet)...
    let info = f.public_info_outcome(&stream, &grant_id).await;
    assert_eq!(info, Outcome::Admitted(stream.id.clone()));

    // ...but watching needs the token: none, then a bogus one, refuse.
    let unpaid = f.public_outcome(&stream, &grant_id).await;
    assert_eq!(
        refused(&unpaid),
        (StatusCode::PAYMENT_REQUIRED, "MM_PAYMENT_REQUIRED")
    );
    let bogus = f
        .public_outcome_with_token(&stream, &grant_id, Some("not-a-real-token"))
        .await;
    assert_eq!(
        refused(&bogus),
        (StatusCode::PAYMENT_REQUIRED, "MM_PAYMENT_REQUIRED")
    );

    // A claim settled under a different grant for the same stream does
    // not unlock this link.
    let other_grant = f.grant(&stream, chrono::Duration::hours(1)).await;
    let foreign_token = f
        .settled_claim_token("stream", &stream.id, &other_grant, 250)
        .await;
    let foreign = f
        .public_outcome_with_token(&stream, &grant_id, Some(&foreign_token))
        .await;
    assert_eq!(
        refused(&foreign),
        (StatusCode::PAYMENT_REQUIRED, "MM_PAYMENT_REQUIRED")
    );

    // The settled token for this grant admits.
    let token = f
        .settled_claim_token("stream", &stream.id, &grant_id, 250)
        .await;
    let paid = f
        .public_outcome_with_token(&stream, &grant_id, Some(&token))
        .await;
    assert_eq!(paid, Outcome::Admitted(stream.id.clone()));
}

#[tokio::test]
async fn paying_the_lightning_price_does_not_lift_a_subscription_gate() {
    let pool = pool_or_skip!("paying_the_lightning_price_does_not_lift_a_subscription_gate");
    let f = fixture(&pool, "priced-and-tiered").await;
    let stream = f.public_stream(Some(2)).await;
    f.set_stream_paywall_price(&stream, 250).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;
    let token = f
        .settled_claim_token("stream", &stream.id, &grant_id, 250)
        .await;

    // Settled in sats, still not a subscriber: the subscription gate wins.
    let answer = f
        .public_outcome_with_token(&stream, &grant_id, Some(&token))
        .await;
    assert_eq!(
        refused(&answer),
        (StatusCode::PAYMENT_REQUIRED, "MM_CONTENT_GATED")
    );
}

#[tokio::test]
async fn a_priced_public_recording_needs_a_settled_paywall_token() {
    let pool = pool_or_skip!("a_priced_public_recording_needs_a_settled_paywall_token");
    let f = fixture(&pool, "priced-recording").await;
    let recording = f.public_recording(None).await;
    f.set_recording_paywall_price(&recording, 100).await;
    let grant_id = f
        .recording_grant(&recording, chrono::Duration::hours(1))
        .await;

    let unpaid = f.public_recording_outcome(&recording, &grant_id).await;
    assert_eq!(
        refused(&unpaid),
        (StatusCode::PAYMENT_REQUIRED, "MM_PAYMENT_REQUIRED")
    );

    // A claim for the same id but the wrong kind does not unlock it.
    let wrong_kind = f
        .settled_claim_token("stream", &recording.id, &grant_id, 100)
        .await;
    let refused_wrong_kind = f
        .public_recording_outcome_with_token(&recording, &grant_id, Some(&wrong_kind))
        .await;
    assert_eq!(
        refused(&refused_wrong_kind),
        (StatusCode::PAYMENT_REQUIRED, "MM_PAYMENT_REQUIRED")
    );

    let token = f
        .settled_claim_token("recording", &recording.id, &grant_id, 100)
        .await;
    let paid = f
        .public_recording_outcome_with_token(&recording, &grant_id, Some(&token))
        .await;
    assert_eq!(paid, Outcome::Admitted(recording.id.clone()));
}

/// The switch secret the offer gate verifies against. These tests mint
/// their own viewer tokens with it, the way `issue_viewer_join` does.
const SWITCH_SECRET: &str = "test-switch-secret";

/// Seat one browser on `grant_id`'s link the way `public-join` does, and
/// return the seat's user id with the switch viewer id the join would hand
/// out for it.
async fn seat_browser(f: &Fixture, stream: &Stream, grant_id: &str) -> (String, String) {
    let user_id = guest_seat_user_id(grant_id);
    f.db
        .add_participant(
            &StreamId(stream.id.clone()),
            &user_id,
            ParticipantRole::Viewer,
            None,
        )
        .await
        .expect("seat the browser");
    let viewer_id = switch_viewer_id(&stream.id, &user_id.0);
    (user_id.0, viewer_id)
}

fn viewer_token(viewer_id: &str) -> String {
    generate_switch_token(SWITCH_SECRET, "viewer", viewer_id, 300)
}

/// The offer gate's answer: the viewer id it admitted, or the refusal's
/// status and code.
async fn offer(
    f: &Fixture,
    stream: &Stream,
    grant_id: &str,
    access_token: Option<&str>,
    token: &str,
) -> Result<String, (StatusCode, String)> {
    match authorize_anonymous_offer(
        &f.gate(),
        &stream.id,
        grant_id,
        access_token,
        token,
        SWITCH_SECRET,
        None,
    )
    .await
    {
        Ok((_, viewer_id)) => Ok(viewer_id),
        Err(err) => match refusal(err).await {
            Outcome::Refused { status, code } => Err((status, code)),
            Outcome::Admitted(_) => unreachable!("a refusal never admits"),
        },
    }
}

#[tokio::test]
async fn two_browsers_on_one_grant_take_two_seats_with_distinct_viewer_ids() {
    let pool = pool_or_skip!("two_browsers_on_one_grant_take_two_seats_with_distinct_viewer_ids");
    let f = fixture(&pool, "two-browsers").await;
    let stream = f.public_stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;

    // Both joins pass the public gate (the second is a new seat, not a
    // re-join of the first).
    for _ in 0..2 {
        assert_eq!(
            f.public_outcome(&stream, &grant_id).await,
            Outcome::Admitted(stream.id.clone())
        );
    }

    let (user_a, vid_a) = seat_browser(&f, &stream, &grant_id).await;
    let (user_b, vid_b) = seat_browser(&f, &stream, &grant_id).await;
    assert_ne!(user_a, user_b, "each browser mints its own seat");
    assert_ne!(vid_a, vid_b, "each seat derives its own switch viewer");

    // And each browser's join-minted token opens the proxy for its own seat.
    assert_eq!(
        offer(&f, &stream, &grant_id, None, &viewer_token(&vid_a)).await,
        Ok(vid_a)
    );
    assert_eq!(
        offer(&f, &stream, &grant_id, None, &viewer_token(&vid_b)).await,
        Ok(vid_b)
    );
}

#[tokio::test]
async fn anonymous_offer_with_a_wrong_tampered_or_seatless_token_gets_401() {
    let pool = pool_or_skip!("anonymous_offer_with_a_wrong_tampered_or_seatless_token_gets_401");
    let f = fixture(&pool, "bad-token").await;
    let stream = f.public_stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;
    let (_, vid) = seat_browser(&f, &stream, &grant_id).await;
    let good = viewer_token(&vid);

    let mut bad = good.clone();
    let first = bad.remove(0);
    bad.insert(0, if first == 'A' { 'B' } else { 'A' });
    let seatless_vid = switch_viewer_id(&stream.id, &format!("guest:{grant_id}:nobody"));
    for token in [
        // Signed with the wrong secret.
        generate_switch_token("wrong-secret", "viewer", &vid, 300),
        // Right secret, wrong role.
        generate_switch_token(SWITCH_SECRET, "publisher", &vid, 300),
        // Tampered payload: the signature no longer matches.
        bad,
        // Well-formed and well-signed, but no live seat behind it.
        viewer_token(&seatless_vid),
        // Not a token at all.
        "not-a-token".to_string(),
    ] {
        assert_eq!(
            offer(&f, &stream, &grant_id, None, &token).await,
            Err((StatusCode::UNAUTHORIZED, "MM_INVALID_TOKEN".to_string())),
            "token {token:?} must not open the proxy",
        );
    }
}

#[tokio::test]
async fn anonymous_offer_with_a_cross_stream_token_gets_401() {
    let pool = pool_or_skip!("anonymous_offer_with_a_cross_stream_token_gets_401");
    let f = fixture(&pool, "cross-stream").await;
    let stream = f.public_stream(None).await;
    let other = f.public_stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;
    // A token minted for the other stream's viewer namespace. (Its own
    // stream would admit it — the subject is bound to the path stream.)
    let foreign_vid = switch_viewer_id(&other.id, &format!("guest:{grant_id}:x"));

    assert_eq!(
        offer(&f, &stream, &grant_id, None, &viewer_token(&foreign_vid)).await,
        Err((StatusCode::UNAUTHORIZED, "MM_INVALID_TOKEN".to_string())),
    );
}

#[tokio::test]
async fn revoked_or_expired_grant_refuses_the_offer_with_404() {
    let pool = pool_or_skip!("revoked_or_expired_grant_refuses_the_offer_with_404");
    let f = fixture(&pool, "dead-grant-offer").await;
    let stream = f.public_stream(None).await;

    // Revoked: the seat and token are genuine, the link is dead.
    let revoked = f.grant(&stream, chrono::Duration::hours(1)).await;
    let (_, revoked_vid) = seat_browser(&f, &stream, &revoked).await;
    let revoked_token = viewer_token(&revoked_vid);
    mm_db::share_grants_db::revoke_share_grant(&pool, &revoked)
        .await
        .expect("revoke grant");
    assert_eq!(
        offer(&f, &stream, &revoked, None, &revoked_token).await,
        Err((StatusCode::NOT_FOUND, "MM_NOT_FOUND".to_string())),
    );

    // Expired: same, the link outlived its window.
    let expired = f.grant(&stream, chrono::Duration::seconds(-60)).await;
    let (_, expired_vid) = seat_browser(&f, &stream, &expired).await;
    assert_eq!(
        offer(&f, &stream, &expired, None, &viewer_token(&expired_vid)).await,
        Err((StatusCode::NOT_FOUND, "MM_NOT_FOUND".to_string())),
    );
}

#[tokio::test]
async fn priced_link_needs_the_access_token_at_offer_time_too() {
    let pool = pool_or_skip!("priced_link_needs_the_access_token_at_offer_time_too");
    let f = fixture(&pool, "priced-offer").await;
    let stream = f.public_stream(None).await;
    f.set_stream_paywall_price(&stream, 100).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;
    let (_, vid) = seat_browser(&f, &stream, &grant_id).await;
    let token = viewer_token(&vid);

    // The offer re-runs the paywall: no proof, no admission.
    assert_eq!(
        offer(&f, &stream, &grant_id, None, &token).await,
        Err((StatusCode::PAYMENT_REQUIRED, "MM_PAYMENT_REQUIRED".to_string())),
    );
    // A settled claim's bearer token admits, exactly as at join.
    let access = f.settled_claim_token("stream", &stream.id, &grant_id, 100).await;
    assert_eq!(
        offer(&f, &stream, &grant_id, Some(&access), &token).await,
        Ok(vid)
    );
}

#[tokio::test]
async fn legacy_bare_guest_seat_passes_the_offer() {
    let pool = pool_or_skip!("legacy_bare_guest_seat_passes_the_offer");
    let f = fixture(&pool, "bare-seat").await;
    let stream = f.public_stream(None).await;
    let grant_id = f.grant(&stream, chrono::Duration::hours(1)).await;

    // A seat taken before per-join identity existed: the user id is the
    // bare grant, and its token still opens the proxy.
    let bare = UserId(format!("guest:{grant_id}"));
    f.db
        .add_participant(
            &StreamId(stream.id.clone()),
            &bare,
            ParticipantRole::Viewer,
            None,
        )
        .await
        .expect("seat the legacy browser");
    let vid = switch_viewer_id(&stream.id, &bare.0);
    assert_eq!(
        offer(&f, &stream, &grant_id, None, &viewer_token(&vid)).await,
        Ok(vid)
    );
}
