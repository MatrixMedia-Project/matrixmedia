//! The RunPod checker against a stand-in RunPod. Shapes follow the published docs:
//! REST v1 `GET /pods` (a top-level array) for the key, REST v2 `GET /v2/catalog/datacenters
//! ?include=GPU_AVAILABILITY` and `GET /v2/catalog/gpus` (host `api.runpod.io`, here
//! `{base}/v2host`) for stock and prices. The checker never gets a database or a real host.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use mm_fleet::checks::{CheckState, ProviderChecker, Stock};
use mm_fleet::sealed::CredentialPlaintext;

const KEY: &str = "RP-TEST-KEY-9f3a1c";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    auth: Option<String>,
}

#[derive(Default)]
struct Knobs {
    seen: Vec<Seen>,
    /// path -> (status, body) answered instead of the default.
    overrides: HashMap<String, (u16, String)>,
}
type Shared = Arc<Mutex<Knobs>>;

const PODS: &str = "/pods";
const DATACENTERS: &str = "/v2host/v2/catalog/datacenters";
const GPUS: &str = "/v2host/v2/catalog/gpus";

fn datacenters() -> Value {
    json!({"dataCenters": [
        {"id": "EU-RO-1", "name": "Europe Romania", "region": "EUROPE", "globalNetwork": true,
         "networkVolumeTypes": [], "compliance": [],
         "gpuAvailability": [
            {"id": "NVIDIA L4", "name": "L4", "availability": "HIGH"},
            {"id": "NVIDIA A40", "name": "A40", "availability": "LOW"},
            {"id": "NVIDIA RTX A4000", "name": "RTX A4000", "availability": "MEDIUM"},
            {"id": "NVIDIA H100 80GB HBM3", "name": "H100", "availability": "NONE"},
            {"id": "NVIDIA RTX A5000", "name": "RTX A5000", "availability": "SOMETHING_NEW"}
         ]},
        {"id": "US-KS-2", "name": "US Kansas 2", "region": "NORTH_AMERICA", "globalNetwork": true,
         "networkVolumeTypes": [], "compliance": [],
         "gpuAvailability": [{"id": "NVIDIA L4", "name": "L4", "availability": "NONE"}]},
        // No GPUs at all: the API omits `gpuAvailability`.
        {"id": "CA-MTL-1", "name": "Canada Montreal", "region": "NORTH_AMERICA",
         "globalNetwork": false, "networkVolumeTypes": [], "compliance": []}
    ]})
}

fn gpus() -> Value {
    json!({"gpus": [
        {"id": "NVIDIA L4", "name": "L4", "pool": null, "manufacturer": "NVIDIA", "memory": 24,
         "secure": true, "community": true,
         "price": {"secure": 0.43, "community": 0.32}, "maxCount": {"secure": 8, "community": 4}},
        {"id": "NVIDIA A40", "name": "A40", "pool": null, "manufacturer": "NVIDIA", "memory": 48,
         "secure": true, "community": false,
         "price": {"secure": 0.4, "community": 0.35}, "maxCount": {"secure": 8, "community": 0}},
        {"id": "NVIDIA H100 80GB HBM3", "name": "H100", "pool": null, "manufacturer": "NVIDIA",
         "memory": 80, "secure": true, "community": false,
         "price": {"secure": 2.69, "community": 0.0}, "maxCount": {"secure": 8, "community": 0}},
        // Known to the catalogue but listed in no data centre of the fixture.
        {"id": "NVIDIA GeForce RTX 4090", "name": "RTX 4090", "pool": null,
         "manufacturer": "NVIDIA", "memory": 24, "secure": true, "community": true,
         "price": {"secure": 0.69, "community": 0.34}, "maxCount": {"secure": 8, "community": 4}},
        // A community-only GPU: the API documents `price.secure` as a number anyway, and 0.0
        // is a placeholder, not a price.
        {"id": "NVIDIA RTX A4000", "name": "RTX A4000", "pool": null, "manufacturer": "NVIDIA",
         "memory": 16, "secure": false, "community": true,
         "price": {"secure": 0.0, "community": 0.2}, "maxCount": {"secure": 0, "community": 4}},
        {"id": "NVIDIA RTX A5000", "name": "RTX A5000", "pool": null, "manufacturer": "NVIDIA",
         "memory": 24, "secure": true, "community": true,
         "price": {"secure": null, "community": 0.2}, "maxCount": {"secure": 8, "community": 4}}
    ]})
}

