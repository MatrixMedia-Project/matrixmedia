//! `ScalewayChecker` and the three read-only `ScalewayProvider` calls behind it, against a
//! stand-in Scaleway. The list routes copy the shapes `tests/scaleway_wire.rs` pins
//! (instance API: total in the `x-total-count` header; block API: `total_count` in the
//! body), because `Provider::list` reads the block volumes first and then the servers.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use mm_fleet::checks::{CheckState, ProviderChecker, ScalewayChecker, Stock};
use mm_fleet::scaleway::ScalewayProvider;

#[derive(Default)]
struct Knobs {
    reject_key: bool,
    servers_calls: u32,
    /// Zones whose servers route answers 401, whatever token is sent.
    reject_key_zones: Vec<String>,
    /// Zones whose servers route answers 500.
    fail_servers_zones: Vec<String>,
    /// The block-volumes route answers 403: a key that can list servers but not volumes.
    volumes_forbidden: bool,
    /// The public availability route answers 500.
    availability_down: bool,
}
type Shared = Arc<Mutex<Knobs>>;

async fn servers(
    State(s): State<Shared>,
    Path(zone): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    k.servers_calls += 1;
    if k.fail_servers_zones.contains(&zone) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"type": "internal_error", "message": "boom"})),
        )
            .into_response();
    }
    if k.reject_key
        || k.reject_key_zones.contains(&zone)
        || headers
            .get("x-auth-token")
            .map(|v| v != "SCW-TEST-SECRET")
            .unwrap_or(true)
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"type": "denied_authentication", "message": "nope"})),
        )
            .into_response();
    }
    // The instance API reports its total in a header, not the body.
    ([("x-total-count", "0")], Json(json!({"servers": []}))).into_response()
}
/// block/v1 list: empty, with `total_count` in the body.
async fn volumes(State(s): State<Shared>, Path(_zone): Path<String>) -> axum::response::Response {
    if s.lock().unwrap().volumes_forbidden {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"type": "denied_authorization", "message": "no block access"})),
        )
            .into_response();
    }
    Json(json!({"volumes": [], "total_count": 0})).into_response()
}
async fn availability(
    State(s): State<Shared>,
    Path(_zone): Path<String>,
) -> axum::response::Response {
    if s.lock().unwrap().availability_down {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"type": "internal_error", "message": "boom"})),
        )
            .into_response();
    }
    Json(
        json!({"servers": {"L4-1-24G": {"availability": "scarce"}, "COMPUTE3-X8C-16G": {"availability": "available"}}}),
    )
    .into_response()
}
async fn products(Path(_zone): Path<String>) -> Json<Value> {
    Json(
        json!({"servers": {"L4-1-24G": {"hourly_price": 0.7875, "network": {"sum_internet_bandwidth": 2000000000}}}}),
    )
}

async fn fake() -> (String, Shared) {
    let state: Shared = Arc::default();
    let app = Router::new()
        .route("/instance/v1/zones/{zone}/servers", get(servers))
        .route("/block/v1/zones/{zone}/volumes", get(volumes))
        .route(
            "/instance/v1/zones/{zone}/products/servers/availability",
            get(availability),
        )
        .route("/instance/v1/zones/{zone}/products/servers", get(products))
        .with_state(state.clone());
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), state)
}

fn provider(base: &str) -> ScalewayProvider {
    ScalewayProvider::new(
        "SCW-TEST-SECRET",
        "proj-1",
        "fr-par-2",
        "ubuntu_noble",
        "mm-fleet",
    )
    .with_base_url(base)
}

#[tokio::test]
async fn verify_key_distinguishes_rejection_from_success() {
    let (base, knobs) = fake().await;
    provider(&base).verify_key().await.expect("valid key");
    knobs.lock().unwrap().reject_key = true;
    let err = provider(&base).verify_key().await.unwrap_err();
    assert!(err.needs_human(), "{err}");
}

#[tokio::test]
async fn availability_and_prices_are_public_reads() {
    let (base, knobs) = fake().await;
    let stock = provider(&base).availability().await.unwrap();
    assert_eq!(stock.get("L4-1-24G"), Some(&Stock::Scarce));
    let prices = provider(&base).hourly_prices().await.unwrap();
    assert_eq!(prices.get("L4-1-24G"), Some(&0.7875));
    assert_eq!(
        knobs.lock().unwrap().servers_calls,
        0,
        "no authenticated call for public data"
    );
}

