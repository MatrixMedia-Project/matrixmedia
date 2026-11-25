//! Legacy wire contract (ADR-0011 section 5): what the apps already in the
//! stores depend on, pinned so that no server change can take it away.
//!
//! Each test runs the real write path (stream start, resume, end, the E2EE key,
//! recording and donation) against a recording stub homeserver and asserts
//! three things:
//!
//! 1. the event goes out under its **legacy** name, sent as the bot;
//! 2. the payload validates against the **frozen legacy schema** in
//!    `contracts/events/` (`com.matrixmedia.*.json`,
//!    `com.steegler.matrixmedia.feed.*.json`);
//! 3. the event ids mm-core stores (`state_event_id`, `ended_event_id`,
//!    `feed_started_event_id`) point at the legacy events.
//!
//! The bot's notice texts, which both apps parse, are pinned byte for byte.
//!
//! The database is an in-memory SQLite (no external service), so these tests
//! run everywhere. The SQLite backend does not store `feed_started_event_id`
//! and has no recording rows; the two `pg_*` tests cover those against
//! Postgres and skip without `MM_DATABASE_URL` (fail with `MM_REQUIRE_DB`).
//!
//! Changing an assertion here changes what shipped apps are promised. Don't,
//! unless the legacy contract is being retired on purpose.

#[path = "support/json_schema.rs"]
mod json_schema;
#[path = "support/stub_homeserver.rs"]
mod stub_homeserver;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use json_schema::assert_valid;
use mm_api::stream_lifecycle::{
    EndContext, MarkerContext, RecordingRelease, end_and_finalise_stream, finalize_stream_marker,
    republish_active_marker,
};
use mm_core::config::Config;
use mm_core::e2ee::E2eeStreamInfo;
use mm_core::metrics::Metrics;
use mm_core::types::{RoomId, StreamId, UserId};
use mm_db::Database;
use mm_db::models::Stream;
use mm_db::sqlite::SqliteDatabase;
use mm_matrix::client::HomeserverClient;
use mm_matrix::events::{self, DonationEventContent, E2eeKeyEvent, StreamEventContent};
use mm_sfu::{
    CreateRoomRequest, EgressInfo, ParticipantInfo, ParticipantPermissions, RoomStats, SfuAdapter,
    SfuError, SfuRoom, SfuToken,
};
use stub_homeserver::StubHomeserver;

const BOT: &str = "@mmbot:example.org";
const HOST: &str = "@alice:example.org";
const PUBLIC_URL: &str = "https://mm.example.org";

const LEGACY_STREAM: &str = "com.matrixmedia.stream";
const LEGACY_E2EE_KEY: &str = "com.matrixmedia.stream.e2ee_key";
const LEGACY_DONATION: &str = "com.matrixmedia.donation";
const LEGACY_FEED_STARTED: &str = "com.steegler.matrixmedia.feed.broadcast.started";
const LEGACY_FEED_ENDED: &str = "com.steegler.matrixmedia.feed.broadcast.ended";
const LEGACY_FEED_RECORDING: &str = "com.steegler.matrixmedia.feed.recording.available";
const LEGACY_FEED_POST: &str = "com.steegler.matrixmedia.feed.post";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct NoSfu;

#[async_trait::async_trait]
impl SfuAdapter for NoSfu {
    fn name(&self) -> &str {
        "none"
    }
    async fn health_check(&self) -> Result<(), SfuError> {
        Ok(())
    }
    async fn create_room(&self, req: CreateRoomRequest) -> Result<SfuRoom, SfuError> {
        Ok(SfuRoom {
            sfu_room_id: req.name.clone(),
            name: req.name,
            num_participants: 0,
        })
    }
    async fn delete_room(&self, _sfu_room_id: &str) -> Result<(), SfuError> {
        Ok(())
    }
    fn supports_egress(&self) -> bool {
        false
    }
    async fn list_egresses(&self, _room_name: &str) -> Result<Vec<EgressInfo>, SfuError> {
        Ok(Vec::new())
    }
    async fn stop_egress(&self, _egress_id: &str) -> Result<(), SfuError> {
        Ok(())
    }
    async fn generate_token(
        &self,
        _room: &SfuRoom,
        _participant: &ParticipantInfo,
        _permissions: ParticipantPermissions,
    ) -> Result<SfuToken, SfuError> {
        Ok(SfuToken {
            token: "t".into(),
            url: "ws://none".into(),
        })
    }
    async fn remove_participant(&self, _r: &str, _p: &str) -> Result<(), SfuError> {
        Ok(())
    }
    async fn list_participants(&self, _r: &str) -> Result<Vec<ParticipantInfo>, SfuError> {
        Ok(Vec::new())
    }
    async fn room_stats(&self, sfu_room_id: &str) -> Result<RoomStats, SfuError> {
        Ok(RoomStats {
            sfu_room_id: sfu_room_id.to_string(),
            num_participants: 0,
            num_publishers: 0,
            num_subscribers: 0,
        })
    }
}

