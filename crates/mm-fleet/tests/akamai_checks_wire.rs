//! The Akamai (Linode) checker against a stand-in Linode API v4. Shapes follow the
//! published docs: `GET /linode/instances` (paginated), `GET /regions/{region}/availability`
//! (a top-level array of `{region, plan, available}`) and `GET /linode/types/{type}`
//! (`price.hourly` and `region_prices[{id, hourly}]`).

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

const TOKEN: &str = "LIN-TEST-TOKEN-77b2e0";

const SMALL: &str = "g2-gpu-rtx4000a1-s";
const MEDIUM: &str = "g2-gpu-rtx4000a1-m";
const LARGE: &str = "g2-gpu-rtx4000a1-l";

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
    /// `X-OAuth-Scopes` sent with an override, as Linode sends it on a 401.
    oauth_scopes: Option<String>,
}
type Shared = Arc<Mutex<Knobs>>;

fn availability_of(region: &str) -> Option<Value> {
    match region {
        "us-iad" => Some(json!([
            {"region": "us-iad", "plan": SMALL, "available": true},
            {"region": "us-iad", "plan": MEDIUM, "available": false},
            {"region": "us-iad", "plan": "g6-standard-2", "available": true}
        ])),
        "de-fra-2" => Some(json!([
            {"region": "de-fra-2", "plan": SMALL, "available": false}
        ])),
        _ => None,
    }
}

fn type_of(size: &str) -> Option<Value> {
    match size {
        SMALL => Some(json!({
            "id": SMALL, "label": "RTX4000 Ada x1 Small", "class": "gpu", "gpus": 1,
            "price": {"hourly": 0.52, "monthly": 350.0},
            "region_prices": [
                {"id": "de-fra-2", "hourly": 0.62, "monthly": 420.0},
                {"id": "id-cgk", "hourly": 0.7, "monthly": 470.0}
            ]
        })),
        MEDIUM => Some(json!({
            "id": MEDIUM, "label": "RTX4000 Ada x1 Medium", "class": "gpu", "gpus": 1,
            "price": {"hourly": 1.04, "monthly": 700.0},
            "region_prices": []
        })),
        _ => None,
    }
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
        k.overrides
            .get(&path)
            .cloned()
            .map(|o| (o, k.oauth_scopes.clone()))
    };
    if let Some(((status, body), scopes)) = over {
        let mut resp = (StatusCode::from_u16(status).unwrap(), body).into_response();
        if let Some(s) = scopes {
            resp.headers_mut()
                .insert("x-oauth-scopes", s.parse().unwrap());
        }
        return resp;
    }
    if auth.as_deref() != Some(&format!("Bearer {TOKEN}")) {
        return (
            StatusCode::UNAUTHORIZED,
            json!({"errors": [{"reason": format!("Invalid Token {}", auth.unwrap_or_default())}]})
                .to_string(),
        )
            .into_response();
    }
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            json!({"errors": [{"reason": "Not found"}]}).to_string(),
        )
            .into_response()
    };
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segs.as_slice() {
        ["linode", "instances"] => {
            axum::Json(json!({"data": [], "page": 1, "pages": 1, "results": 0})).into_response()
        }
        ["regions", region, "availability"] => match availability_of(region) {
            Some(v) => axum::Json(v).into_response(),
            None => not_found(),
        },
        ["linode", "types", size] => match type_of(size) {
            Some(v) => axum::Json(v).into_response(),
            None => not_found(),
        },
        _ => not_found(),
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
        kind: "akamai".into(),
        endpoint: "https://api.linode.com/v4".into(),
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
    mm_fleet::akamai::checker(&pt(Some(TOKEN)), zones(spec), Some(base))
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
async fn ok_path_maps_availability_and_hourly_prices() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("us-iad", &[SMALL])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.last_error, None);
    assert_eq!(r.key_scope, None);
    assert_eq!(r.balance_minor, None);
    assert_eq!(r.zones.len(), 1);
    assert_eq!(r.zones[0].zone, "us-iad");
    assert_eq!(r.zones[0].instances_running, None);
    assert_eq!(r.zones[0].stock[SMALL], Stock::Available);
    assert_eq!(r.prices[SMALL], 0.52);
    let row = r.to_status_row("p-1", 1, chrono::Utc::now());
    assert_eq!(row.state, "ok");
    assert_eq!(row.stock["us-iad"][SMALL], "available");
}

