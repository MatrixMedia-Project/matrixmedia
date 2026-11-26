//! ADR-0011 section 3: the MM-room marker `dev.matrixmedia.room_config`, its
//! idempotency, power-level pinning (section 4, rule 8) and the backfill.
//! In-memory SQLite and a recording stub homeserver: runs everywhere.

#[path = "support/json_schema.rs"]
mod json_schema;
#[path = "support/stub_homeserver.rs"]
mod stub_homeserver;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

use mm_api::room_marker::{MarkerOutcome, Pinning, backfill, ensure_room_marker};
use mm_api::stream_lifecycle::MarkerContext;
use mm_core::config::Config;
use mm_core::metrics::Metrics;
use mm_db::Database;
use mm_db::sqlite::SqliteDatabase;
use mm_matrix::client::HomeserverClient;
use stub_homeserver::StubHomeserver;

const BOT: &str = "@mmbot:example.org";
const MARKER: &str = "dev.matrixmedia.room_config";

struct Harness {
    stub: Arc<StubHomeserver>,
    client: HomeserverClient,
    config: Config,
    metrics: Metrics,
    db: SqliteDatabase,
}

impl Harness {
    async fn new() -> Self {
        let (stub, url) = stub_homeserver::spawn().await;
        let mut config = Config::default();
        config.matrix.homeserver_url = url.clone();
        config.matrix.server_name = "example.org".into();
        config.server.public_url = Some("https://mm.example.org".into());
        let db = SqliteDatabase::new(&format!(
            "sqlite:file:marker_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        Self {
            stub,
            client: HomeserverClient::new(url, "as".into(), BOT.into()),
            config,
            metrics: Metrics::new(),
            db,
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

    fn namespaces(&self) -> mm_api::event_namespaces::EventNamespaces {
        mm_api::event_namespaces::build(&self.config.matrix, &[])
    }

    fn power(&self, room: &str, bot_level: i64) {
        self.stub.answer_get(
            &format!("{room}/state/m.room.power_levels/"),
            StatusCode::OK,
            json!({ "users": { BOT: bot_level, "@alice:example.org": 100 }, "state_default": 50,
                    "events": { "m.room.name": 50 } }),
        );
    }

    async fn ensure(&self, room: &str, enabled: bool) -> MarkerOutcome {
        ensure_room_marker(
            &self.ctx(),
            &self.config,
            &self.namespaces(),
            room,
            enabled,
            Pinning::Allowed,
        )
        .await
    }
}

#[tokio::test]
async fn the_marker_is_written_once_with_version_2_content_and_rewritten_on_change() {
    let h = Harness::new().await;
    let room = "!m1:example.org";
    h.power(room, 100);

    let MarkerOutcome::Written(id) = h.ensure(room, true).await else {
        panic!("not written")
    };
    let puts = h.stub.state_puts(MARKER);
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].state_target().unwrap().1, "", "state key is empty");
    assert_eq!(puts[0].as_user().as_deref(), Some(BOT));
    let body = &puts[0].body;
    assert_eq!(body["version"], 2);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["mm_server_url"], "https://mm.example.org");
    assert_eq!(body["mm_matrix_server"], "example.org");
    assert!(body["updated_at_ms"].as_i64().unwrap() > 0);
    assert_eq!(
        body["event_namespaces"],
        serde_json::to_value(h.namespaces()).unwrap()
    );
    json_schema::assert_valid(MARKER, body);
    assert_eq!(
        h.db.get_room_marker(room).await.unwrap().map(|r| r.0),
        Some(id)
    );

    // Same content: skipped.
    assert_eq!(h.ensure(room, true).await, MarkerOutcome::Unchanged);
    assert_eq!(h.stub.state_puts(MARKER).len(), 1);

    // The owner switches MatrixMedia off: rewritten.
    assert!(matches!(
        h.ensure(room, false).await,
        MarkerOutcome::Written(_)
    ));
    assert_eq!(h.stub.state_puts(MARKER)[1].body["enabled"], false);
}

/// Brief 8.2: pinning ships off.
#[tokio::test]
async fn pinning_is_off_by_default() {
    let h = Harness::new().await;
    assert!(!h.config.matrix.pin_event_power_levels);
    let room = "!m0:example.org";
    h.power(room, 100);
    h.ensure(room, true).await;
    assert!(h.stub.state_puts("m.room.power_levels").is_empty());
    assert_eq!(h.stub.state_puts(MARKER).len(), 1);
}

#[tokio::test]
async fn with_pinning_on_the_current_state_types_are_pinned_at_100_once() {
    let mut h = Harness::new().await;
    h.config.matrix.pin_event_power_levels = true;
    let room = "!m2:example.org";
    h.power(room, 100);
    h.ensure(room, true).await;

    let pl = h.stub.state_puts("m.room.power_levels");
    assert_eq!(pl.len(), 1, "pinned once");
    assert_eq!(pl[0].as_user().as_deref(), Some(BOT));
    let events = &pl[0].body["events"];
    for t in [
        "dev.matrixmedia.stream",
        "dev.matrixmedia.stream.e2ee_key",
        MARKER,
    ] {
        assert_eq!(events[t], 100, "{t}");
    }
    assert_eq!(events["m.room.name"], 50, "other entries kept");
    assert_eq!(pl[0].body["users"]["@alice:example.org"], 100, "users kept");
    // A recorded pin is not repeated at the next check.
    h.ensure(room, true).await;
    assert_eq!(h.stub.state_puts("m.room.power_levels").len(), 1);
}