struct Harness {
    stub: Arc<StubHomeserver>,
    client: HomeserverClient,
    config: Config,
    metrics: Metrics,
    db: Box<dyn Database>,
    room_id: String,
}

fn test_config(homeserver_url: &str) -> Config {
    let mut config = Config::default();
    config.matrix.homeserver_url = homeserver_url.to_string();
    config.matrix.server_name = "example.org".to_string();
    config.matrix.bot_localpart = "mmbot".to_string();
    config.matrix.synapse_admin_token = "test-synapse-admin".to_string();
    config.matrix.as_token = "test-as-token".to_string();
    config.server.public_url = Some(PUBLIC_URL.to_string());
    config
}

async fn sqlite_db() -> SqliteDatabase {
    let url = format!(
        "sqlite:file:wire_{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4()
    );
    let db = SqliteDatabase::new(&url).await.expect("sqlite");
    db.migrate().await.expect("sqlite migrations");
    db
}

impl Harness {
    async fn new() -> Self {
        Self::with_db(Box::new(sqlite_db().await)).await
    }

    async fn with_db(db: Box<dyn Database>) -> Self {
        let (stub, url) = stub_homeserver::spawn().await;
        let client = HomeserverClient::new(url.clone(), "test-as-token".into(), BOT.into());
        Self {
            stub,
            client,
            config: test_config(&url),
            metrics: Metrics::new(),
            db,
            room_id: format!("!wire-{}:example.org", uuid::Uuid::new_v4().simple()),
        }
    }

    fn ctx(&self) -> MarkerContext<'_> {
        MarkerContext {
            hs_client: &self.client,
            db: self.db.as_ref(),
            matrix: &self.config.matrix,
            metrics: &self.metrics,
        }
    }

    async fn seed_stream(&self, media_type: &str) -> Stream {
        let room = self
            .db
            .get_or_create_room(&RoomId(self.room_id.clone()))
            .await
            .expect("room");
        self.db
            .create_stream(
                room.id,
                &UserId(HOST.to_string()),
                Some("Morning Show"),
                media_type,
                Some("sfu-wire"),
                None,
            )
            .await
            .expect("stream")
    }

    async fn stored(&self, stream: &Stream) -> Stream {
        self.db
            .get_stream(&StreamId(stream.id.clone()))
            .await
            .expect("read stream")
            .expect("stream row")
    }

    async fn start(
        &self,
        stream: &Stream,
        e2ee: &Option<E2eeStreamInfo>,
        source: Option<&str>,
    ) -> String {
        mm_api::client::publish_stream_started_with(
            &self.ctx(),
            &self.config,
            stream,
            &self.room_id,
            stream.title.as_deref(),
            &stream.media_type,
            stream.media_type != "audio",
            e2ee,
            source,
        )
        .await
    }
}

fn e2ee_info() -> Option<E2eeStreamInfo> {
    Some(E2eeStreamInfo {
        enabled: true,
        algorithm: "aes-gcm-256".into(),
        key_id: "0011223344556677".into(),
        key_generation: 1,
        key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
    })
}

/// Every MatrixMedia event and notice goes out as the bot. (The power-levels
/// promotion in `ensure_bot_in_room` is made with the host's own token.)
#[track_caller]
fn assert_sent_as_bot(stub: &StubHomeserver) {
    for w in stub.writes() {
        if w.state_target()
            .is_some_and(|(t, _)| t == "m.room.power_levels")
        {
            continue;
        }
        assert_eq!(
            w.as_user().as_deref(),
            Some(BOT),
            "write not sent as the bot: {w:?}"
        );
    }
}

/// Index of the first write of `event_type` (state or timeline).
fn first_write_index(stub: &StubHomeserver, event_type: &str) -> usize {
    stub.writes()
        .iter()
        .position(|w| {
            w.state_target().is_some_and(|(t, _)| t == event_type)
                || w.send_type().as_deref() == Some(event_type)
        })
        .unwrap_or_else(|| panic!("no write of {event_type}"))
}

// ---------------------------------------------------------------------------
// The schemas themselves
// ---------------------------------------------------------------------------

/// Every contract's own examples validate against it, so a schema that
/// rejects its own documented payload is caught here and not in an app.
#[test]
fn every_event_contract_validates_its_own_examples() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/events");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("contracts/events") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap().to_string();
        let schema = json_schema::load_event_schema(&name);
        for (i, example) in schema["examples"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Err(errors) = json_schema::validate(&schema, example) {
                panic!("{name}.json example {i} does not validate: {errors:?}");
            }
            checked += 1;
        }
    }
    assert!(checked >= 10, "expected examples to check, found {checked}");
}