async fn handle(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> axum::response::Response {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = uri.path().to_string();
    let over = {
        let mut k = s.lock().unwrap();
        k.seen.push(Seen {
            method: method.to_string(),
            path: path.clone(),
            query: uri.query().unwrap_or("").to_string(),
            auth: auth.clone(),
        });
        k.overrides.get(&path).cloned()
    };
    if let Some((status, body)) = over {
        return (StatusCode::from_u16(status).unwrap(), body).into_response();
    }
    if auth.as_deref() != Some(&format!("Bearer {KEY}")) {
        return (
            StatusCode::UNAUTHORIZED,
            json!({"title": "Unauthorized", "status": 401, "detail": format!("bad key {}", auth.unwrap_or_default())}).to_string(),
        )
            .into_response();
    }
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    match path.as_str() {
        PODS => axum::Json(json!([])).into_response(),
        DATACENTERS => axum::Json(datacenters()).into_response(),
        GPUS => axum::Json(gpus()).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn fake() -> (String, Shared) {
    let state: Shared = Arc::default();
    let app = Router::new().fallback(handle).with_state(state.clone());
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), state)
}

fn pt(key: Option<&str>) -> CredentialPlaintext {
    CredentialPlaintext {
        v: 1,
        provider_id: "p-1".into(),
        kind: "runpod".into(),
        endpoint: "https://rest.runpod.io/v1".into(),
        account: None,
        fields: key
            .map(|k| [("api_key".to_string(), k.to_string())].into())
            .unwrap_or_default(),
    }
}

fn zones(spec: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    spec.iter()
        .map(|(z, sizes)| (z.to_string(), sizes.iter().map(|s| s.to_string()).collect()))
        .collect()
}

fn checker(base: &str, spec: &[(&str, &[&str])]) -> Box<dyn ProviderChecker> {
    mm_fleet::runpod::checker(&pt(Some(KEY)), zones(spec), Some(base))
}

fn set(knobs: &Shared, path: &str, status: u16, body: &str) {
    knobs
        .lock()
        .unwrap()
        .overrides
        .insert(path.to_string(), (status, body.to_string()));
}

fn calls(knobs: &Shared) -> usize {
    knobs.lock().unwrap().seen.len()
}

#[tokio::test]
async fn ok_path_maps_stock_and_secure_prices() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4", "NVIDIA A40"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.last_error, None);
    assert_eq!(r.key_scope, None);
    assert_eq!(r.balance_minor, None, "REST exposes no balance");
    assert_eq!(r.zones.len(), 1);
    assert_eq!(
        r.zones[0].zone, "eu-ro-1",
        "the zone keeps the server's spelling"
    );
    assert_eq!(r.zones[0].instances_running, None);
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Available);
    assert_eq!(r.zones[0].stock["NVIDIA A40"], Stock::Scarce);
    // `secure`, not `community`.
    assert_eq!(r.prices["NVIDIA L4"], 0.43);
    assert_eq!(r.prices["NVIDIA A40"], 0.4);
    let row = r.to_status_row("p-1", 1, chrono::Utc::now());
    assert_eq!(row.state, "ok");
    assert_eq!(row.stock["eu-ro-1"]["NVIDIA L4"], "available");
}

