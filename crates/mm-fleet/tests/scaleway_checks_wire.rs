//! `ScalewayChecker` and the three read-only `ScalewayProvider` calls behind it, against a
//! stand-in Scaleway. The list routes copy the shapes `tests/scaleway_wire.rs` pins
//! (instance API: total in the `x-total-count` header; block API: `total_count` in the
//! body), because `Provider::list` reads the block volumes first and then the servers.
//! The two public product routes page the way the live API does: alphabetically, 50 per
//! page unless `per_page` (at most 100) says otherwise, the total in `x-total-count`.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
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
    /// The public products route answers 500.
    products_down: bool,
    /// Types listed before the real ones (`A-FILLER-0000`...), as a zone with many types
    /// has: with 110 of them, every real type is on page 2 at 100 per page.
    filler_types: usize,
    /// This page (1-based) of either product route answers 500.
    fail_product_page: Option<usize>,
    /// Lie in `x-total-count` on the product routes, as a list changing under a reader.
    product_total_override: Option<usize>,
    /// Send no `x-total-count` on the product routes.
    product_no_total: bool,
    /// Serve at most this many per page, whatever `per_page` asks.
    product_page_cap: Option<usize>,
    /// Every product request: (route, `page`, `per_page`) as sent.
    product_requests: Vec<(&'static str, Option<String>, Option<String>)>,
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
fn internal_error() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"type": "internal_error", "message": "boom"})),
    )
        .into_response()
}