// ---------------------------------------------------------------------------
// Stream start (POST /streams and the ingest-key path share this function)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_start_writes_the_legacy_stream_state_and_feed_started() {
    let h = Harness::new().await;
    let stream = h.seed_stream("video").await;

    let returned = h.start(&stream, &e2ee_info(), None).await;

    // 1. Legacy stream state, state key "".
    let puts = h.stub.state_puts(LEGACY_STREAM);
    assert!(
        !puts.is_empty(),
        "no {LEGACY_STREAM} state written at start"
    );
    let legacy = &puts[0];
    assert_eq!(
        legacy.state_target().unwrap().1,
        "",
        "state key must be empty"
    );
    assert_valid(LEGACY_STREAM, &legacy.body);
    assert_eq!(legacy.body["status"], "active");
    assert_eq!(legacy.body["stream_id"], stream.id.as_str());
    assert_eq!(legacy.body["host_user_id"], HOST);
    assert_eq!(legacy.body["marker_generation"], 1);
    assert_eq!(legacy.body["mm_server_url"], PUBLIC_URL);
    assert_eq!(
        legacy.body["viewer_url"],
        format!("{PUBLIC_URL}/view/{}", stream.id)
    );
    assert_eq!(legacy.body["e2ee_enabled"], true);
    assert!(
        legacy.body.get("source").is_none(),
        "app-published stream carries no source"
    );

    // 2. Legacy feed.broadcast.started, after the state event.
    let sends = h.stub.sends(LEGACY_FEED_STARTED);
    assert_eq!(sends.len(), 1, "exactly one broadcast.started: {sends:?}");
    assert_valid(LEGACY_FEED_STARTED, &sends[0].body);
    assert_eq!(
        sends[0].body["body"],
        "\u{1f4fa} @alice:example.org started a broadcast: Morning Show"
    );
    assert!(
        first_write_index(&h.stub, LEGACY_STREAM) < first_write_index(&h.stub, LEGACY_FEED_STARTED)
    );

    // 3. The returned and stored state_event_id is the legacy event's id.
    let legacy_id = legacy.event_id.clone().expect("legacy write answered");
    assert_eq!(
        returned, legacy_id,
        "publish_stream_started returns the legacy id"
    );
    assert_eq!(
        h.stored(&stream).await.state_event_id.as_deref(),
        Some(legacy_id.as_str())
    );

    assert_sent_as_bot(&h.stub);
}

#[tokio::test]
async fn an_ingest_started_stream_writes_the_same_legacy_events_with_source() {
    let h = Harness::new().await;
    let stream = h.seed_stream("video").await;

    let returned = h.start(&stream, &None, Some("whip")).await;

    let puts = h.stub.state_puts(LEGACY_STREAM);
    let legacy = puts.first().expect("legacy stream state");
    assert_valid(LEGACY_STREAM, &legacy.body);
    assert_eq!(legacy.body["source"], "whip");
    assert_eq!(Some(returned), legacy.event_id);
    assert_eq!(h.stub.sends(LEGACY_FEED_STARTED).len(), 1);
    assert_sent_as_bot(&h.stub);
}

// ---------------------------------------------------------------------------
// E2EE key (stream start and rotation share this function)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_e2ee_key_is_written_under_the_legacy_name_keyed_by_stream() {
    let h = Harness::new().await;
    let stream = h.seed_stream("video").await;
    let key = E2eeKeyEvent {
        stream_id: stream.id.clone(),
        algorithm: "aes-gcm-256".into(),
        key_id: "0011223344556677".into(),
        key_generation: 2,
        key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        rotated_at_ms: 1_765_000_000_000,
        rotates_next_ms: Some(1_765_000_600_000),
    };

    let id = mm_api::matrix_writes::publish_stream_e2ee_key(&h.ctx(), &h.room_id, &key)
        .await
        .expect("key published");

    let puts = h.stub.state_puts(LEGACY_E2EE_KEY);
    let legacy = puts.first().expect("legacy key state");
    assert_eq!(
        legacy.state_target().unwrap().1,
        stream.id,
        "state key is the stream id"
    );
    assert_valid(LEGACY_E2EE_KEY, &legacy.body);
    assert_eq!(legacy.body["key_generation"], 2);
    assert_eq!(
        Some(id),
        legacy.event_id,
        "the legacy write's id is returned"
    );
    assert_sent_as_bot(&h.stub);
}