#[tokio::test]
async fn every_availability_level_maps_and_an_absent_gpu_is_a_shortage() {
    let (base, _) = fake().await;
    let sizes = [
        "NVIDIA L4",               // HIGH
        "NVIDIA A40",              // LOW
        "NVIDIA RTX A4000",        // MEDIUM
        "NVIDIA H100 80GB HBM3",   // NONE
        "NVIDIA GeForce RTX 4090", // not listed in this data centre
        "NVIDIA RTX A5000",        // a level this checker does not know
    ];
    let r = checker(&base, &[("eu-ro-1", &sizes)]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let s = &r.zones[0].stock;
    assert_eq!(s["NVIDIA L4"], Stock::Available);
    assert_eq!(s["NVIDIA A40"], Stock::Scarce);
    assert_eq!(s["NVIDIA RTX A4000"], Stock::Scarce);
    assert_eq!(s["NVIDIA H100 80GB HBM3"], Stock::Shortage);
    assert_eq!(s["NVIDIA GeForce RTX 4090"], Stock::Shortage);
    assert_eq!(
        s["NVIDIA RTX A5000"],
        Stock::Unknown,
        "never invent a level"
    );
}

#[tokio::test]
async fn a_data_centre_with_no_gpus_is_a_shortage_for_every_size() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("ca-mtl-1", &["NVIDIA L4"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Shortage);
}

#[tokio::test]
async fn each_zone_is_read_in_its_own_data_centre_and_failover_order_is_kept() {
    let (base, knobs) = fake().await;
    let r = checker(
        &base,
        &[("us-ks-2", &["NVIDIA L4"]), ("eu-ro-1", &["NVIDIA L4"])],
    )
    .check()
    .await;
    assert_eq!(r.zones[0].zone, "us-ks-2");
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Shortage);
    assert_eq!(r.zones[1].zone, "eu-ro-1");
    assert_eq!(r.zones[1].stock["NVIDIA L4"], Stock::Available);
    // The catalog is read once for the whole check, not once per zone.
    assert_eq!(calls(&knobs), 3);
}

#[tokio::test]
async fn requests_are_reads_to_the_documented_paths_with_the_bearer_key() {
    let (base, knobs) = fake().await;
    let _ = checker(&base, &[("eu-ro-1", &["NVIDIA L4"])]).check().await;
    let seen = knobs.lock().unwrap().seen.clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    for s in &seen {
        assert_eq!(s.method, "GET", "a check never writes: {s:?}");
        assert_eq!(
            s.auth.as_deref(),
            Some(format!("Bearer {KEY}").as_str()),
            "{s:?}"
        );
    }
    let paths: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
    // The GPU catalogue is read before the data centres: it says which sizes exist at all.
    assert_eq!(paths, [PODS, GPUS, DATACENTERS]);
    assert_eq!(seen[2].query, "include=GPU_AVAILABILITY");
}

#[tokio::test]
async fn a_rejected_key_is_needs_you_with_the_status_line_and_no_secret() {
    for status in [401u16, 403] {
        let (base, knobs) = fake().await;
        // A provider that echoes the key in its rejection.
        set(
            &knobs,
            PODS,
            status,
            &format!("{{\"detail\":\"key {KEY} unknown\"}}"),
        );
        let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4"])]).check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{status}");
        let (kind, msg) = r.last_error.clone().unwrap();
        assert_eq!(kind, "permanent");
        assert!(msg.contains(&status.to_string()), "{msg}");
        assert!(msg.contains("key rejected"), "{msg}");
        assert!(!msg.contains(KEY), "{msg}");
        assert!(!msg.contains("unknown"), "the body is discarded: {msg}");
        // The catalog is not read with a key that was just refused.
        assert_eq!(calls(&knobs), 1);
        assert_eq!(r.zones.len(), 1);
        assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Unknown);
    }
}

#[tokio::test]
async fn a_key_the_catalog_refuses_is_needs_you() {
    let (base, knobs) = fake().await;
    set(
        &knobs,
        DATACENTERS,
        403,
        &format!("{{\"detail\":\"{KEY} lacks scope\"}}"),
    );
    let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(msg.contains("403") && !msg.contains(KEY), "{msg}");
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Unknown);
    // Prices come from another route and are still reported.
    assert_eq!(r.prices["NVIDIA L4"], 0.43);
}