/// Brief 8.2: a skipped pin is not recorded as done; the next check tries
/// again even though the marker itself is unchanged.
#[tokio::test]
async fn a_skipped_pin_is_tried_again_at_the_next_check() {
    let mut h = Harness::new().await;
    h.config.matrix.pin_event_power_levels = true;
    let room = "!m3:example.org";
    h.power(room, 50);
    assert!(matches!(
        h.ensure(room, true).await,
        MarkerOutcome::Written(_)
    ));
    assert!(h.stub.state_puts("m.room.power_levels").is_empty());

    // The bot is promoted; the marker is unchanged, the pin now lands.
    h.power(room, 100);
    assert_eq!(h.ensure(room, true).await, MarkerOutcome::Unchanged);
    assert_eq!(h.stub.state_puts("m.room.power_levels").len(), 1);
    assert_eq!(h.stub.state_puts(MARKER).len(), 1);
}

/// A failed pin write is retried too.
#[tokio::test]
async fn a_failed_pin_is_tried_again_at_the_next_check() {
    let mut h = Harness::new().await;
    h.config.matrix.pin_event_power_levels = true;
    let room = "!m4:example.org";
    h.power(room, 100);
    h.stub.fail_times("/state/m.room.power_levels", 1);
    h.ensure(room, true).await;
    h.ensure(room, true).await;
    let pins = h.stub.state_puts("m.room.power_levels");
    assert_eq!(pins.len(), 2, "one failed, one retried");
    assert!(pins[1].event_id.is_some());
}

#[tokio::test]
async fn no_public_url_means_no_marker() {
    let mut h = Harness::new().await;
    h.config.server.public_url = None;
    assert_eq!(
        h.ensure("!m5:example.org", true).await,
        MarkerOutcome::NotConfigured
    );
    assert!(h.stub.writes().is_empty());
}

#[tokio::test]
async fn the_backfill_counts_on_a_dry_run_writes_on_a_real_one_and_is_idempotent() {
    let h = Harness::new().await;
    h.stub.answer_get(
        "/joined_rooms",
        StatusCode::OK,
        json!({ "joined_rooms": ["!ok:example.org", "!weak:example.org", "!off:example.org"] }),
    );
    h.power("!ok:example.org", 100);
    h.power("!weak:example.org", 0);
    h.power("!off:example.org", 100);
    let enabled = |room: String| async move { room != "!off:example.org" };
    let mut h = h;
    h.config.matrix.pin_event_power_levels = true; // the backfill never pins anyway
    let ctx = h.ctx();
    let ns = h.namespaces();
    let run = |dry| backfill(&ctx, &h.config, &ns, dry, Duration::ZERO, enabled);

    let dry = run(true).await.unwrap();
    assert_eq!(
        (dry.eligible, dry.already_marked, dry.to_write, dry.no_power),
        (3, 0, 2, 1)
    );
    assert_eq!(dry.no_power_rooms, vec!["!weak:example.org".to_string()]);
    assert!(
        h.stub.state_puts(MARKER).is_empty(),
        "a dry run writes nothing"
    );

    let real = run(false).await.unwrap();
    assert_eq!((real.to_write, real.written, real.failed), (2, 2, 0));
    let off = h
        .stub
        .state_puts(MARKER)
        .into_iter()
        .find(|r| r.path.contains("!off:example.org"))
        .unwrap();
    assert_eq!(
        off.body["enabled"], false,
        "an opted-out room is marked enabled: false"
    );

    let again = run(false).await.unwrap();
    assert_eq!(
        (again.already_marked, again.to_write, again.written),
        (2, 0, 0)
    );
    assert_eq!(
        h.stub.state_puts(MARKER).len(),
        2,
        "running twice changes nothing"
    );
    assert!(
        h.stub.state_puts("m.room.power_levels").is_empty(),
        "the backfill never pins"
    );
}

/// A homeserver error on the power-levels read is reported apart from "no
/// power".
#[tokio::test]
async fn the_backfill_reports_unreadable_power_levels_separately() {
    let h = Harness::new().await;
    h.stub.answer_get(
        "/joined_rooms",
        StatusCode::OK,
        json!({ "joined_rooms": ["!broken:example.org"] }),
    );
    h.stub.answer_get(
        "!broken:example.org/state/m.room.power_levels/",
        StatusCode::BAD_GATEWAY,
        json!({ "errcode": "M_UNKNOWN" }),
    );
    let ctx = h.ctx();
    let ns = h.namespaces();
    let r = backfill(&ctx, &h.config, &ns, true, Duration::ZERO, |_| async {
        true
    })
    .await
    .unwrap();
    assert_eq!((r.no_power, r.unreadable), (0, 1));
    assert_eq!(r.unreadable_rooms, vec!["!broken:example.org".to_string()]);
}