// ---------------------------------------------------------------------------
// Host resume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_republishes_the_legacy_active_marker() {
    let h = Harness::new().await;
    let stream = h.seed_stream("audio").await;
    h.start(&stream, &None, None).await;
    let before = h.stub.state_puts(LEGACY_STREAM).len();

    let content = StreamEventContent {
        stream_id: stream.id.clone(),
        status: "active".into(),
        host_user_id: HOST.into(),
        title: stream.title.clone(),
        media_type: "audio".into(),
        video_config: None,
        viewer_url: Some(format!("{PUBLIC_URL}/view/{}", stream.id)),
        mm_server_url: Some(PUBLIC_URL.into()),
        mm_matrix_server: Some("example.org".into()),
        federation_enabled: Some(false),
        participant_count: 1,
        e2ee_enabled: None,
        e2ee_algorithm: None,
        e2ee_key_id: None,
        e2ee_key_generation: None,
        started_at_ms: 0,
        updated_at_ms: 0,
        marker_generation: 1,
        source: None,
    };
    let (event_id, generation) = republish_active_marker(&h.ctx(), &stream, &h.room_id, content)
        .await
        .expect("republished");

    let puts = h.stub.state_puts(LEGACY_STREAM);
    let resumed = puts
        .get(before)
        .expect("a new legacy stream state write on resume");
    assert_valid(LEGACY_STREAM, &resumed.body);
    assert_eq!(resumed.body["status"], "active");
    assert_eq!(resumed.body["marker_generation"], 2);
    assert_eq!(generation, 2);
    assert_eq!(
        Some(event_id.clone()),
        resumed.event_id,
        "resume returns the legacy id"
    );
    assert_eq!(
        h.stored(&stream).await.state_event_id.as_deref(),
        Some(event_id.as_str())
    );
    assert_sent_as_bot(&h.stub);
}

// ---------------------------------------------------------------------------
// Stream end (host end, sweep, admin and moderation all run this path)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn end_writes_the_legacy_terminal_marker_clears_the_key_and_sends_feed_ended() {
    let h = Harness::new().await;
    let stream = h.seed_stream("video").await;
    h.start(&stream, &e2ee_info(), None).await;
    let before = h.stub.state_puts(LEGACY_STREAM).len();

    // The row as the end path sees it: E2EE on, the started feed event known.
    let mut row = h.stored(&stream).await;
    row.e2ee_enabled = true;
    row.feed_started_event_id = Some("$started:stub".into());

    let sfu = NoSfu;
    let ctx = EndContext {
        marker: h.ctx(),
        sfu: &sfu,
        switch: None,
        pg_pool: None,
        public_url: PUBLIC_URL,
        switch_call_timeout: Duration::from_secs(1),
    };
    let outcome = end_and_finalise_stream(&ctx, &row, RecordingRelease::Publish)
        .await
        .expect("end");
    assert!(outcome.ended_now && outcome.marker_written, "{outcome:?}");

    // Key cleared under the legacy name, with {} — before the terminal marker.
    let clears = h.stub.state_puts(LEGACY_E2EE_KEY);
    let clear = clears.first().expect("legacy key clear");
    assert_eq!(clear.state_target().unwrap().1, stream.id);
    assert_eq!(clear.body, json!({}));
    assert_valid(LEGACY_E2EE_KEY, &clear.body);

    // Terminal marker under the legacy name.
    let puts = h.stub.state_puts(LEGACY_STREAM);
    let terminal = puts.get(before).expect("legacy terminal marker");
    assert_valid(LEGACY_STREAM, &terminal.body);
    assert_eq!(terminal.body["status"], "ended");
    assert_eq!(terminal.body["stream_id"], stream.id.as_str());
    assert_eq!(terminal.body["marker_generation"], 2);
    assert!(
        terminal.body.get("reason").is_none(),
        "no reason unless one is given"
    );
    let clear_at = h
        .stub
        .writes()
        .iter()
        .position(|w| w.path == clear.path)
        .unwrap();
    let terminal_at = h
        .stub
        .writes()
        .iter()
        .position(|w| w.event_id.is_some() && w.event_id == terminal.event_id)
        .unwrap();
    assert!(
        clear_at < terminal_at,
        "the key is cleared before the terminal marker"
    );

    // ended_event_id points at the legacy terminal event.
    let stored = h.stored(&stream).await;
    assert_eq!(stored.ended_event_id, terminal.event_id);
    assert_eq!(stored.status, "ended");

    // feed.broadcast.ended under the legacy name, related to the started event.
    let ended = h.stub.sends(LEGACY_FEED_ENDED);
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_valid(LEGACY_FEED_ENDED, &ended[0].body);
    assert_eq!(
        ended[0].body["body"],
        "@alice:example.org's broadcast ended"
    );
    assert_eq!(ended[0].body["m.relates_to"]["rel_type"], "m.reference");
    assert_eq!(ended[0].body["m.relates_to"]["event_id"], "$started:stub");

    assert_eq!(h.metrics.stream_terminal_events_total.get(), 1);
    assert_sent_as_bot(&h.stub);
}