/// One page of a product map: the real entries plus the fillers, in name order, `per_page`
/// (default 50, at most 100) from `page`, with the total in `x-total-count`.
fn product_page(
    k: &mut Knobs,
    route: &'static str,
    q: &HashMap<String, String>,
    real: Value,
    filler: Value,
) -> axum::response::Response {
    k.product_requests
        .push((route, q.get("page").cloned(), q.get("per_page").cloned()));
    let page: usize = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
    let per_page: usize = q
        .get("per_page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
        .min(100)
        .min(k.product_page_cap.unwrap_or(usize::MAX));
    if k.fail_product_page == Some(page) {
        return internal_error();
    }
    let mut all: BTreeMap<String, Value> = serde_json::from_value(real).unwrap();
    for i in 0..k.filler_types {
        all.insert(format!("A-FILLER-{i:04}"), filler.clone());
    }
    let total = k.product_total_override.unwrap_or(all.len());
    let slice: serde_json::Map<String, Value> = all
        .into_iter()
        .skip(page.saturating_sub(1) * per_page)
        .take(per_page)
        .collect();
    if k.product_no_total {
        return Json(json!({ "servers": slice })).into_response();
    }
    (
        [("x-total-count", total.to_string())],
        Json(json!({ "servers": slice })),
    )
        .into_response()
}

async fn availability(
    State(s): State<Shared>,
    Path(_zone): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    if k.availability_down {
        return internal_error();
    }
    product_page(
        &mut k,
        "availability",
        &q,
        json!({"L4-1-24G": {"availability": "scarce"}, "COMPUTE3-X8C-16G": {"availability": "available"}}),
        json!({"availability": "available"}),
    )
}
async fn products(
    State(s): State<Shared>,
    Path(_zone): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    if k.products_down {
        return internal_error();
    }
    product_page(
        &mut k,
        "products",
        &q,
        json!({
            "L4-1-24G": {"hourly_price": 0.7875, "network": {"sum_internet_bandwidth": 2000000000}},
            "COMPUTE3-X8C-16G": {"hourly_price": 0.3, "network": {"sum_internet_bandwidth": 2000000000}}
        }),
        json!({"hourly_price": 0.01}),
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

fn product_requests(knobs: &Shared) -> Vec<(&'static str, Option<String>, Option<String>)> {
    knobs.lock().unwrap().product_requests.clone()
}

fn page(route: &'static str, n: &str) -> (&'static str, Option<String>, Option<String>) {
    (route, Some(n.to_string()), Some("100".to_string()))
}

/// `fr-par-2` lists 134 types and every `L4-*` is on page 2 (live, 2026-10-10): a reader of
/// page 1 alone reported the dashboard's default size as `unknown` with no price.
#[tokio::test]
async fn a_size_on_the_second_page_of_the_products_has_its_stock_and_price() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().filler_types = 110;
    let r = checker(base, &["fr-par-2"]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.zones[0].stock.get("L4-1-24G"), Some(&Stock::Scarce));
    assert_eq!(r.prices.get("L4-1-24G"), Some(&0.7875));
    // At the page-size ceiling, every page, and nothing past the total.
    assert_eq!(
        product_requests(&knobs),
        [
            page("availability", "1"),
            page("availability", "2"),
            page("products", "1"),
            page("products", "2"),
        ]
    );
}

#[tokio::test]
async fn the_provider_reads_every_page_of_both_product_lists() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().filler_types = 110;
    let stock = provider(&base).availability().await.unwrap();
    assert_eq!(stock.len(), 112);
    assert_eq!(stock.get("L4-1-24G"), Some(&Stock::Scarce));
    let prices = provider(&base).hourly_prices().await.unwrap();
    assert_eq!(prices.len(), 112);
    assert_eq!(prices.get("L4-1-24G"), Some(&0.7875));
    let caps = provider(&base).sku_bandwidth_mbps().await.unwrap();
    assert_eq!(caps.get("L4-1-24G"), Some(&2000));
}

/// A page that fails fails the whole read, as a failed single read does: the sizes are
/// `unknown`, never taken from the pages that did answer.
#[tokio::test]
async fn a_failed_second_page_fails_the_whole_read() {
    let (base, knobs) = fake().await;
    {
        let mut k = knobs.lock().unwrap();
        k.filler_types = 110;
        k.fail_product_page = Some(2);
    }
    let mut c = checker(base, &["fr-par-2"]);
    c.zones[0].1 = vec!["A-FILLER-0000".into(), "L4-1-24G".into()];
    let r = c.check().await;
    assert_eq!(r.state, CheckState::Unknown, "{:?}", r.last_error);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert!(r.zones[0].stock.values().all(|s| *s == Stock::Unknown));
    assert!(r.prices.is_empty(), "{:?}", r.prices);
}

/// A page that comes back empty before `x-total-count` ends the read: the list changed
/// under the reader, so it is an outage to retry, and no size is called unoffered from a
/// list that was never read in full.
#[tokio::test]
async fn an_empty_page_before_the_total_ends_the_read_as_an_outage() {
    let (base, knobs) = fake().await;
    {
        let mut k = knobs.lock().unwrap();
        k.filler_types = 110;
        k.product_total_override = Some(500);
    }
    let mut c = checker(base, &["fr-par-2"]);
    c.zones[0].1 = vec!["L4-1-24G".into(), "L4-1-24".into()];
    let r = c.check().await;
    assert_eq!(r.state, CheckState::Unknown, "{:?}", r.last_error);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert!(r.zones[0].stock.values().all(|s| *s == Stock::Unknown));
    // Pages 1 and 2 hold the 112 types; page 3 is empty and ends each read.
    let pages: Vec<_> = product_requests(&knobs)
        .into_iter()
        .filter(|(route, _, _)| *route == "availability")
        .collect();
    assert_eq!(
        pages,
        [
            page("availability", "1"),
            page("availability", "2"),
            page("availability", "3"),
        ]
    );
}

/// Ten pages is the bound: a total that keeps the reader paging past it is an outage, not
/// a partial list.
#[tokio::test]
async fn a_product_list_longer_than_ten_pages_is_an_outage() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().filler_types = 1_050;
    let r = checker(base, &["fr-par-2"]).check().await;
    assert_eq!(r.state, CheckState::Unknown, "{:?}", r.last_error);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock.get("L4-1-24G"), Some(&Stock::Unknown));
    let requests = product_requests(&knobs);
    for route in ["availability", "products"] {
        assert_eq!(
            requests.iter().filter(|(r, _, _)| *r == route).count(),
            10,
            "{route}"
        );
    }
}

/// Scaleway lists only the types a zone has, so once its whole list has been read a size
/// absent from it is a configuration error (a typo, or a GPU this zone has none of), not
/// a stock signal.
#[tokio::test]
async fn a_size_the_zone_does_not_offer_is_needs_you_and_stays_unknown() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().filler_types = 110;
    let mut c = checker(base, &["fr-par-2"]);
    c.zones[0].1 = vec!["L4-1-24".into(), "L4-1-24G".into()];
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: size L4-1-24 is not offered in zone fr-par-2"
    );
    assert_eq!(r.zones[0].stock.get("L4-1-24"), Some(&Stock::Unknown));
    assert!(!r.prices.contains_key("L4-1-24"));
    // The size the zone has is still mapped.
    assert_eq!(r.zones[0].stock.get("L4-1-24G"), Some(&Stock::Scarce));
    assert_eq!(r.prices.get("L4-1-24G"), Some(&0.7875));
}

