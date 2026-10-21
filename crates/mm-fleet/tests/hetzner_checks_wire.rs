//! The Hetzner checker against a stand-in Hetzner Cloud API v1. Shapes follow the
//! published reference (`https://docs.hetzner.cloud/reference/cloud`):
//! `GET /servers?per_page=1` for the key check, `GET /locations` (a
//! `{locations: [{name, ...}]}` object with `meta.pagination`), `GET /server_types` (a
//! `{server_types: [{name, deprecated, prices: [{location, price_hourly: {gross}}]}]}`
//! object) and `GET /servers?label_selector=...` for the per-zone count. Prices are
//! decimal strings, as the real API sends them.
//!
//! What these do NOT prove: that the real Hetzner behaves as its docs describe.
//! That claim transfers only against a real account.

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

const TOKEN: &str = "HETZNER-TEST-TOKEN-77b2e0";

const SMALL: &str = "cx22";
const MEDIUM: &str = "cx32";
const DEPRECATED: &str = "cx11";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    auth: Option<String>,
    label_selector: Option<String>,
}

#[derive(Default)]
struct Knobs {
    seen: Vec<Seen>,
    /// path -> (status, body) answered instead of the default.
    overrides: HashMap<String, (u16, String)>,
    /// When set, `/servers` lists exactly these (the default fixture has none).
    servers: Option<Vec<Value>>,
}
type Shared = Arc<Mutex<Knobs>>;

fn server(id: u64, location: &str, labels: Value) -> Value {
    json!({
        "id": id, "name": "bc-b1-fanout-0", "status": "running",
        "created": "2026-10-20T12:00:00Z",
        "public_net": {"ipv4": {"ip": "203.0.113.7", "blocked": false, "dns_ptr": ""}},
        "server_type": {"id": 22, "name": SMALL},
        "datacenter": {"name": "fsn1-dc14", "location": {"name": location, "country": "DE"}},
        "image": {"name": "ubuntu-24.04"},
        "labels": labels,
    })
}

fn server_types() -> Value {
    json!({
        "server_types": [
            {"id": 22, "name": SMALL, "deprecated": false, "prices": [
                {"location": "fsn1", "price_hourly": {"net": "0.0049", "gross": "0.005831"},
                 "price_monthly": {"net": "2.90", "gross": "3.451"}},
                {"location": "nbg1", "price_hourly": {"net": "0.005", "gross": "0.006"},
                 "price_monthly": {"net": "2.95", "gross": "3.5105"}}
            ]},
            {"id": 23, "name": MEDIUM, "deprecated": false, "prices": [
                {"location": "fsn1", "price_hourly": {"net": "0.0098", "gross": "0.011662"},
                 "price_monthly": {"net": "5.80", "gross": "6.902"}}
            ]},
            {"id": 20, "name": DEPRECATED, "deprecated": true, "prices": [
                {"location": "fsn1", "price_hourly": {"net": "0.004", "gross": "0.00476"},
                 "price_monthly": {"net": "2.49", "gross": "2.9631"}}
            ]}
        ],
        "meta": {"pagination": {"page": 1, "per_page": 50, "total_entries": 3}}
    })
}

fn locations() -> Value {
    json!({
        "locations": [
            {"id": 1, "name": "fsn1", "country": "DE"},
            {"id": 2, "name": "nbg1", "country": "DE"}
        ],
        "meta": {"pagination": {"page": 1, "per_page": 50, "total_entries": 2}}
    })
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
    let query = uri.query().unwrap_or("").to_string();
    let over = {
        let mut k = s.lock().unwrap();
        let selector = uri.query().and_then(|q| {
            q.split('&')
                .find_map(|p| p.strip_prefix("label_selector=").map(|v| v.to_string()))
        });
        k.seen.push(Seen {
            method: method.to_string(),
            path: path.clone(),
            query: query.clone(),
            auth: auth.clone(),
            label_selector: selector,
        });
        k.overrides.get(&path).cloned()
    };
    if let Some((status, body)) = over {
        return (StatusCode::from_u16(status).unwrap(), body).into_response();
    }
    if auth.as_deref() != Some(&format!("Bearer {TOKEN}")) {
        return (
            StatusCode::UNAUTHORIZED,
            json!({"error": {"code": "unauthorized", "message": "bad token"}}).to_string(),
        )
            .into_response();
    }
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segs.as_slice() {
        ["servers"] => {
            let servers = s
                .lock()
                .unwrap()
                .servers
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|srv| {
                    // The stand-in honors the fleet-tag half of the selector, the way the
                    // real API does; the client enforces the location half itself.
                    let tagged = srv
                        .get("labels")
                        .and_then(|l| l.get("mm-fleet-api"))
                        .and_then(|v| v.as_str())
                        == Some("mm-fleet-api");
                    !query.contains("label_selector") || tagged
                })
                .collect::<Vec<_>>();
            let total = servers.len();
            axum::Json(json!({
                "servers": servers,
                "meta": {"pagination": {"page": 1, "per_page": 50, "total_entries": total}}
            }))
            .into_response()
        }
        ["server_types"] => axum::Json(server_types()).into_response(),
        ["locations"] => axum::Json(locations()).into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            json!({"error": {"code": "not_found", "message": "nope"}}).to_string(),
        )
            .into_response(),
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