#[tokio::test]
async fn an_end_with_a_reason_still_validates_against_the_legacy_schema() {
    let h = Harness::new().await;
    let stream = h.seed_stream("audio").await;
    h.start(&stream, &None, None).await;
    let before = h.stub.state_puts(LEGACY_STREAM).len();

    let id = mm_api::stream_lifecycle::finalize_stream_marker_with_reason(
        &h.ctx(),
        &stream,
        &h.room_id,
        Some("This broadcast has been ended because the balance ran out."),
    )
    .await
    .expect("terminal written");

    let puts = h.stub.state_puts(LEGACY_STREAM);
    let terminal = puts.get(before).expect("terminal");
    assert_eq!(Some(id), terminal.event_id);
    assert_eq!(
        terminal.body["reason"],
        "This broadcast has been ended because the balance ran out."
    );
    assert_valid(LEGACY_STREAM, &terminal.body);
}

#[tokio::test]
async fn a_failed_legacy_terminal_write_is_retried_and_its_id_stored() {
    let h = Harness::new().await;
    let stream = h.seed_stream("audio").await;
    h.start(&stream, &None, None).await;
    h.stub.fail_times(&format!("/state/{LEGACY_STREAM}/"), 1);

    let id = finalize_stream_marker(&h.ctx(), &stream, &h.room_id)
        .await
        .expect("written after a retry");
    let terminals: Vec<_> = h
        .stub
        .state_puts(LEGACY_STREAM)
        .into_iter()
        .filter(|r| r.body["status"] == "ended")
        .collect();
    assert_eq!(terminals.len(), 2, "one failure, one retry: {terminals:?}");
    assert_eq!(terminals[1].event_id.as_deref(), Some(id.as_str()));
    assert_eq!(
        h.stored(&stream).await.ended_event_id.as_deref(),
        Some(id.as_str())
    );
}

// ---------------------------------------------------------------------------
// Recording, donation, feed mute
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recording_available_goes_out_under_the_legacy_name() {
    let h = Harness::new().await;
    // The end path builds and sends it exactly like this for each ready recording
    // (`announce_ready_recordings`); the Postgres test below runs that path itself.
    let content = events::build_feed_recording_available(
        "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "rec-1",
        HOST,
        Some("Morning Show"),
        360_000,
        Some(format!("{PUBLIC_URL}/_mm/recordings/rec-1.jpg")),
        None,
        None,
    );
    // `announce_ready_recordings` (the end path) sends each one through this
    // function; it picks the name from the settings.
    mm_api::matrix_writes::emit_feed_recording_available(&h.ctx(), &h.room_id, &content)
        .await
        .expect("sent");
    let sends = h.stub.sends(LEGACY_FEED_RECORDING);
    assert_eq!(sends.len(), 1);
    assert_valid(LEGACY_FEED_RECORDING, &sends[0].body);
    assert_eq!(
        sends[0].body["body"],
        "\u{1f3ac} New recording: Morning Show (6m 0s)"
    );
    assert_sent_as_bot(&h.stub);
}

#[tokio::test]
async fn a_donation_goes_out_under_the_legacy_name_with_its_notice() {
    let h = Harness::new().await;
    let content = DonationEventContent {
        donation_id: "b2c3d4e5-f6a7-8901-bcde-f12345678901".into(),
        stream_id: "a1b2c3d4-e5f6-7890-abcd-ef1234567890".into(),
        donor_display_name: "Bob".into(),
        amount_cents: 500,
        currency: "usd".into(),
        message: Some("Great stream!".into()),
        tier: "yellow".into(),
        pin_duration_secs: 90,
        color: "#FFEB3B".into(),
        version: 1,
    };
    let (donation_id, notice_id) =
        mm_api::matrix_writes::emit_donation(&h.ctx(), &h.room_id, &content)
            .await
            .expect("donation sent");

    let sends = h.stub.sends(LEGACY_DONATION);
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert_valid(LEGACY_DONATION, &sends[0].body);
    assert_eq!(Some(donation_id), sends[0].event_id);

    let notices = h.stub.sends("m.room.message");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body["msgtype"], "m.notice");
    assert_eq!(
        notices[0].body["body"],
        "$5.00 donation from Bob: 'Great stream!'"
    );
    assert_eq!(Some(notice_id), notices[0].event_id);
    assert!(
        first_write_index(&h.stub, LEGACY_DONATION) < first_write_index(&h.stub, "m.room.message")
    );
    assert_sent_as_bot(&h.stub);
}