#[tokio::test]
async fn checker_reports_ok_with_stock_prices_and_null_scope() {
    let (base, _) = fake().await;
    let c = ScalewayChecker {
        secret_key: "SCW-TEST-SECRET".into(),
        project_id: "proj-1".into(),
        fleet_tag: "mm-fleet".into(),
        base_url: base,
        zones: vec![("fr-par-2".into(), vec!["L4-1-24G".into()])],
    };
    let r = c.check().await;
    assert_eq!(r.state, CheckState::Ok);
    assert_eq!(r.key_scope, None);
    assert_eq!(r.zones[0].stock.get("L4-1-24G"), Some(&Stock::Scarce));
    assert_eq!(r.zones[0].instances_running, Some(0));
    assert_eq!(r.prices.get("L4-1-24G"), Some(&0.7875));
    let row = r.to_status_row("p-1", 1, chrono::Utc::now());
    assert_eq!(row.state, "ok");
    assert_eq!(row.quota["fr-par-2"]["limit"], 1);
    assert_eq!(row.stock["fr-par-2"]["L4-1-24G"], "scarce");
}

#[tokio::test]
async fn checker_reports_needs_you_on_a_rejected_key_and_never_echoes_it() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().reject_key = true;
    let c = ScalewayChecker {
        secret_key: "SCW-TEST-SECRET".into(),
        project_id: "proj-1".into(),
        fleet_tag: "mm-fleet".into(),
        base_url: base,
        zones: vec![("fr-par-2".into(), vec!["L4-1-24G".into()])],
    };
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(!msg.contains("SCW-TEST-SECRET"));
}

fn checker(base: String, zones: &[&str]) -> ScalewayChecker {
    ScalewayChecker {
        secret_key: "SCW-TEST-SECRET".into(),
        project_id: "proj-1".into(),
        fleet_tag: "mm-fleet".into(),
        base_url: base,
        zones: zones
            .iter()
            .map(|z| (z.to_string(), vec!["L4-1-24G".to_string()]))
            .collect(),
    }
}

/// Zone order must not decide the verdict: a key a human has to fix outranks a provider
/// 500 in another zone, whichever zone is checked first, and its error is the one kept.
#[tokio::test]
async fn a_key_rejection_outranks_another_zones_outage_in_either_order() {
    for zones in [["fr-par-2", "nl-ams-1"], ["nl-ams-1", "fr-par-2"]] {
        let (base, knobs) = fake().await;
        {
            let mut k = knobs.lock().unwrap();
            k.reject_key_zones = vec!["fr-par-2".into()];
            k.fail_servers_zones = vec!["nl-ams-1".into()];
        }
        let r = checker(base, &zones).check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "zones {zones:?}");
        let (kind, msg) = r.last_error.clone().unwrap();
        assert_eq!(kind, "permanent", "zones {zones:?}");
        assert!(!msg.contains("SCW-TEST-SECRET"));
        assert_eq!(r.zones.len(), 2, "both zones still reported");
    }
}

/// The key lists servers (so `verify_key` passes) but is refused on the block-volumes
/// route that `list()` reads first: that is a key a human must fix, not an `ok`.
#[tokio::test]
async fn a_failure_after_a_good_verify_key_still_escalates() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().volumes_forbidden = true;
    let r = checker(base, &["fr-par-2"]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, _) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(r.zones[0].instances_running, None);
    assert_eq!(r.zones[0].stock.get("L4-1-24G"), Some(&Stock::Scarce));
    assert_eq!(r.prices.get("L4-1-24G"), Some(&0.7875));
}

#[tokio::test]
async fn no_configured_zones_is_unknown_and_makes_no_call() {
    let (base, knobs) = fake().await;
    let r = checker(base, &[]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(
        r.last_error,
        Some(("config".to_string(), "no zones configured".to_string()))
    );
    assert!(r.zones.is_empty());
    assert_eq!(knobs.lock().unwrap().servers_calls, 0);
}

/// Stock is "per configured size": a stock read that failed reports every size as
/// `unknown`, never an empty map that the dashboard would render as no sizes at all.
#[tokio::test]
async fn a_failed_stock_read_reports_every_configured_size_unknown() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().availability_down = true;
    let mut c = checker(base, &["fr-par-2"]);
    c.zones[0].1 = vec!["L4-1-24G".into(), "COMPUTE3-X8C-16G".into()];
    let r = c.check().await;
    assert_eq!(r.state, CheckState::Unknown);
    let (kind, _) = r.last_error.clone().unwrap();
    assert_eq!(kind, "transient");
    assert_eq!(r.zones[0].stock.len(), 2);
    assert_eq!(r.zones[0].stock.get("L4-1-24G"), Some(&Stock::Unknown));
    assert_eq!(
        r.zones[0].stock.get("COMPUTE3-X8C-16G"),
        Some(&Stock::Unknown)
    );
    let row = r.to_status_row("p-1", 1, chrono::Utc::now());
    assert_eq!(row.stock["fr-par-2"]["L4-1-24G"], "unknown");
    // The other reads still ran.
    assert_eq!(r.prices.get("L4-1-24G"), Some(&0.7875));
    assert_eq!(r.zones[0].instances_running, Some(0));
}