fn pt(token: Option<&str>) -> CredentialPlaintext {
    CredentialPlaintext {
        v: 1,
        provider_id: "p-1".into(),
        kind: "hetzner".into(),
        endpoint: "https://api.hetzner.cloud/v1".into(),
        account: None,
        fields: token
            .map(|t| [("token".to_string(), t.to_string())].into())
            .unwrap_or_default(),
    }
}

fn zones(spec: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    spec.iter()
        .map(|(z, sizes)| (z.to_string(), sizes.iter().map(|s| s.to_string()).collect()))
        .collect()
}

fn checker(base: &str, spec: &[(&str, &[&str])]) -> Box<dyn ProviderChecker> {
    mm_fleet::hetzner::checker(&pt(Some(TOKEN)), zones(spec), Some(base))
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
async fn ok_path_maps_stock_hourly_prices_and_the_zone_count() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().servers = Some(vec![
        server(
            1,
            "fsn1",
            json!({"mm-fleet-api": "mm-fleet-api", "mm-node-id": "bc-b1-fanout-0"}),
        ),
        server(
            2,
            "nbg1",
            json!({"mm-fleet-api": "mm-fleet-api", "mm-node-id": "bc-b1-fanout-1"}),
        ),
        server(3, "fsn1", json!({"other": "true"})),
    ]);
    let r = checker(&base, &[("fsn1", &[SMALL])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.last_error, None);
    assert_eq!(r.key_scope, None);
    assert_eq!(r.balance_minor, None);
    assert_eq!(r.zones.len(), 1);
    assert_eq!(r.zones[0].zone, "fsn1");
    // Only the fsn1 machine carrying our tag is counted: not nbg1's, not someone else's.
    assert_eq!(r.zones[0].instances_running, Some(1));
    assert_eq!(r.zones[0].stock[SMALL], Stock::Available);
    // Hourly gross, in EUR, for this location — never the monthly figure.
    assert_eq!(r.prices[SMALL], 0.005831);
    let row = r.to_status_row("p-1", 1, chrono::Utc::now());
    assert_eq!(row.state, "ok");
    assert_eq!(row.stock["fsn1"][SMALL], "available");
}

#[tokio::test]
async fn prices_are_hourly_gross_per_location() {
    let (base, _) = fake().await;
    // nbg1 first in failover order: its location price wins.
    let r = checker(&base, &[("nbg1", &[SMALL]), ("fsn1", &[SMALL])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.prices[SMALL], 0.006);
    let r = checker(&base, &[("fsn1", &[SMALL, MEDIUM])]).check().await;
    assert_eq!(r.prices[SMALL], 0.005831);
    assert_eq!(r.prices[MEDIUM], 0.011662);
}

#[tokio::test]
async fn a_deprecated_type_is_a_shortage_not_a_page() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("fsn1", &[DEPRECATED])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.zones[0].stock[DEPRECATED], Stock::Shortage);
    // A deprecated type still prices: it may yet bill an hour.
    assert_eq!(r.prices[DEPRECATED], 0.00476);
}

#[tokio::test]
async fn an_unknown_type_is_needs_you_with_a_plain_message() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("fsn1", &["cx99-nope"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: server type cx99-nope is not offered"
    );
    assert!(r.prices.is_empty());
}