#[tokio::test]
async fn a_provider_error_on_the_key_check_is_unknown_and_transient() {
    for status in [500u16, 502, 429, 408] {
        let (base, knobs) = fake().await;
        set(&knobs, PODS, status, "upstream down");
        let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4", "NVIDIA A40"])])
            .check()
            .await;
        assert_eq!(r.state, CheckState::Unknown, "{status}");
        let (kind, _) = r.last_error.clone().unwrap();
        assert_eq!(kind, "transient", "{status}");
        assert_eq!(
            r.zones[0].stock.len(),
            2,
            "every configured size is reported"
        );
        assert!(r.zones[0].stock.values().all(|s| *s == Stock::Unknown));
    }
}

#[tokio::test]
async fn a_failed_stock_read_marks_every_size_unknown_and_keeps_prices() {
    let (base, knobs) = fake().await;
    set(&knobs, DATACENTERS, 503, "down");
    let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4", "NVIDIA A40"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock.len(), 2);
    assert!(r.zones[0].stock.values().all(|s| *s == Stock::Unknown));
    assert_eq!(r.prices["NVIDIA L4"], 0.43);
}

#[tokio::test]
async fn a_failed_price_read_keeps_stock_and_leaves_prices_empty() {
    let (base, knobs) = fake().await;
    set(&knobs, GPUS, 500, "down");
    let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4"])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Available);
    assert!(r.prices.is_empty());
}

#[tokio::test]
async fn a_gpu_without_a_secure_price_has_no_price() {
    let (base, _) = fake().await;
    let r = checker(
        &base,
        &[("eu-ro-1", &["NVIDIA RTX A5000", "NVIDIA RTX A4000"])],
    )
    .check()
    .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert!(r.prices.is_empty(), "{:?}", r.prices);
}

#[tokio::test]
async fn an_unknown_data_centre_is_needs_you_and_the_other_zones_are_still_read() {
    let (base, _) = fake().await;
    let r = checker(
        &base,
        &[("xx-nope-9", &["NVIDIA L4"]), ("eu-ro-1", &["NVIDIA L4"])],
    )
    .check()
    .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: data centre XX-NOPE-9 does not exist"
    );
    assert_eq!(r.zones.len(), 2);
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Unknown);
    assert_eq!(r.zones[1].stock["NVIDIA L4"], Stock::Available);
}