/// Without `x-total-count`, a page shorter than asked proves nothing (the API may serve
/// smaller pages): the read goes on until an empty page, so a size on a later page is found.
#[tokio::test]
async fn without_a_total_the_product_lists_are_read_to_an_empty_page() {
    let (base, knobs) = fake().await;
    {
        let mut k = knobs.lock().unwrap();
        k.filler_types = 110;
        k.product_no_total = true;
        k.product_page_cap = Some(40);
    }
    let r = checker(base, &["fr-par-2"]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.zones[0].stock.get("L4-1-24G"), Some(&Stock::Scarce));
    assert_eq!(r.prices.get("L4-1-24G"), Some(&0.7875));
}

/// The size goes to create as typed, so `l4-1-24g` is not offered, but the message names the id
/// that differs only in case.
#[tokio::test]
async fn a_size_that_differs_only_in_case_names_the_real_id() {
    let (base, _) = fake().await;
    let mut c = checker(base, &["fr-par-2"]);
    c.zones[0].1 = vec!["l4-1-24g".into()];
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    assert_eq!(
        r.last_error.clone().unwrap().1,
        "permanent provider failure: size l4-1-24g is not offered in zone fr-par-2 — Scaleway's id \
         is L4-1-24G (case matters)"
    );
    assert_eq!(r.zones[0].stock.get("l4-1-24g"), Some(&Stock::Unknown));
}

/// Either list read in full says what the zone offers: a size in neither is unoffered
/// even while the other list is down, and a size in one of them never is.
#[tokio::test]
async fn one_list_read_in_full_is_enough_to_call_a_size_unoffered() {
    for availability_down in [true, false] {
        let (base, knobs) = fake().await;
        {
            let mut k = knobs.lock().unwrap();
            k.availability_down = availability_down;
            k.products_down = !availability_down;
        }
        let mut c = checker(base, &["fr-par-2"]);
        c.zones[0].1 = vec!["COMPUTE3-X8C-16G".into(), "L4-1-24".into()];
        let r = c.check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{availability_down}");
        let (_, msg) = r.last_error.clone().unwrap();
        assert!(
            msg.contains("size L4-1-24 is not offered in zone fr-par-2"),
            "{availability_down}: {msg}"
        );
        assert!(!msg.contains("COMPUTE3"), "{availability_down}: {msg}");
    }
}

/// A list that could not be read says nothing about what the zone offers: as before, the
/// sizes are `unknown` and the outage is what is reported.
#[tokio::test]
async fn no_size_is_called_unoffered_when_neither_list_could_be_read() {
    let (base, knobs) = fake().await;
    {
        let mut k = knobs.lock().unwrap();
        k.availability_down = true;
        k.products_down = true;
    }
    let mut c = checker(base, &["fr-par-2"]);
    c.zones[0].1 = vec!["L4-1-24".into()];
    let r = c.check().await;
    assert_eq!(r.state, CheckState::Unknown, "{:?}", r.last_error);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock.get("L4-1-24"), Some(&Stock::Unknown));
}

/// `https://api.scaleway.com/` would build `//instance/...`, which Scaleway answers with a
/// 401: a false "key rejected".
#[tokio::test]
async fn an_endpoint_with_a_trailing_slash_reads_the_same_paths() {
    for slashes in ["/", "//"] {
        let (base, _) = fake().await;
        let r = checker(format!("{base}{slashes}"), &["fr-par-2"])
            .check()
            .await;
        assert_eq!(r.state, CheckState::Ok, "{slashes}: {:?}", r.last_error);
        assert_eq!(r.zones[0].stock.get("L4-1-24G"), Some(&Stock::Scarce));
        assert_eq!(r.prices.get("L4-1-24G"), Some(&0.7875));
        assert_eq!(r.zones[0].instances_running, Some(0));
    }
}