/// A Lightning donation is sent with `currency: "sats"` and an approximate
/// cent amount with no floor (`monetization.rs`, LNbits webhook). The donation
/// schema described card donations only; it was corrected to the real wire.
#[tokio::test]
async fn a_lightning_donation_validates_against_the_legacy_schema() {
    let h = Harness::new().await;
    let content = DonationEventContent {
        donation_id: "b2c3d4e5-f6a7-8901-bcde-f12345678901".into(),
        stream_id: "a1b2c3d4-e5f6-7890-abcd-ef1234567890".into(),
        donor_display_name: "@bob:example.org".into(),
        amount_cents: 250,
        currency: "sats".into(),
        message: None,
        tier: "blue".into(),
        pin_duration_secs: 30,
        color: "#1E88E5".into(),
        version: 1,
    };
    mm_api::matrix_writes::emit_donation(&h.ctx(), &h.room_id, &content)
        .await
        .expect("donation sent");
    assert_valid(LEGACY_DONATION, &h.stub.sends(LEGACY_DONATION)[0].body);
    assert_eq!(
        h.stub.sends("m.room.message")[0].body["body"],
        "$2.50 donation from @bob:example.org"
    );
    // A small tip converts to fewer than 100 cents.
    let tiny = DonationEventContent {
        amount_cents: 7,
        ..content
    };
    mm_api::matrix_writes::emit_donation(&h.ctx(), &h.room_id, &tiny)
        .await
        .expect("donation sent");
    assert_valid(LEGACY_DONATION, &h.stub.sends(LEGACY_DONATION)[1].body);
}

#[tokio::test]
async fn get_feed_honours_the_legacy_feed_muted_account_data() {
    let (stub, url) = stub_homeserver::spawn().await;
    stub.answer_get(
        "/account_data/com.steegler.matrixmedia.feed_muted",
        StatusCode::OK,
        json!({ "rooms": ["!muted:example.org"] }),
    );
    let rooms =
        mm_api::feed::fetch_muted_rooms_from(&url, "test-as-token", "@carol:example.org").await;
    assert_eq!(rooms, vec!["!muted:example.org".to_string()]);
}

// ---------------------------------------------------------------------------
// The bot's notice texts (both apps parse them)
// ---------------------------------------------------------------------------

#[test]
fn the_bot_notice_texts_are_frozen() {
    assert_eq!(
        events::format_stream_started(HOST, Some("Jam"), "https://mm.example.org/view/s1"),
        "@alice:example.org started streaming: Jam\nJoin: https://mm.example.org/view/s1"
    );
    assert_eq!(
        events::format_stream_started(HOST, None, "https://mm.example.org/view/s1"),
        "@alice:example.org started streaming\nJoin: https://mm.example.org/view/s1"
    );
    assert_eq!(
        events::format_stream_ended(HOST, 125, 7),
        "Stream ended (hosted by @alice:example.org, 2m 5s, peak 7 viewers)"
    );
    assert_eq!(
        events::stream_started_notice(Some("Jam"), Some("https://x/v")),
        "Live stream started: Jam\nJoin: https://x/v"
    );
    assert_eq!(
        events::stream_ended_notice(None),
        "Stream ended: Untitled stream"
    );

    let donation = |currency: &str, cents: i64, message: Option<&str>| DonationEventContent {
        donation_id: String::new(),
        stream_id: String::new(),
        donor_display_name: "Bob".into(),
        amount_cents: cents,
        currency: currency.into(),
        message: message.map(str::to_string),
        tier: "blue".into(),
        pin_duration_secs: 30,
        color: "#000000".into(),
        version: 1,
    };
    assert_eq!(
        events::format_donation_notice(&donation("usd", 500, Some("Hi"))),
        "$5.00 donation from Bob: 'Hi'"
    );
    assert_eq!(
        events::format_donation_notice(&donation("eur", 1050, None)),
        "\u{20ac}10.50 donation from Bob"
    );
    assert_eq!(
        events::format_donation_notice(&donation("gbp", 99, Some(""))),
        "\u{a3}0.99 donation from Bob"
    );

    // Feed event bodies: Android hides 📺 / 🎬 bodies and "broadcast ended".
    assert_eq!(
        events::build_feed_broadcast_started("s", HOST, None, 0).body,
        "\u{1f4fa} @alice:example.org started a broadcast"
    );
    assert_eq!(
        events::build_feed_broadcast_ended("s", HOST, 0, 0, None).body,
        "@alice:example.org's broadcast ended"
    );
    assert_eq!(
        events::build_feed_recording_available("s", "r", HOST, None, 3_723_000, None, None, None)
            .body,
        "\u{1f3ac} New recording (1h 2m)"
    );
    assert_eq!(
        events::build_feed_recording_available("s", "r", HOST, None, 45_000, None, None, None).body,
        "\u{1f3ac} New recording (45s)"
    );
}

#[tokio::test]
async fn the_stream_ended_notice_is_sent_as_an_m_notice_by_the_bot() {
    let (stub, url) = stub_homeserver::spawn().await;
    let client = HomeserverClient::new(url, "test-as-token".into(), BOT.into());
    events::notify_stream_ended(&client, "!r:example.org", HOST, 61, 3)
        .await
        .expect("sent");
    let notices = stub.sends("m.room.message");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body["msgtype"], "m.notice");
    assert_eq!(
        notices[0].body["body"],
        "Stream ended (hosted by @alice:example.org, 1m 1s, peak 3 viewers)"
    );
    assert_sent_as_bot(&stub);
}