#[tokio::test]
async fn listed_true_is_available_listed_false_is_shortage_absent_is_unknown() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("us-iad", &[SMALL, MEDIUM, LARGE])])
        .check()
        .await;
    let s = &r.zones[0].stock;
    assert_eq!(s[SMALL], Stock::Available);
    assert_eq!(s[MEDIUM], Stock::Shortage);
    // LARGE is not in the region's list: Linode did not say, so neither do we.
    assert_eq!(s[LARGE], Stock::Unknown);
    // LARGE does not exist as a plan at all: that is a configuration error.
    assert_eq!(r.state, CheckState::NeedsYou);
}

#[tokio::test]
async fn a_regional_price_overrides_the_base_price_for_the_first_zone_that_has_the_size() {
    let (base, _) = fake().await;
    // de-fra-2 first: its regional hourly price wins.
    let r = checker(&base, &[("de-fra-2", &[SMALL]), ("us-iad", &[SMALL])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.prices[SMALL], 0.62);
    assert_eq!(r.zones[0].stock[SMALL], Stock::Shortage);
    assert_eq!(r.zones[1].stock[SMALL], Stock::Available);
    // A region with no override pays the base price.
    let r = checker(&base, &[("us-iad", &[SMALL])]).check().await;
    assert_eq!(r.prices[SMALL], 0.52);
    // A plan with no region_prices at all.
    let r = checker(&base, &[("us-iad", &[MEDIUM])]).check().await;
    assert_eq!(r.prices[MEDIUM], 1.04);
}

#[tokio::test]
async fn requests_are_reads_to_the_documented_paths_with_the_bearer_token_on_every_call() {
    let (base, knobs) = fake().await;
    let _ = checker(
        &base,
        &[("us-iad", &[SMALL, MEDIUM]), ("de-fra-2", &[SMALL])],
    )
    .check()
    .await;
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
        [
            "/linode/instances",
            "/regions/us-iad/availability",
            "/regions/de-fra-2/availability",
            "/linode/types/g2-gpu-rtx4000a1-s",
            "/linode/types/g2-gpu-rtx4000a1-m",
        ],
        "each plan's price is read once, however many zones list it"
    );
    // `page_size` is documented as 25..=500, so the smallest legal page is asked for.
    assert_eq!(seen[0].query, "page_size=25");
}

/// An endpoint saved with a trailing slash (`https://api.linode.com/v4/`) reads the same
/// paths: a doubled `/` would be refused as if the token were bad.
#[tokio::test]
async fn an_endpoint_with_a_trailing_slash_builds_the_same_paths() {
    for slashes in ["/", "//"] {
        let (base, knobs) = fake().await;
        let r = checker(&format!("{base}{slashes}"), &[("us-iad", &[SMALL])])
            .check()
            .await;
        assert_eq!(r.state, CheckState::Ok, "{slashes}: {:?}", r.last_error);
        assert_eq!(r.zones[0].stock[SMALL], Stock::Available);
        assert_eq!(r.prices[SMALL], 0.52);
        let paths: Vec<String> = knobs
            .lock()
            .unwrap()
            .seen
            .iter()
            .map(|s| s.path.clone())
            .collect();
        assert_eq!(
            paths,
            [
                "/linode/instances",
                "/regions/us-iad/availability",
                "/linode/types/g2-gpu-rtx4000a1-s",
            ],
            "{slashes}"
        );
    }
}