#[tokio::test]
async fn a_needs_you_error_outranks_an_outage_in_either_order() {
    // Transient first, permanent second: the GPU catalogue is down (it is read first), then
    // the data centre turns out not to exist. The page wins.
    let (base, knobs) = fake().await;
    set(&knobs, GPUS, 500, "down");
    let r = checker(&base, &[("xx-nope-9", &["NVIDIA L4"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(msg.contains("XX-NOPE-9"), "{msg}");

    // Permanent first, transient second: the size is unknown to the catalogue, then the data
    // centre read fails. The page still wins.
    let (base, knobs) = fake().await;
    set(&knobs, DATACENTERS, 503, "down");
    let r = checker(&base, &[("eu-ro-1", &["L4"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(msg.contains("GPU type L4 does not exist"), "{msg}");
}

#[tokio::test]
async fn a_gpu_type_the_catalogue_does_not_know_is_needs_you_and_unknown_in_every_zone() {
    let (base, _) = fake().await;
    let r = checker(
        &base,
        &[
            ("eu-ro-1", &["L4", "NVIDIA L4"]),
            ("us-ks-2", &["L4", "NVIDIA L4"]),
        ],
    )
    .check()
    .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: GPU type L4 does not exist — use RunPod's GPU type id, e.g. NVIDIA L4"
    );
    // The misspelt size is unknown everywhere, never a shortage; the right one is mapped.
    assert_eq!(r.zones.len(), 2);
    for z in &r.zones {
        assert_eq!(z.stock["L4"], Stock::Unknown, "{}", z.zone);
    }
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Available);
    assert_eq!(r.zones[1].stock["NVIDIA L4"], Stock::Shortage);
    assert!(!r.prices.contains_key("L4"), "{:?}", r.prices);
    assert_eq!(r.prices["NVIDIA L4"], 0.43);
}

#[tokio::test]
async fn the_gpu_type_id_matches_the_catalogue_whatever_its_case() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("eu-ro-1", &["nvidia l4"])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.zones[0].stock["nvidia l4"], Stock::Available);
    assert_eq!(r.prices["nvidia l4"], 0.43);
}

#[tokio::test]
async fn without_the_catalogue_an_absent_gpu_stays_a_shortage_and_the_outage_is_reported() {
    let (base, knobs) = fake().await;
    set(&knobs, GPUS, 500, "down");
    let r = checker(
        &base,
        &[("eu-ro-1", &["NVIDIA L4", "NVIDIA GeForce RTX 4090", "L4"])],
    )
    .check()
    .await;
    // The catalogue could not say whether `L4` exists, so it is not called a typo.
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    let s = &r.zones[0].stock;
    assert_eq!(s["NVIDIA L4"], Stock::Available);
    assert_eq!(s["NVIDIA GeForce RTX 4090"], Stock::Shortage);
    assert_eq!(s["L4"], Stock::Shortage);
    assert!(r.prices.is_empty());
}

#[tokio::test]
async fn an_empty_catalogue_is_no_evidence_that_a_gpu_does_not_exist() {
    let (base, knobs) = fake().await;
    set(&knobs, GPUS, 200, r#"{"gpus": []}"#);
    let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4"])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Available);
}

#[tokio::test]
async fn a_zone_with_no_sizes_reads_no_gpu_catalogue() {
    let (base, knobs) = fake().await;
    let r = checker(&base, &[("eu-ro-1", &[])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let paths: Vec<String> = knobs
        .lock()
        .unwrap()
        .seen
        .iter()
        .map(|s| s.path.clone())
        .collect();
    assert_eq!(paths, [PODS, DATACENTERS]);
}

#[tokio::test]
async fn a_key_a_header_cannot_carry_is_needs_you_and_makes_no_call() {
    let (base, knobs) = fake().await;
    let c = mm_fleet::runpod::checker(
        &pt(Some("RP-BAD\nKEY")),
        zones(&[("eu-ro-1", &["NVIDIA L4"])]),
        Some(&base),
    );
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(
        msg.contains("characters an HTTP header cannot carry"),
        "{msg}"
    );
    assert!(msg.contains("re-enter it"), "{msg}");
    assert!(!msg.contains("RP-BAD"), "{msg}");
    assert_eq!(calls(&knobs), 0, "the request never left");
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Unknown);
}

#[tokio::test]
async fn no_zones_is_the_config_error_and_makes_no_call() {
    let (base, knobs) = fake().await;
    let r = checker(&base, &[]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(
        r.last_error,
        Some(("config".to_string(), "no zones configured".to_string()))
    );
    assert!(r.zones.is_empty());
    assert_eq!(calls(&knobs), 0);
}

#[tokio::test]
async fn a_credential_without_an_api_key_is_needs_you_and_makes_no_call() {
    let (base, knobs) = fake().await;
    let c = mm_fleet::runpod::checker(
        &pt(None),
        zones(&[("eu-ro-1", &["NVIDIA L4"])]),
        Some(&base),
    );
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(msg.contains("api_key"), "{msg}");
    assert_eq!(calls(&knobs), 0);
}

#[tokio::test]
async fn a_provider_body_that_echoes_the_key_is_redacted() {
    for status in [500u16, 400] {
        let (base, knobs) = fake().await;
        set(
            &knobs,
            PODS,
            status,
            &format!("oops Authorization: Bearer {KEY} was bad"),
        );
        let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4"])]).check().await;
        let (_, msg) = r.last_error.clone().unwrap();
        assert!(!msg.contains(KEY), "{status}: {msg}");
        assert!(msg.contains("[redacted]"), "{status}: {msg}");
        assert!(!format!("{:?}", r.to_status_row("p", 1, chrono::Utc::now())).contains(KEY));
    }
}

#[tokio::test]
async fn a_body_that_is_not_the_documented_shape_is_unknown_not_a_panic() {
    let (base, knobs) = fake().await;
    set(&knobs, DATACENTERS, 200, "<html>maintenance</html>");
    let r = checker(&base, &[("eu-ro-1", &["NVIDIA L4"])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock["NVIDIA L4"], Stock::Unknown);
}