/// The client-written `feed.post` the apps send today validates against its
/// frozen schema (the server indexes it; see the feed indexer's own tests for
/// the legacy-name dispatch).
#[test]
fn the_client_written_feed_post_shape_is_frozen() {
    let post = json!({
        "version": 1,
        "body": "New post from @alice:example.org",
        "msgtype": "m.notice",
        "post_id": "01JABCDEF0123456789",
        "author": HOST,
        "text": "Hello",
        "media": [{ "kind": "image", "mxc": "mxc://example.org/abc", "width": 10, "height": 10 }]
    });
    assert_valid(LEGACY_FEED_POST, &post);
    let parsed: events::FeedPostContent = serde_json::from_value(post).expect("decodes");
    assert_eq!(parsed.media.len(), 1);
}

// ---------------------------------------------------------------------------
// Postgres: feed_started_event_id and the recording announcement of the end path
// ---------------------------------------------------------------------------

async fn pg_harness() -> Option<(Harness, sqlx::PgPool)> {
    let pool = mm_db::test_support::require_or_try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    let db = mm_db::PgDatabase::from_pool(pool.clone());
    Some((Harness::with_db(Box::new(db)).await, pool))
}

#[tokio::test]
async fn pg_start_stores_the_legacy_feed_started_id_and_end_relates_to_it() {
    let Some((h, pool)) = pg_harness().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let stream = h.seed_stream("audio").await;
    h.start(&stream, &None, None).await;

    let started = h.stub.sends(LEGACY_FEED_STARTED);
    let started_id = started[0].event_id.clone().expect("answered");
    let row = h.stored(&stream).await;
    assert_eq!(
        row.feed_started_event_id.as_deref(),
        Some(started_id.as_str())
    );
    assert_eq!(
        row.state_event_id,
        h.stub.state_puts(LEGACY_STREAM)[0].event_id
    );

    let sfu = NoSfu;
    let ctx = EndContext {
        marker: h.ctx(),
        sfu: &sfu,
        switch: None,
        pg_pool: Some(&pool),
        public_url: PUBLIC_URL,
        switch_call_timeout: Duration::from_secs(1),
    };
    end_and_finalise_stream(&ctx, &row, RecordingRelease::Publish)
        .await
        .expect("end");
    let ended = h.stub.sends(LEGACY_FEED_ENDED);
    assert_eq!(
        ended[0].body["m.relates_to"]["event_id"],
        started_id.as_str()
    );
}

#[tokio::test]
async fn pg_the_end_path_announces_ready_recordings_under_the_legacy_name() {
    let Some((h, pool)) = pg_harness().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let stream = h.seed_stream("video").await;
    h.start(&stream, &None, None).await;
    let room_pk: i64 = stream.room_id;
    let rec_id = format!("rec-wire-{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mm_recordings \
           (id, stream_id, room_id, host_user_id, status, media_type, storage_key, storage_backend, \
            duration_ms, mime_type, title) \
         VALUES ($1, $2, $3, $4, 'recording', 'video', $5, 'local', 360000, 'video/webm', 'Morning Show')",
    )
    .bind(&rec_id)
    .bind(&stream.id)
    .bind(room_pk)
    .bind(HOST)
    .bind(format!("recordings/{rec_id}.webm"))
    .execute(&pool)
    .await
    .expect("seed recording");

    let sfu = NoSfu;
    let ctx = EndContext {
        marker: h.ctx(),
        sfu: &sfu,
        switch: None,
        pg_pool: Some(&pool),
        public_url: PUBLIC_URL,
        switch_call_timeout: Duration::from_secs(1),
    };
    end_and_finalise_stream(&ctx, &h.stored(&stream).await, RecordingRelease::Publish)
        .await
        .expect("end");

    let sends = h.stub.sends(LEGACY_FEED_RECORDING);
    let ours: Vec<&Value> = sends
        .iter()
        .map(|r| &r.body)
        .filter(|b| b["recording_id"] == rec_id.as_str())
        .collect();
    assert_eq!(ours.len(), 1, "{sends:?}");
    assert_valid(LEGACY_FEED_RECORDING, ours[0]);
    assert_eq!(
        ours[0]["thumbnail_url_hint"],
        format!("{PUBLIC_URL}/_mm/recordings/{rec_id}.jpg")
    );
}

// ---------------------------------------------------------------------------
// Golden payloads: every optional field the server sends today
// ---------------------------------------------------------------------------

/// Replace the values that change from run to run (ids, clocks, durations)
/// with a placeholder, keeping every key.
fn normalise(mut v: Value) -> Value {
    const VOLATILE: &[&str] = &[
        "stream_id",
        "started_at_ms",
        "updated_at_ms",
        "ended_at_ms",
        "started_at",
        "ended_at",
        "duration_ms",
        "viewer_url",
        "rotated_at_ms",
        "rotates_next_ms",
    ];
    if let Some(o) = v.as_object_mut() {
        for (k, val) in o.iter_mut() {
            if VOLATILE.contains(&k.as_str()) {
                *val = json!("<volatile>");
            } else {
                *val = normalise(val.take());
            }
        }
    }
    v
}

