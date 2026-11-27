//! ADR-0011 phase A: the server writes `dev.matrixmedia.*` beside the legacy
//! names, keeps the legacy ids where comment threads are rooted, and reads
//! both names.
//!
//! Same harness as `legacy_wire_contract.rs` (in-memory SQLite, recording
//! stub homeserver), so every test here runs without external services. The
//! legacy guarantees themselves are pinned in `legacy_wire_contract.rs`; this
//! file covers what phase A adds.

#[path = "support/json_schema.rs"]
mod json_schema;
#[path = "support/stub_homeserver.rs"]
mod stub_homeserver;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

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
use mm_matrix::events::{DonationEventContent, E2eeKeyEvent, StreamEventContent};
use mm_sfu::{
    CreateRoomRequest, EgressInfo, ParticipantInfo, ParticipantPermissions, RoomStats, SfuAdapter,
    SfuError, SfuRoom, SfuToken,
};
use stub_homeserver::StubHomeserver;

const BOT: &str = "@mmbot:example.org";
const HOST: &str = "@alice:example.org";

const LEGACY_STREAM: &str = "com.matrixmedia.stream";
const DEV_STREAM: &str = "dev.matrixmedia.stream";
const LEGACY_KEY: &str = "com.matrixmedia.stream.e2ee_key";
const DEV_KEY: &str = "dev.matrixmedia.stream.e2ee_key";
const LEGACY_FEED_STARTED: &str = "com.steegler.matrixmedia.feed.broadcast.started";
const DEV_FEED_STARTED: &str = "dev.matrixmedia.feed.broadcast.started";
const LEGACY_FEED_ENDED: &str = "com.steegler.matrixmedia.feed.broadcast.ended";
const DEV_FEED_ENDED: &str = "dev.matrixmedia.feed.broadcast.ended";

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
    async fn delete_room(&self, _: &str) -> Result<(), SfuError> {
        Ok(())
    }
    fn supports_egress(&self) -> bool {
        false
    }
    async fn list_egresses(&self, _: &str) -> Result<Vec<EgressInfo>, SfuError> {
        Ok(Vec::new())
    }
    async fn stop_egress(&self, _: &str) -> Result<(), SfuError> {
        Ok(())
    }
    async fn generate_token(
        &self,
        _: &SfuRoom,
        _: &ParticipantInfo,
        _: ParticipantPermissions,
    ) -> Result<SfuToken, SfuError> {
        Ok(SfuToken {
            token: "t".into(),
            url: "ws://none".into(),
        })
    }
    async fn remove_participant(&self, _: &str, _: &str) -> Result<(), SfuError> {
        Ok(())
    }
    async fn list_participants(&self, _: &str) -> Result<Vec<ParticipantInfo>, SfuError> {
        Ok(Vec::new())
    }
    async fn room_stats(&self, id: &str) -> Result<RoomStats, SfuError> {
        Ok(RoomStats {
            sfu_room_id: id.into(),
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
    db: SqliteDatabase,
    room_id: String,
}

impl Harness {
    async fn new(legacy_contract: &str) -> Self {
        let (stub, url) = stub_homeserver::spawn().await;
        let mut config = Config::default();
        config.matrix.homeserver_url = url.clone();
        config.matrix.server_name = "example.org".into();
        config.matrix.synapse_admin_token = "adm".into();
        config.matrix.as_token = "as".into();
        config.matrix.event_legacy_contract = legacy_contract.into();
        config.server.public_url = Some("https://mm.example.org".into());
        let db_url = format!(
            "sqlite:file:ns_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        let db = SqliteDatabase::new(&db_url).await.unwrap();
        db.migrate().await.unwrap();
        Self {
            stub,
            client: HomeserverClient::new(url, "as".into(), BOT.into()),
            config,
            metrics: Metrics::new(),
            db,
            room_id: format!("!ns-{}:example.org", uuid::Uuid::new_v4().simple()),
        }
    }

    fn ctx(&self) -> MarkerContext<'_> {
        MarkerContext {
            hs_client: &self.client,
            db: &self.db,
            matrix: &self.config.matrix,
            metrics: &self.metrics,
        }
    }

    async fn seed(&self) -> Stream {
        let room = self
            .db
            .get_or_create_room(&RoomId(self.room_id.clone()))
            .await
            .unwrap();
        self.db
            .create_stream(
                room.id,
                &UserId(HOST.into()),
                Some("Show"),
                "video",
                Some("sfu"),
                None,
                "core",
            )
            .await
            .unwrap()
    }

    async fn start(&self, stream: &Stream, e2ee: bool) -> String {
        let info = e2ee.then(|| E2eeStreamInfo {
            enabled: true,
            algorithm: "aes-gcm-256".into(),
            key_id: "k1".into(),
            key_generation: 1,
            key_b64: "QUFBQQ==".into(),
        });
        mm_api::client::publish_stream_started_with(
            &self.ctx(),
            &self.config,
            stream,
            &self.room_id,
            Some("Show"),
            "video",
            true,
            &info,
            None,
        )
        .await
    }

    async fn row(&self, stream: &Stream) -> (Stream, Option<String>, Option<String>) {
        let id = StreamId(stream.id.clone());
        let row = self.db.get_stream(&id).await.unwrap().unwrap();
        let (cur_state, cur_ended) = self.db.get_stream_current_event_ids(&id).await.unwrap();
        (row, cur_state, cur_ended)
    }

    fn written(&self, event_type: &str) -> u64 {
        self.metrics
            .matrix_events_written_total
            .with_label_values(&[event_type])
            .get()
    }

    async fn end(&self, stream: &Stream) {
        let sfu = NoSfu;
        let ctx = EndContext {
            marker: self.ctx(),
            sfu: &sfu,
            switch: None,
            pg_pool: None,
            public_url: "https://mm.example.org",
            switch_call_timeout: Duration::from_secs(1),
        };
        let mut row = self.row(stream).await.0;
        row.e2ee_enabled = true;
        end_and_finalise_stream(&ctx, &row, RecordingRelease::Publish)
            .await
            .unwrap();
    }
}

fn write_order(stub: &StubHomeserver, types: &[&str]) -> Vec<String> {
    stub.writes()
        .into_iter()
        .filter_map(|w| w.state_target().map(|t| t.0).or_else(|| w.send_type()))
        .filter(|t| types.contains(&t.as_str()))
        .collect()
}

#[tokio::test]
async fn start_writes_the_stream_state_under_both_names_legacy_first() {
    let h = Harness::new("active").await;
    let stream = h.seed().await;
    let returned = h.start(&stream, false).await;

    assert_eq!(
        write_order(
            &h.stub,
            &[
                LEGACY_STREAM,
                DEV_STREAM,
                LEGACY_FEED_STARTED,
                DEV_FEED_STARTED
            ]
        ),
        vec![LEGACY_STREAM, DEV_STREAM, LEGACY_FEED_STARTED],
        "state under both names, legacy first; the timeline event once, legacy"
    );
    let legacy = &h.stub.state_puts(LEGACY_STREAM)[0];
    let dev = &h.stub.state_puts(DEV_STREAM)[0];
    assert_eq!(legacy.body, dev.body, "one payload, profile v1");
    assert_eq!(dev.state_target().unwrap().1, "");
    assert_valid(DEV_STREAM, &dev.body);

    let (row, cur_state, _) = h.row(&stream).await;
    assert_eq!(Some(returned), legacy.event_id, "the legacy id is returned");
    assert_eq!(
        row.state_event_id, legacy.event_id,
        "state_event_id stays legacy"
    );
    assert_eq!(
        cur_state, dev.event_id,
        "the dev id goes into the new column"
    );

    assert_eq!(h.written(LEGACY_STREAM), 1);
    assert_eq!(h.written(DEV_STREAM), 1);
    assert_eq!(h.written(LEGACY_FEED_STARTED), 1);
    assert_eq!(h.written(DEV_FEED_STARTED), 0);

    let history: Vec<String> =
        h.db.list_event_namespaces()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.0)
            .collect();
    for ns in [
        "dev.matrixmedia",
        "com.matrixmedia",
        "com.steegler.matrixmedia",
    ] {
        assert!(
            history.contains(&ns.to_string()),
            "{ns} recorded: {history:?}"
        );
    }
}

#[tokio::test]
async fn resume_writes_both_names_and_keeps_the_legacy_id() {
    let h = Harness::new("active").await;
    let stream = h.seed().await;
    h.start(&stream, false).await;
    let content = StreamEventContent {
        stream_id: stream.id.clone(),
        status: "active".into(),
        host_user_id: HOST.into(),
        title: None,
        media_type: "video".into(),
        video_config: None,
        viewer_url: None,
        mm_server_url: None,
        mm_matrix_server: None,
        federation_enabled: None,
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
    let (id, generation) = republish_active_marker(&h.ctx(), &stream, &h.room_id, content)
        .await
        .unwrap();
    let legacy = h.stub.state_puts(LEGACY_STREAM);
    let dev = h.stub.state_puts(DEV_STREAM);
    assert_eq!((legacy.len(), dev.len()), (2, 2));
    assert_eq!(dev[1].body["marker_generation"], 2);
    assert_eq!(generation, 2);
    assert_eq!(Some(id), legacy[1].event_id);
    let (row, cur_state, _) = h.row(&stream).await;
    assert_eq!(row.state_event_id, legacy[1].event_id);
    assert_eq!(cur_state, dev[1].event_id);
}

#[tokio::test]
async fn end_writes_both_terminal_names_and_clears_the_key_under_both() {
    let h = Harness::new("active").await;
    let stream = h.seed().await;
    h.start(&stream, true).await;
    h.end(&stream).await;

    let clears: Vec<_> = write_order(&h.stub, &[LEGACY_KEY, DEV_KEY]);
    assert_eq!(clears, vec![LEGACY_KEY, DEV_KEY]);
    for t in [LEGACY_KEY, DEV_KEY] {
        assert_eq!(h.stub.state_puts(t).last().unwrap().body, json!({}));
    }
    let legacy_end = h.stub.state_puts(LEGACY_STREAM).pop().unwrap();
    let dev_end = h.stub.state_puts(DEV_STREAM).pop().unwrap();
    assert_eq!(legacy_end.body["status"], "ended");
    assert_eq!(dev_end.body, legacy_end.body);
    let (row, _, cur_ended) = h.row(&stream).await;
    assert_eq!(
        row.ended_event_id, legacy_end.event_id,
        "ended_event_id stays legacy"
    );
    assert_eq!(cur_ended, dev_end.event_id);
    assert_eq!(h.stub.sends(LEGACY_FEED_ENDED).len(), 1);
    assert!(
        h.stub.sends(DEV_FEED_ENDED).is_empty(),
        "no timeline dual-write"
    );
    assert_eq!(h.metrics.stream_terminal_events_total.get(), 1);
}

/// Brief 8.1: while the legacy name is written, the legacy write alone
/// decides the end. A current-name failure is not retried, does not sleep,
/// does not count as a terminal failure, and has its own counter.
#[tokio::test]
async fn a_failed_new_name_terminal_write_neither_fails_nor_delays_the_end() {
    let h = Harness::new("active").await;
    let stream = h.seed().await;
    h.start(&stream, false).await;
    h.stub.fail_always(&format!("/state/{DEV_STREAM}/"));

    let started = std::time::Instant::now();
    let id = finalize_stream_marker(&h.ctx(), &stream, &h.room_id)
        .await
        .expect("the legacy terminal write decides: done");
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "no retry backoff for the current name"
    );

    let legacy_end = h.stub.state_puts(LEGACY_STREAM).pop().unwrap();
    assert_eq!(legacy_end.body["status"], "ended");
    assert_eq!(Some(id), legacy_end.event_id);
    let dev_terminals: Vec<_> = h
        .stub
        .state_puts(DEV_STREAM)
        .into_iter()
        .filter(|r| r.body["status"] == "ended")
        .collect();
    assert_eq!(
        dev_terminals.len(),
        1,
        "one attempt under the current name, no retry"
    );
    let (row, _, cur_ended) = h.row(&stream).await;
    assert_eq!(row.ended_event_id, legacy_end.event_id);
    assert_eq!(cur_ended, None);
    assert_eq!(h.metrics.stream_terminal_events_total.get(), 1);
    assert_eq!(h.metrics.stream_terminal_event_failures_total.get(), 0);
    assert_eq!(
        h.metrics
            .matrix_current_name_write_failures_total
            .with_label_values(&[DEV_STREAM])
            .get(),
        1
    );
}

/// The legacy write still has its retry and its failure metric.
#[tokio::test]
async fn a_legacy_terminal_failure_is_retried_and_loud_whatever_the_current_name_does() {
    let h = Harness::new("active").await;
    let stream = h.seed().await;
    h.start(&stream, false).await;
    h.stub.fail_always(&format!("/state/{LEGACY_STREAM}/"));
    assert!(
        finalize_stream_marker(&h.ctx(), &stream, &h.room_id)
            .await
            .is_none()
    );
    let legacy_terminals = h
        .stub
        .state_puts(LEGACY_STREAM)
        .into_iter()
        .filter(|r| r.body["status"] == "ended")
        .count();
    assert_eq!(legacy_terminals, 3, "three attempts");
    assert_eq!(h.metrics.stream_terminal_event_failures_total.get(), 1);
    assert_eq!(h.metrics.stream_terminal_events_total.get(), 0);
    // The current name was still written, once.
    let (_, _, cur_ended) = h.row(&stream).await;
    assert!(cur_ended.is_some());
}

/// Retired contract, stream started after retirement: the current name is
/// the anchor and gets the retry.
#[tokio::test]
async fn retired_retries_the_current_name_terminal_write() {
    let h = Harness::new("retired").await;
    let stream = h.seed().await;
    h.start(&stream, false).await;
    h.stub.fail_times(&format!("/state/{DEV_STREAM}/"), 1);
    let id = finalize_stream_marker(&h.ctx(), &stream, &h.room_id)
        .await
        .expect("retried");
    let dev_terminals: Vec<_> = h
        .stub
        .state_puts(DEV_STREAM)
        .into_iter()
        .filter(|r| r.body["status"] == "ended")
        .collect();
    assert_eq!(dev_terminals.len(), 2);
    assert_eq!(Some(id), dev_terminals[1].event_id);
    assert_eq!(h.metrics.stream_terminal_events_total.get(), 1);
}

/// Start and key writes: a current-name failure is counted, the legacy id
/// is still returned and stored.
#[tokio::test]
async fn a_failed_new_name_write_at_start_is_counted_and_does_not_fail_the_start() {
    let h = Harness::new("active").await;
    let stream = h.seed().await;
    h.stub.fail_always(&format!("/state/{DEV_STREAM}/"));
    let id = h.start(&stream, false).await;
    assert_eq!(Some(id), h.stub.state_puts(LEGACY_STREAM)[0].event_id);
    assert_eq!(
        h.metrics
            .matrix_current_name_write_failures_total
            .with_label_values(&[DEV_STREAM])
            .get(),
        1
    );
    let (row, cur, _) = h.row(&stream).await;
    assert!(row.state_event_id.is_some());
    assert_eq!(cur, None);
}

#[tokio::test]
async fn the_e2ee_key_is_published_under_both_names() {
    let h = Harness::new("active").await;
    let stream = h.seed().await;
    let key = E2eeKeyEvent {
        stream_id: stream.id.clone(),
        algorithm: "aes-gcm-256".into(),
        key_id: "k".into(),
        key_generation: 3,
        key_b64: "QUFBQQ==".into(),
        rotated_at_ms: 1,
        rotates_next_ms: None,
    };
    let id = mm_api::matrix_writes::publish_stream_e2ee_key(&h.ctx(), &h.room_id, &key)
        .await
        .unwrap();
    assert_eq!(
        write_order(&h.stub, &[LEGACY_KEY, DEV_KEY]),
        vec![LEGACY_KEY, DEV_KEY]
    );
    assert_eq!(Some(id), h.stub.state_puts(LEGACY_KEY)[0].event_id);
    assert_eq!(
        h.stub.state_puts(DEV_KEY)[0].state_target().unwrap().1,
        stream.id
    );
}

#[tokio::test]
async fn retired_writes_the_current_names_only() {
    let h = Harness::new("retired").await;
    let stream = h.seed().await;
    let returned = h.start(&stream, true).await;
    assert!(h.stub.state_puts(LEGACY_STREAM).is_empty());
    assert!(h.stub.sends(LEGACY_FEED_STARTED).is_empty());
    assert_eq!(h.stub.sends(DEV_FEED_STARTED).len(), 1);
    let dev = &h.stub.state_puts(DEV_STREAM)[0];
    assert_eq!(Some(returned), dev.event_id);
    let (row, cur_state, _) = h.row(&stream).await;
    assert_eq!(
        row.state_event_id, dev.event_id,
        "retired: state_event_id is the current id"
    );
    assert_eq!(cur_state, dev.event_id);

    h.end(&stream).await;
    assert!(
        h.stub.state_puts(LEGACY_STREAM).is_empty(),
        "no legacy terminal for a new stream"
    );
    assert!(h.stub.state_puts(LEGACY_KEY).is_empty());
    assert_eq!(h.stub.sends(DEV_FEED_ENDED).len(), 1);
    let (row, _, cur_ended) = h.row(&stream).await;
    assert_eq!(row.ended_event_id, cur_ended);
    assert!(row.ended_event_id.is_some());

    let donation = DonationEventContent {
        donation_id: "d".into(),
        stream_id: stream.id.clone(),
        donor_display_name: "Bob".into(),
        amount_cents: 100,
        currency: "usd".into(),
        message: None,
        tier: "blue".into(),
        pin_duration_secs: 30,
        color: "#000000".into(),
        version: 1,
    };
    mm_api::matrix_writes::emit_donation(&h.ctx(), &h.room_id, &donation)
        .await
        .unwrap();
    assert_eq!(h.stub.sends("dev.matrixmedia.donation").len(), 1);
    assert!(h.stub.sends("com.matrixmedia.donation").is_empty());
}

/// Rule 4: a stream that started with a legacy write ends with one, even when
/// the legacy contract was retired in between.
#[tokio::test]
async fn a_stream_started_under_the_legacy_contract_ends_with_a_legacy_write_after_retirement() {
    let mut h = Harness::new("active").await;
    let stream = h.seed().await;
    h.start(&stream, true).await;

    h.config.matrix.event_legacy_contract = "retired".into();
    h.end(&stream).await;

    let legacy_end = h.stub.state_puts(LEGACY_STREAM).pop().unwrap();
    assert_eq!(
        legacy_end.body["status"], "ended",
        "legacy terminal still written"
    );
    assert_eq!(
        h.stub.state_puts(LEGACY_KEY).last().unwrap().body,
        json!({}),
        "legacy key cleared"
    );
    let (row, _, _) = h.row(&stream).await;
    assert_eq!(row.ended_event_id, legacy_end.event_id);
    // The timeline event follows the contract: current name only.
    assert_eq!(h.stub.sends(DEV_FEED_ENDED).len(), 1);
    assert!(h.stub.sends(LEGACY_FEED_ENDED).is_empty());
}

/// Brief 7.7: while the legacy contract is active the legacy copy is the
/// truth (apps that have not updated write only it); the current name is read
/// only when the legacy one does not exist. After retirement the order flips.
#[tokio::test]
async fn feed_muted_reads_legacy_first_while_active_and_current_first_after() {
    let (stub, url) = stub_homeserver::spawn().await;
    let active = mm_matrix::events::WriteNames::new("dev.matrixmedia", true);
    let retired = mm_matrix::events::WriteNames::new("dev.matrixmedia", false);
    let read = |names: &mm_matrix::events::WriteNames| {
        let names = names.clone();
        let url = url.clone();
        async move {
            mm_api::feed::fetch_muted_rooms_with(&url, "as", "@carol:example.org", &names).await
        }
    };

    // Neither set: nothing muted.
    assert!(read(&active).await.is_empty());
    assert!(read(&retired).await.is_empty());

    // Only the current name set: both modes fall back to it.
    stub.answer_get(
        "/account_data/dev.matrixmedia.feed_muted",
        StatusCode::OK,
        json!({ "rooms": ["!new:example.org"] }),
    );
    assert_eq!(read(&active).await, vec!["!new:example.org".to_string()]);
    assert_eq!(read(&retired).await, vec!["!new:example.org".to_string()]);

    // Both set: active reads the legacy copy (even an empty one), retired the current.
    stub.answer_get(
        "/account_data/com.steegler.matrixmedia.feed_muted",
        StatusCode::OK,
        json!({ "rooms": [] }),
    );
    assert!(
        read(&active).await.is_empty(),
        "the legacy copy wins while active"
    );
    assert_eq!(read(&retired).await, vec!["!new:example.org".to_string()]);
    stub.answer_get(
        "/account_data/com.steegler.matrixmedia.feed_muted",
        StatusCode::OK,
        json!({ "rooms": ["!old:example.org"] }),
    );
    assert_eq!(read(&active).await, vec!["!old:example.org".to_string()]);
    // The default entry point is the active-contract order.
    assert_eq!(
        mm_api::feed::fetch_muted_rooms_from(&url, "as", "@carol:example.org").await,
        vec!["!old:example.org".to_string()]
    );
}

/// Every legacy schema has a `dev.matrixmedia` twin that accepts exactly the
/// same payload shape (profile v1), and the legacy one is marked deprecated.
#[test]
fn every_legacy_schema_has_a_dev_twin_with_the_same_payload() {
    let pairs = [
        ("com.matrixmedia.stream", "dev.matrixmedia.stream"),
        (
            "com.matrixmedia.stream.e2ee_key",
            "dev.matrixmedia.stream.e2ee_key",
        ),
        ("com.matrixmedia.donation", "dev.matrixmedia.donation"),
        (
            "com.steegler.matrixmedia.feed.broadcast.started",
            "dev.matrixmedia.feed.broadcast.started",
        ),
        (
            "com.steegler.matrixmedia.feed.broadcast.ended",
            "dev.matrixmedia.feed.broadcast.ended",
        ),
        (
            "com.steegler.matrixmedia.feed.recording.available",
            "dev.matrixmedia.feed.recording.available",
        ),
        (
            "com.steegler.matrixmedia.feed.post",
            "dev.matrixmedia.feed.post",
        ),
        (
            "com.steegler.matrixmedia.feed_muted",
            "dev.matrixmedia.feed_muted",
        ),
    ];
    let strip = |mut v: serde_json::Value| {
        for k in ["$id", "title", "description", "deprecated", "$comment"] {
            v.as_object_mut().unwrap().remove(k);
        }
        v
    };
    for (legacy, dev) in pairs {
        let l = json_schema::load_event_schema(legacy);
        let d = json_schema::load_event_schema(dev);
        assert_eq!(l["deprecated"], true, "{legacy} is marked deprecated");
        assert!(d.get("deprecated").is_none(), "{dev} is current");
        assert_eq!(
            d["$id"],
            format!("https://matrixmedia.dev/schemas/events/{dev}.json")
        );
        assert_eq!(
            strip(l),
            strip(d),
            "{legacy} and {dev} differ in payload rules"
        );
    }
}

/// Brief 7.7 / audit: the second name is read only when the first one does
/// not exist (404). Any other failure of the first read is "no muted rooms",
/// as before ADR-0011, and never surfaces the other copy.
#[tokio::test]
async fn feed_muted_falls_back_only_when_the_first_copy_does_not_exist() {
    let (stub, url) = stub_homeserver::spawn().await;
    let active = mm_matrix::events::WriteNames::new("dev.matrixmedia", true);
    let retired = mm_matrix::events::WriteNames::new("dev.matrixmedia", false);
    stub.answer_get(
        "/account_data/dev.matrixmedia.feed_muted",
        StatusCode::OK,
        json!({ "rooms": ["!x:example.org"] }),
    );

    // The auditor's case: the legacy read answers 502 → nothing, not room X.
    stub.answer_get(
        "/account_data/com.steegler.matrixmedia.feed_muted",
        StatusCode::BAD_GATEWAY,
        json!({ "errcode": "M_UNKNOWN" }),
    );
    let read = |names: mm_matrix::events::WriteNames| {
        let url = url.clone();
        async move {
            mm_api::feed::fetch_muted_rooms_with(&url, "as", "@carol:example.org", &names).await
        }
    };
    assert!(read(active.clone()).await.is_empty());

    // An unexpected shape is a failure too, not an absence.
    stub.answer_get(
        "/account_data/com.steegler.matrixmedia.feed_muted",
        StatusCode::OK,
        json!({ "unexpected": true }),
    );
    assert!(read(active.clone()).await.is_empty());

    // Retired: the current copy first; its failure does not fall back either.
    stub.answer_get(
        "/account_data/com.steegler.matrixmedia.feed_muted",
        StatusCode::OK,
        json!({ "rooms": ["!old:example.org"] }),
    );
    stub.answer_get(
        "/account_data/dev.matrixmedia.feed_muted",
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({}),
    );
    assert!(read(retired.clone()).await.is_empty());
    // Absent current copy: the legacy one.
    stub.answer_get(
        "/account_data/dev.matrixmedia.feed_muted",
        StatusCode::NOT_FOUND,
        json!({ "errcode": "M_NOT_FOUND" }),
    );
    assert_eq!(read(retired).await, vec!["!old:example.org".to_string()]);
}