#[tokio::test]
async fn a_rejected_token_is_needs_you_with_the_status_line_and_no_secret() {
    let (base, knobs) = fake().await;
    set(
        &knobs,
        "/linode/instances",
        401,
        &format!("{{\"errors\":[{{\"reason\":\"token {TOKEN} invalid\"}}]}}"),
    );
    let r = checker(&base, &[("us-iad", &[SMALL])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: 401 Unauthorized: key rejected"
    );
    assert!(!msg.contains(TOKEN), "{msg}");
    assert!(!msg.contains("invalid"), "the body is discarded: {msg}");
    assert_eq!(calls(&knobs), 1, "nothing else is asked of a refused token");
    assert_eq!(r.zones[0].stock[SMALL], Stock::Unknown);
}

/// Linode answers 403 for a valid token without the scope a call needs: "key rejected" would
/// send the operator to replace a token that only needs another scope.
#[tokio::test]
async fn a_token_without_the_scope_is_needs_you_and_names_the_scope() {
    let (base, knobs) = fake().await;
    set(
        &knobs,
        "/linode/instances",
        403,
        &format!("{{\"errors\":[{{\"reason\":\"token {TOKEN} unauthorized for linodes\"}}]}}"),
    );
    let r = checker(&base, &[("us-iad", &[SMALL])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: 403 Forbidden: the token lacks a scope — give it Linodes: Read Only (Read/Write to rent)"
    );
    assert!(!msg.contains(TOKEN), "{msg}");
    assert!(
        !msg.contains("unauthorized for linodes"),
        "the body is discarded: {msg}"
    );
    assert_eq!(calls(&knobs), 1, "nothing else is asked of a refused token");
    assert_eq!(r.zones[0].stock[SMALL], Stock::Unknown);
}

/// Linode answers a real token without the Linodes scope with a 401 too, and tells it apart by
/// naming the token's scopes in `X-OAuth-Scopes` (an unknown token gets `unknown`). The header
/// value is never shown.
#[tokio::test]
async fn a_401_for_a_token_with_other_scopes_names_the_scope() {
    for (scopes, want) in [
        (
            Some("account:read_only events:read_only"),
            "permanent provider failure: 401 Unauthorized: the token lacks a scope — give it Linodes: Read Only (Read/Write to rent)",
        ),
        (
            Some("unknown"),
            "permanent provider failure: 401 Unauthorized: key rejected",
        ),
        (
            Some("  "),
            "permanent provider failure: 401 Unauthorized: key rejected",
        ),
        (
            None,
            "permanent provider failure: 401 Unauthorized: key rejected",
        ),
    ] {
        let (base, knobs) = fake().await;
        knobs.lock().unwrap().oauth_scopes = scopes.map(str::to_string);
        set(
            &knobs,
            "/linode/instances",
            401,
            "{\"errors\":[{\"reason\":\"Invalid Token\"}]}",
        );
        let r = checker(&base, &[("us-iad", &[SMALL])]).check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{scopes:?}");
        let (_, msg) = r.last_error.clone().unwrap();
        assert_eq!(msg, want, "{scopes:?}");
        assert!(!msg.contains("account:read_only"), "{msg}");
    }
}

#[tokio::test]
async fn a_token_refused_after_verify_is_still_needs_you() {
    let (base, knobs) = fake().await;
    set(
        &knobs,
        "/regions/us-iad/availability",
        403,
        "{\"errors\":[{\"reason\":\"BODY-MARKER\"}]}",
    );
    let r = checker(&base, &[("us-iad", &[SMALL])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    // The same wording on any call, and the body is discarded.
    assert!(
        msg.contains("403 Forbidden: the token lacks a scope — give it Linodes: Read Only"),
        "{msg}"
    );
    assert!(!msg.contains("BODY-MARKER"), "the body is discarded: {msg}");
    assert_eq!(r.zones[0].stock[SMALL], Stock::Unknown);
    assert_eq!(r.prices[SMALL], 0.52, "the price read still ran");
}

#[tokio::test]
async fn a_provider_error_on_verify_is_unknown_and_transient() {
    for status in [500u16, 503, 429, 408] {
        let (base, knobs) = fake().await;
        set(&knobs, "/linode/instances", status, "busy");
        let r = checker(&base, &[("us-iad", &[SMALL, MEDIUM])])
            .check()
            .await;
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
async fn a_failed_availability_read_marks_every_size_unknown_and_keeps_prices() {
    let (base, knobs) = fake().await;
    set(&knobs, "/regions/us-iad/availability", 502, "bad gateway");
    let r = checker(&base, &[("us-iad", &[SMALL, MEDIUM])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert!(r.zones[0].stock.values().all(|s| *s == Stock::Unknown));
    assert_eq!(r.zones[0].stock.len(), 2);
    assert_eq!(r.prices[SMALL], 0.52);
    assert_eq!(r.prices[MEDIUM], 1.04);
}

#[tokio::test]
async fn a_failed_price_read_keeps_stock_and_leaves_that_price_out() {
    let (base, knobs) = fake().await;
    set(&knobs, "/linode/types/g2-gpu-rtx4000a1-s", 500, "down");
    let r = checker(&base, &[("us-iad", &[SMALL, MEDIUM])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock[SMALL], Stock::Available);
    assert_eq!(r.zones[0].stock[MEDIUM], Stock::Shortage);
    assert!(!r.prices.contains_key(SMALL));
    assert_eq!(r.prices[MEDIUM], 1.04);
}

#[tokio::test]
async fn an_unknown_plan_is_needs_you_with_a_plain_message() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("us-iad", &["g9-nope"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: plan g9-nope does not exist"
    );
    assert!(r.prices.is_empty());
}

#[tokio::test]
async fn an_unknown_region_is_needs_you_and_the_other_zones_are_still_read() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("xx-nope", &[SMALL]), ("us-iad", &[SMALL])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: region xx-nope does not exist"
    );
    assert_eq!(r.zones.len(), 2);
    assert_eq!(r.zones[0].stock[SMALL], Stock::Unknown);
    assert_eq!(r.zones[1].stock[SMALL], Stock::Available);
}

#[tokio::test]
async fn a_needs_you_error_outranks_an_outage_whichever_zone_comes_first() {
    for order in [["xx-nope", "us-iad"], ["us-iad", "xx-nope"]] {
        let (base, knobs) = fake().await;
        set(&knobs, "/regions/us-iad/availability", 500, "down");
        let r = checker(&base, &[(order[0], &[SMALL]), (order[1], &[SMALL])])
            .check()
            .await;
        assert_eq!(r.state, CheckState::NeedsYou, "{order:?}");
        assert_eq!(r.last_error.as_ref().unwrap().0, "permanent", "{order:?}");
    }
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
async fn a_credential_without_a_token_is_needs_you_and_makes_no_call() {
    let (base, knobs) = fake().await;
    let c = mm_fleet::akamai::checker(&pt(None), zones(&[("us-iad", &[SMALL])]), Some(&base));
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(msg.contains("token"), "{msg}");
    assert_eq!(calls(&knobs), 0);
}

#[tokio::test]
async fn a_token_a_header_cannot_carry_is_needs_you_and_makes_no_call() {
    let (base, knobs) = fake().await;
    let c = mm_fleet::akamai::checker(
        &pt(Some("LIN-BAD\nTOKEN")),
        zones(&[("us-iad", &[SMALL])]),
        Some(&base),
    );
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(
        msg.contains("characters an HTTP header cannot carry") && msg.contains("re-enter it"),
        "{msg}"
    );
    assert!(!msg.contains("LIN-BAD"), "{msg}");
    assert_eq!(calls(&knobs), 0, "the request never left");
}

#[tokio::test]
async fn a_provider_body_that_echoes_the_token_is_redacted() {
    for status in [500u16, 400] {
        let (base, knobs) = fake().await;
        set(
            &knobs,
            "/linode/instances",
            status,
            &format!("oops Authorization: Bearer {TOKEN} was bad"),
        );
        let r = checker(&base, &[("us-iad", &[SMALL])]).check().await;
        let (_, msg) = r.last_error.clone().unwrap();
        assert!(!msg.contains(TOKEN), "{status}: {msg}");
        assert!(msg.contains("[redacted]"), "{status}: {msg}");
    }
}

#[tokio::test]
async fn a_body_that_is_not_the_documented_shape_is_unknown_not_a_panic() {
    let (base, knobs) = fake().await;
    set(
        &knobs,
        "/regions/us-iad/availability",
        200,
        "<html>maintenance</html>",
    );
    let r = checker(&base, &[("us-iad", &[SMALL])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock[SMALL], Stock::Unknown);
}

/// A zone or plan name from the operator must not be able to change the path it lands in.
#[tokio::test]
async fn a_plan_name_cannot_rewrite_the_request_path() {
    let (base, knobs) = fake().await;
    // The URL library strips an embedded tab, LF or CR from a segment *before* it applies the
    // dot rules, so `.\t.` would climb one level unless control characters are refused.
    let r = checker(
        &base,
        &[(
            "us-iad",
            &[
                "../../profile",
                "..",
                "a?b#c",
                ".\t.",
                ".\n.",
                "\t..",
                ".\r",
            ],
        )],
    )
    .check()
    .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let seen = knobs.lock().unwrap().seen.clone();
    for s in &seen {
        assert_ne!(s.path, "/profile", "{seen:?}");
        assert_ne!(s.path, "/linode", "{seen:?}");
        assert_ne!(s.path, "/linode/", "{seen:?}");
        assert_ne!(s.path, "/linode/types", "{seen:?}");
        assert_ne!(s.path, "/linode/types/", "{seen:?}");
    }
    // Only the verify call, the one availability read and a price read per *valid* name.
    let paths: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/linode/instances",
            "/regions/us-iad/availability",
            "/linode/types/..%2F..%2Fprofile",
            "/linode/types/a%3Fb%23c",
        ],
        "a name with a dot-only or control-character segment is refused before any request"
    );
}