#[tokio::test]
async fn an_unknown_location_is_needs_you_and_the_other_zones_are_still_read() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("xx-nope", &[SMALL]), ("fsn1", &[SMALL])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: location xx-nope does not exist"
    );
    assert_eq!(r.zones.len(), 2);
    assert_eq!(r.zones[1].stock[SMALL], Stock::Available);
    assert_eq!(r.prices[SMALL], 0.005831);
}

#[tokio::test]
async fn requests_are_reads_to_the_documented_paths_with_the_bearer_token_on_every_call() {
    let (base, knobs) = fake().await;
    let _ = checker(&base, &[("fsn1", &[SMALL, MEDIUM])]).check().await;
    let seen = knobs.lock().unwrap().seen.clone();
    for s in &seen {
        assert_eq!(s.method, "GET", "a check never writes: {s:?}");
        assert_eq!(
            s.auth.as_deref(),
            Some(format!("Bearer {TOKEN}").as_str()),
            "{s:?}"
        );
    }
    let paths: Vec<String> = seen.iter().map(|s| s.path.clone()).collect();
    assert_eq!(
        paths,
        ["/servers", "/server_types", "/locations", "/servers",],
        "the catalogue reads once, the key check and the count around them"
    );
    assert!(seen[0].query.contains("per_page=1"), "{:?}", seen[0]);
    assert!(
        seen[3]
            .label_selector
            .as_deref()
            .unwrap()
            .contains("mm-fleet-api"),
        "{:?}",
        seen[3]
    );
}

/// An endpoint saved with a trailing slash reads the same paths: a doubled `/`
/// would be refused as if the token were bad.
#[tokio::test]
async fn an_endpoint_with_a_trailing_slash_builds_the_same_paths() {
    let (base, _) = fake().await;
    let r = checker(&format!("{base}/"), &[("fsn1", &[SMALL])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.zones[0].stock[SMALL], Stock::Available);
    assert_eq!(r.prices[SMALL], 0.005831);
}

#[tokio::test]
async fn a_rejected_token_is_needs_you_with_the_status_line_and_no_secret() {
    let (base, knobs) = fake().await;
    set(
        &knobs,
        "/servers",
        401,
        &format!("{{\"error\":{{\"code\":\"unauthorized\",\"message\":\"token {TOKEN} bad\"}}}}"),
    );
    let r = checker(&base, &[("fsn1", &[SMALL])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: 401 Unauthorized: key rejected"
    );
    assert!(!msg.contains(TOKEN), "{msg}");
    assert!(!msg.contains("bad"), "the body is discarded: {msg}");
    assert_eq!(calls(&knobs), 1, "nothing else is asked of a refused token");
    assert_eq!(r.zones[0].stock[SMALL], Stock::Unknown);
}

#[tokio::test]
async fn a_provider_error_on_verify_is_unknown_and_transient() {
    for status in [500u16, 503, 429, 408] {
        let (base, knobs) = fake().await;
        set(&knobs, "/servers", status, "busy");
        let r = checker(&base, &[("fsn1", &[SMALL, MEDIUM])]).check().await;
        assert_eq!(r.state, CheckState::Unknown, "{status}");
        assert_eq!(r.last_error.as_ref().unwrap().0, "transient", "{status}");
        assert_eq!(
            r.zones[0].stock.len(),
            2,
            "every configured size is reported"
        );
        assert!(r.zones[0].stock.values().all(|s| *s == Stock::Unknown));
    }
}

#[tokio::test]
async fn a_failed_catalogue_read_marks_every_size_unknown_and_prices_nothing() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().servers = Some(vec![server(
        1,
        "fsn1",
        json!({"mm-fleet-api": "mm-fleet-api", "mm-node-id": "bc-b1-fanout-0"}),
    )]);
    set(&knobs, "/server_types", 502, "bad gateway");
    let r = checker(&base, &[("fsn1", &[SMALL])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock[SMALL], Stock::Unknown);
    assert!(r.prices.is_empty());
    // Without the catalogue there is no stock signal and no price — but the location
    // list and the count still ran.
    assert_eq!(r.zones[0].instances_running, Some(1));
}