/// Compare `payload` with `tests/golden/legacy/<name>.json` (written when
/// `MM_WRITE_GOLDEN=1`). Exact key-for-key equality after [`normalise`], so a
/// field that disappears, is renamed or changes value fails here.
#[track_caller]
fn assert_golden(name: &str, payload: &Value) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/legacy")
        .join(format!("{name}.json"));
    let actual = normalise(payload.clone());
    if std::env::var("MM_WRITE_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&actual).unwrap() + "\n").unwrap();
        return;
    }
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
    )
    .expect("golden parses");
    assert_eq!(
        actual, expected,
        "{name}: the payload differs from its golden file"
    );
}

#[tokio::test]
async fn every_legacy_payload_matches_its_golden_file() {
    let h = Harness::new().await;
    let stream = h.seed_stream("video").await;
    // Every optional field of the active marker at once: video, E2EE, the
    // public URL, the encoder source.
    h.start(&stream, &e2ee_info(), Some("whip")).await;
    assert_golden(
        "com.matrixmedia.stream.active",
        &h.stub.state_puts(LEGACY_STREAM)[0].body,
    );
    assert_golden(
        LEGACY_FEED_STARTED,
        &h.stub.sends(LEGACY_FEED_STARTED)[0].body,
    );

    let key = E2eeKeyEvent {
        stream_id: stream.id.clone(),
        algorithm: "aes-gcm-256".into(),
        key_id: "0011223344556677".into(),
        key_generation: 2,
        key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        rotated_at_ms: 1_765_000_000_000,
        rotates_next_ms: Some(1_765_000_600_000),
    };
    mm_api::matrix_writes::publish_stream_e2ee_key(&h.ctx(), &h.room_id, &key)
        .await
        .expect("key");
    assert_golden(LEGACY_E2EE_KEY, &h.stub.state_puts(LEGACY_E2EE_KEY)[0].body);

    let mut row = h.stored(&stream).await;
    row.e2ee_enabled = true;
    row.feed_started_event_id = Some("$started:stub".into());
    let before = h.stub.state_puts(LEGACY_STREAM).len();
    let sfu = NoSfu;
    let ctx = EndContext {
        marker: h.ctx(),
        sfu: &sfu,
        switch: None,
        pg_pool: None,
        public_url: PUBLIC_URL,
        switch_call_timeout: Duration::from_secs(1),
    };
    mm_api::stream_lifecycle::end_and_finalise_stream_with_reason(
        &ctx,
        &row,
        RecordingRelease::Publish,
        Some("This broadcast has been ended because the balance ran out."),
    )
    .await
    .expect("end");
    assert_golden(
        "com.matrixmedia.stream.ended",
        &h.stub.state_puts(LEGACY_STREAM)[before].body,
    );
    assert_golden(
        "com.matrixmedia.stream.e2ee_key.cleared",
        &h.stub.state_puts(LEGACY_E2EE_KEY).last().unwrap().body,
    );
    assert_golden(LEGACY_FEED_ENDED, &h.stub.sends(LEGACY_FEED_ENDED)[0].body);

    let recording = events::build_feed_recording_available(
        &stream.id,
        "rec-1",
        HOST,
        Some("Morning Show"),
        360_000,
        Some(format!("{PUBLIC_URL}/_mm/recordings/rec-1.jpg")),
        Some(format!("{PUBLIC_URL}/_mm/recordings/rec-1.en.vtt")),
        Some("A morning show.".into()),
    );
    mm_api::matrix_writes::emit_feed_recording_available(&h.ctx(), &h.room_id, &recording)
        .await
        .expect("recording");
    assert_golden(
        LEGACY_FEED_RECORDING,
        &h.stub.sends(LEGACY_FEED_RECORDING)[0].body,
    );

    let donation = DonationEventContent {
        donation_id: "b2c3d4e5-f6a7-8901-bcde-f12345678901".into(),
        stream_id: stream.id.clone(),
        donor_display_name: "Bob".into(),
        amount_cents: 500,
        currency: "usd".into(),
        message: Some("Great stream!".into()),
        tier: "yellow".into(),
        pin_duration_secs: 90,
        color: "#FFEB3B".into(),
        version: 1,
    };
    mm_api::matrix_writes::emit_donation(&h.ctx(), &h.room_id, &donation)
        .await
        .expect("donation");
    assert_golden(LEGACY_DONATION, &h.stub.sends(LEGACY_DONATION)[0].body);
    assert_golden(
        "m.room.message.donation_notice",
        &h.stub.sends("m.room.message")[0].body,
    );
}
