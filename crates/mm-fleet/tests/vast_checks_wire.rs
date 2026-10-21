//! The Vast.ai checker against a stand-in Vast.ai. Shapes follow the public HTTP API
//! (`https://console.vast.ai/api/v0`, the CLI's own endpoints): `GET /users/current` for
//! the key, `GET /gpu_names/unique/` for the GPU names that exist, and
//! `GET /bundles/?q={...}` for the rentable offers per GPU name. The checker never gets a
//! database or a real host, and never rents anything.

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

const KEY: &str = "VAST-TEST-KEY-7c2e9a";

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

fn offers() -> Value {
    json!({"offers": [
        {"id": 11, "machine_id": 1, "gpu_name": "RTX 4090", "gpu_ram": 24576, "num_gpus": 1,
         "dph_base": 0.45, "dph_total": 0.50, "geolocation": "US",
         "rentable": true, "rented": false, "reliability2": 0.95, "verified": true},
        {"id": 12, "machine_id": 2, "gpu_name": "RTX 4090", "gpu_ram": 24576, "num_gpus": 1,
         "dph_base": 0.40, "dph_total": 0.44, "geolocation": "DE",
         "rentable": true, "rented": false, "reliability2": 0.9, "verified": false},
        // Already rented: supply for someone else, not for us.
        {"id": 13, "machine_id": 3, "gpu_name": "RTX 4090", "gpu_ram": 24576, "num_gpus": 1,
         "dph_base": 0.30, "dph_total": 0.35, "geolocation": "US",
         "rentable": true, "rented": true, "reliability2": 0.9, "verified": true},
        // Flagged unrentable: never supply, whatever else it says.
        {"id": 14, "machine_id": 4, "gpu_name": "RTX 4090", "gpu_ram": 24576, "num_gpus": 1,
         "dph_base": 0.10, "dph_total": 0.12, "geolocation": "US",
         "rentable": false, "rented": false, "reliability2": 0.9, "verified": true}
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
            json!({"success": false, "msg": "invalid api key"}).to_string(),
        )
            .into_response();
    }
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    match path.as_str() {
        "/users/current" => axum::Json(json!({
            "success": true, "user": "operator", "credit": 12.5,
        }))
        .into_response(),
        "/gpu_names/unique/" => axum::Json(json!(["RTX 4090", "RTX 3090"])).into_response(),
        "/bundles/" => {
            // The `q` filter arrives URL-encoded (`RTX+4090` or `RTX%204090`); match loosely.
            let q = uri.query().unwrap_or("");
            if q.contains("RTX") && q.contains("4090") {
                axum::Json(offers()).into_response()
            } else {
                axum::Json(json!({"offers": []})).into_response()
            }
        }
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

fn pt(base: &str) -> CredentialPlaintext {
    CredentialPlaintext {
        v: 1,
        provider_id: "p-1".into(),
        kind: "vast".into(),
        endpoint: base.into(),
        account: None,
        fields: [("api_key".to_string(), KEY.to_string())].into(),
    }
}

fn zones() -> Vec<(String, Vec<String>)> {
    vec![
        ("eu-1".into(), vec!["RTX 4090".into(), "RTX 3090".into()]),
        ("us-1".into(), vec!["RTX 4090".into()]),
    ]
}

async fn checker_at(base: &str) -> Box<dyn ProviderChecker> {
    mm_fleet::vast::checker(&pt(base), zones(), Some(base))
}

fn paths(seen: &[Seen]) -> Vec<String> {
    seen.iter().map(|s| s.path.clone()).collect()
}

#[tokio::test]
async fn a_healthy_check_reports_supply_shortage_and_balance() {
    let (base, state) = fake().await;
    let report = checker_at(&base).await.check().await;
    assert_eq!(report.state, CheckState::Ok);
    // The key goes on every call as a Bearer [REDACTED]
    let seen = state.lock().unwrap().seen.clone();
    assert!(!seen.is_empty());
    let want = format!("Bearer {KEY}");
    for s in &seen {
        assert_eq!(s.auth.as_deref(), Some(want.as_str()), "{s:?}");
        assert_eq!(s.method, "GET");
    }
    assert!(paths(&seen).contains(&"/users/current".to_string()));
    assert!(paths(&seen).contains(&"/gpu_names/unique/".to_string()));
    // One search per configured size, filtered to rentable offers of that GPU.
    let searches: Vec<_> = seen.iter().filter(|s| s.path == "/bundles/").collect();
    assert_eq!(searches.len(), 2, "{seen:?}");
    let mut queries: Vec<_> = searches.iter().map(|s| s.query.as_str()).collect();
    queries.sort();
    assert!(queries[0].contains("3090"), "{queries:?}");
    assert!(queries[1].contains("4090"), "{queries:?}");
    for s in &searches {
        assert!(s.query.contains("rentable"), "{s:?}");
    }

    // RTX 4090 has rentable offers (the rented and unrentable ones do not count);
    // RTX 3090 exists but has none: a shortage, not a typo.
    assert_eq!(report.zones.len(), 2);
    assert_eq!(report.zones[0].stock.get("RTX 4090"), Some(&Stock::Available));
    assert_eq!(report.zones[0].stock.get("RTX 3090"), Some(&Stock::Shortage));
    assert_eq!(report.zones[1].zone, "us-1");
    // Cheapest rentable offer: 0.44 (the rented 0.35 and unrentable 0.12 do not count).
    assert_eq!(report.prices.get("RTX 4090"), Some(&0.44));
    assert!(!report.prices.contains_key("RTX 3090"));
    assert_eq!(report.balance_minor, Some(1250));
    assert!(report.last_error.is_none());
}

#[tokio::test]
async fn a_bad_key_needs_you_and_says_nothing_else() {
    let (base, state) = fake().await;
    state.lock().unwrap().overrides.insert(
        "/users/current".into(),
        (401, json!({"msg": "bad"}).to_string()),
    );
    let report = checker_at(&base).await.check().await;
    assert_eq!(report.state, CheckState::NeedsYou);
    assert_eq!(report.last_error.as_ref().map(|(k, _)| k.as_str()), Some("permanent"));
    for z in &report.zones {
        assert!(z.stock.values().all(|s| *s == Stock::Unknown), "{z:?}");
    }
}

#[tokio::test]
async fn a_size_vast_does_not_know_is_a_configuration_error() {
    let (base, _) = fake().await;
    let pt = pt(&base);
    let report = mm_fleet::vast::checker(
        &pt,
        vec![("eu-1".into(), vec!["RTX 9099".into()])],
        Some(&base),
    )
    .check()
    .await;
    assert_eq!(report.state, CheckState::NeedsYou);
    let (_, msg) = report.last_error.clone().expect("a config error");
    assert!(msg.contains("RTX 9099"), "{msg}");
    assert_eq!(
        report.zones[0].stock.get("RTX 9099"),
        Some(&Stock::Unknown),
        "a typo is never a shortage"
    );
}

#[tokio::test]
async fn a_failed_offer_search_is_unknown_not_a_shortage() {
    let (base, state) = fake().await;
    state.lock().unwrap().overrides.insert(
        "/bundles/".into(),
        (500, json!({"msg": "boom"}).to_string()),
    );
    let report = checker_at(&base).await.check().await;
    assert_eq!(report.state, CheckState::Unknown);
    for z in &report.zones {
        assert!(z.stock.values().all(|s| *s == Stock::Unknown), "{z:?}");
    }
    assert!(report.prices.is_empty());
}

#[tokio::test]
async fn an_unreadable_gpu_list_is_unknown_but_supply_is_still_read() {
    let (base, state) = fake().await;
    state.lock().unwrap().overrides.insert(
        "/gpu_names/unique/".into(),
        (200, json!({"unexpected": true}).to_string()),
    );
    let report = checker_at(&base).await.check().await;
    assert_eq!(report.state, CheckState::Unknown);
    // Without names nothing is a typo; the searches still run, so readable supply counts.
    assert_eq!(report.zones[0].stock.get("RTX 4090"), Some(&Stock::Available));
    let seen = state.lock().unwrap().seen.clone();
    assert!(paths(&seen).contains(&"/bundles/".to_string()));
}

#[tokio::test]
async fn no_zones_is_unknown_not_ok() {
    let (base, _) = fake().await;
    let pt = pt(&base);
    let report = mm_fleet::vast::checker(&pt, vec![], Some(&base))
        .check()
        .await;
    assert_eq!(report.state, CheckState::Unknown);
}

#[tokio::test]
async fn a_missing_key_needs_you_without_dialling() {
    let (base, state) = fake().await;
    let mut bad = pt(&base);
    bad.fields.remove("api_key");
    let report = mm_fleet::vast::checker(&bad, zones(), Some(&base))
        .check()
        .await;
    assert_eq!(report.state, CheckState::NeedsYou);
    assert!(state.lock().unwrap().seen.is_empty(), "no call without a key");
}

#[tokio::test]
async fn catalog_offers_live_gpu_names_and_no_zones() {
    let (base, _) = fake().await;
    let c = mm_fleet::vast::checker(&pt(&base), zones(), Some(&base))
        .catalog()
        .await
        .expect("catalog");
    // Vast.ai is a global marketplace: no zones of its own, live GPU names as sizes.
    assert!(c.zones.is_empty(), "{:?}", c.zones);
    assert_eq!(c.sizes, vec!["RTX 3090", "RTX 4090"]);
}

#[tokio::test]
async fn catalog_with_a_bad_key_is_an_error_not_an_empty_catalog() {
    let (base, _) = fake().await;
    let mut bad = pt(&base);
    bad.fields.insert("api_key".to_string(), "wrong".to_string());
    let err = mm_fleet::vast::checker(&bad, zones(), Some(&base))
        .catalog()
        .await
        .expect_err("a bad key must fail the catalog");
    assert!(err.to_string().contains("key rejected"), "{err}");
}
