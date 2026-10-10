//! The OVH Public Cloud checker against a stand-in OVH API. The stand-in recomputes the
//! documented request signature (python-ovh: `"$1$" + sha1_hex(AS+CK+METHOD+URL+BODY+TS)`,
//! joined with `+`) from the headers it receives and the URL it was reached at, and answers
//! 401 when it does not match, so a wrong signature fails these tests the way it would fail
//! against OVH. Shapes: `GET /auth/time` (a bare integer), `GET /cloud/project/{serviceName}`
//! and `GET /cloud/project/{serviceName}/flavor?region=` (an array of `cloud.flavor.Flavor`).

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
use sha1::{Digest, Sha1};
use tokio::net::TcpListener;

use mm_fleet::checks::{CheckState, ProviderChecker, Stock};
use mm_fleet::sealed::CredentialPlaintext;

const AK: &str = "OVH-APP-KEY-3d1f";
const AS: &str = "OVH-APP-SECRET-b7e2";
const CK: &str = "OVH-CONSUMER-KEY-91ac";
const PROJECT: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f9";
/// The stand-in's clock runs this many seconds ahead of the checker's.
const SKEW: i64 = 3600;

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path_and_query: String,
    /// Whether any `X-Ovh-*` header came with it.
    had_ovh_headers: bool,
    /// Whether the signature matched what the stand-in recomputed.
    signature_ok: bool,
    timestamp: Option<i64>,
}

#[derive(Default)]
struct Knobs {
    seen: Vec<Seen>,
    /// path -> (status, body) answered instead of the default, for a request that is
    /// correctly signed. `{ECHO}` in a body is replaced by the `X-Ovh-*` header values.
    overrides: HashMap<String, (u16, String)>,
}
type Shared = Arc<Mutex<Knobs>>;

const TIME: &str = "/auth/time";

fn project_path() -> String {
    format!("/cloud/project/{PROJECT}")
}
fn flavor_path() -> String {
    format!("/cloud/project/{PROJECT}/flavor")
}

fn flavors(region: &str) -> Option<Value> {
    match region {
        "GRA11" => Some(json!([
            {"id": "f-l4-90", "name": "l4-90", "region": "GRA11", "osType": "linux",
             "type": "ovh.gpu", "available": true, "quota": 3,
             "planCodes": {"hourly": "l4-90.consumption", "monthly": "l4-90.monthly.postpaid"}},
            {"id": "f-l4-90-win", "name": "l4-90", "region": "GRA11", "osType": "windows",
             "type": "ovh.gpu", "available": false, "quota": 0,
             "planCodes": {"hourly": "l4-90.consumption.windows"}},
            {"id": "f-l4-180", "name": "l4-180", "region": "GRA11", "osType": "linux",
             "type": "ovh.gpu", "available": false, "quota": 3, "planCodes": {}},
            {"id": "f-win-only", "name": "win-only", "region": "GRA11", "osType": "windows",
             "type": "ovh.cpu", "available": true, "quota": 1, "planCodes": {}}
        ])),
        "SBG5" => Some(json!([
            {"id": "f2-l4-90", "name": "l4-90", "region": "SBG5", "osType": "linux",
             "type": "ovh.gpu", "available": false, "quota": 0, "planCodes": {}}
        ])),
        "BHS5" => Some(json!([])),
        _ => None,
    }
}

fn sign(full_url: &str, method: &str, ts: i64) -> String {
    let joined = format!("{AS}+{CK}+{method}+{full_url}++{ts}");
    format!("$1${}", hex::encode(Sha1::digest(joined.as_bytes())))
}

async fn handle(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> axum::response::Response {
    let h = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let path = uri.path().to_string();
    let path_and_query = uri.to_string();
    let host = h("host").unwrap_or_default();
    let full_url = format!("http://{host}{path_and_query}");
    let ovh_headers: Vec<String> = [
        "x-ovh-application",
        "x-ovh-consumer",
        "x-ovh-timestamp",
        "x-ovh-signature",
    ]
    .iter()
    .filter_map(|n| h(n))
    .collect();
    let timestamp = h("x-ovh-timestamp").and_then(|t| t.parse::<i64>().ok());
    let signature_ok = h("x-ovh-application").as_deref() == Some(AK)
        && h("x-ovh-consumer").as_deref() == Some(CK)
        && timestamp.is_some_and(|ts| {
            h("x-ovh-signature").as_deref() == Some(sign(&full_url, method.as_str(), ts).as_str())
        });
    let over = {
        let mut k = s.lock().unwrap();
        k.seen.push(Seen {
            method: method.to_string(),
            path_and_query: path_and_query.clone(),
            had_ovh_headers: !ovh_headers.is_empty(),
            signature_ok,
            timestamp,
        });
        k.overrides.get(&path).cloned()
    };
    if path == TIME {
        if let Some((status, body)) = over {
            return (StatusCode::from_u16(status).unwrap(), body).into_response();
        }
        let now = chrono::Utc::now().timestamp() + SKEW;
        return now.to_string().into_response();
    }
    if !signature_ok {
        return (
            StatusCode::UNAUTHORIZED,
            json!({"class": "Client::Unauthorized", "message": "Invalid signature"}).to_string(),
        )
            .into_response();
    }
    if let Some((status, body)) = over {
        let body = body.replace("{ECHO}", &ovh_headers.join(" "));
        return (StatusCode::from_u16(status).unwrap(), body).into_response();
    }
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            json!({"class": "Client::NotFound", "message": "Not found"}).to_string(),
        )
            .into_response()
    };
    if path == project_path() {
        return axum::Json(
            json!({"project_id": PROJECT, "projectName": "gpu", "status": "ok", "planCode": "project.2018"}),
        )
        .into_response();
    }
    if path == flavor_path() {
        let region = uri
            .query()
            .and_then(|q| q.strip_prefix("region="))
            .unwrap_or("");
        return match flavors(region) {
            Some(v) => axum::Json(v).into_response(),
            None => (
                StatusCode::BAD_REQUEST,
                json!({"message": format!("Region {region} not found")}).to_string(),
            )
                .into_response(),
        };
    }
    not_found()
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

fn pt_with(fields: &[(&str, &str)], account: Option<&str>) -> CredentialPlaintext {
    CredentialPlaintext {
        v: 1,
        provider_id: "p-1".into(),
        kind: "ovh".into(),
        endpoint: "https://eu.api.ovh.com/1.0".into(),
        account: account.map(str::to_string),
        fields: fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn pt() -> CredentialPlaintext {
    pt_with(
        &[
            ("application_key", AK),
            ("application_secret", AS),
            ("consumer_key", CK),
        ],
        Some(PROJECT),
    )
}

fn zones(spec: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    spec.iter()
        .map(|(z, sizes)| (z.to_string(), sizes.iter().map(|s| s.to_string()).collect()))
        .collect()
}

fn checker(base: &str, spec: &[(&str, &[&str])]) -> Box<dyn ProviderChecker> {
    mm_fleet::ovh::checker(&pt(), zones(spec), Some(base))
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

fn assert_no_secret(text: &str) {
    for s in [AK, AS, CK] {
        assert!(!text.contains(s), "{text}");
    }
    assert!(!text.contains("$1$"), "{text}");
}

#[tokio::test]
async fn ok_path_maps_flavor_availability() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.last_error, None);
    assert_eq!(r.key_scope, None);
    assert_eq!(r.balance_minor, None);
    assert!(r.prices.is_empty(), "OVH flavors carry no price here");
    assert_eq!(r.zones.len(), 1);
    assert_eq!(
        r.zones[0].zone, "gra11",
        "the zone keeps the server's spelling"
    );
    assert_eq!(r.zones[0].instances_running, None);
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Available);
    let row = r.to_status_row("p-1", 1, chrono::Utc::now());
    assert_eq!(row.state, "ok");
    assert_eq!(row.stock["gra11"]["l4-90"], "available");
}

#[tokio::test]
async fn the_stand_in_recomputes_the_signature_of_every_signed_call() {
    let (base, knobs) = fake().await;
    let r = checker(&base, &[("gra11", &["l4-90"]), ("sbg5", &["l4-90"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let seen = knobs.lock().unwrap().seen.clone();
    let paths: Vec<&str> = seen.iter().map(|s| s.path_and_query.as_str()).collect();
    assert_eq!(
        paths,
        [
            TIME.to_string(),
            project_path(),
            format!("{}?region=GRA11", flavor_path()),
            format!("{}?region=SBG5", flavor_path()),
        ]
    );
    for s in &seen {
        assert_eq!(s.method, "GET", "a check never writes: {s:?}");
        if s.path_and_query == TIME {
            assert!(!s.had_ovh_headers, "the clock read is unsigned: {s:?}");
        } else {
            assert!(s.signature_ok, "signature must match: {s:?}");
        }
    }
}

#[tokio::test]
async fn the_timestamp_follows_the_providers_clock_not_ours() {
    let (base, knobs) = fake().await;
    let _ = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    let local = chrono::Utc::now().timestamp();
    let seen = knobs.lock().unwrap().seen.clone();
    let signed: Vec<i64> = seen.iter().filter_map(|s| s.timestamp).collect();
    assert_eq!(signed.len(), 2);
    for ts in signed {
        assert!(
            (ts - (local + SKEW)).abs() <= 5,
            "ts {ts} should be the stand-in's clock (+{SKEW}s), local is {local}"
        );
    }
}

#[tokio::test]
async fn a_wrong_application_secret_fails_the_signature_and_is_needs_you() {
    let (base, knobs) = fake().await;
    let wrong = pt_with(
        &[
            ("application_key", AK),
            ("application_secret", "not-the-secret"),
            ("consumer_key", CK),
        ],
        Some(PROJECT),
    );
    let c = mm_fleet::ovh::checker(&wrong, zones(&[("gra11", &["l4-90"])]), Some(&base));
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    assert_eq!(r.last_error.as_ref().unwrap().0, "permanent");
    let seen = knobs.lock().unwrap().seen.clone();
    assert!(
        seen.iter()
            .any(|s| !s.signature_ok && s.path_and_query != TIME)
    );
}

#[tokio::test]
async fn availability_follows_the_flavor_name_case_insensitively_and_the_linux_entry() {
    let (base, _) = fake().await;
    let r = checker(
        &base,
        &[("gra11", &["L4-90", "l4-180", "t2-45", "win-only"])],
    )
    .check()
    .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let s = &r.zones[0].stock;
    // The windows entry of l4-90 is unavailable; the linux one is available.
    assert_eq!(s["L4-90"], Stock::Available);
    // `available: false` is a stock-out.
    assert_eq!(s["l4-180"], Stock::Shortage);
    // Not in the region's list: OVH did not say, so neither do we.
    assert_eq!(s["t2-45"], Stock::Unknown);
    // Only a windows entry exists: its flag is the only signal there is.
    assert_eq!(s["win-only"], Stock::Available);
}

#[tokio::test]
async fn each_zone_is_read_in_its_own_region_in_failover_order() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("sbg5", &["l4-90"]), ("gra11", &["l4-90"])])
        .check()
        .await;
    assert_eq!(r.zones[0].zone, "sbg5");
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Shortage);
    assert_eq!(r.zones[1].zone, "gra11");
    assert_eq!(r.zones[1].stock["l4-90"], Stock::Available);
}

#[tokio::test]
async fn a_region_with_no_flavors_leaves_the_sizes_unknown() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("bhs5", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
}

#[tokio::test]
async fn a_rejected_key_is_needs_you_with_the_status_line_and_no_secret() {
    for status in [401u16, 403] {
        let (base, knobs) = fake().await;
        set(
            &knobs,
            &project_path(),
            status,
            "{ECHO} {\"message\":\"This credential is not valid\"}",
        );
        let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{status}");
        let (kind, msg) = r.last_error.clone().unwrap();
        assert_eq!(kind, "permanent");
        assert!(msg.contains(&status.to_string()), "{msg}");
        assert!(msg.contains("key rejected"), "{msg}");
        assert!(
            !msg.contains("credential is not valid"),
            "body discarded: {msg}"
        );
        assert_no_secret(&msg);
        assert_eq!(calls(&knobs), 2, "the clock and the project, nothing after");
        assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
    }
}

#[tokio::test]
async fn a_key_refused_on_the_flavor_list_is_still_needs_you() {
    let (base, knobs) = fake().await;
    set(
        &knobs,
        &flavor_path(),
        403,
        "{\"message\":\"This call has not been granted\"}",
    );
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(msg.contains("403") && msg.contains("key rejected"), "{msg}");
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
}

#[tokio::test]
async fn a_provider_error_is_unknown_and_transient_on_each_call() {
    for (path, status) in [
        (TIME.to_string(), 500u16),
        (project_path(), 503),
        (project_path(), 429),
        (flavor_path(), 502),
        (flavor_path(), 408),
    ] {
        let (base, knobs) = fake().await;
        set(&knobs, &path, status, "busy");
        let r = checker(&base, &[("gra11", &["l4-90", "l4-180"])])
            .check()
            .await;
        assert_eq!(r.state, CheckState::Unknown, "{path} {status}");
        assert_eq!(
            r.last_error.as_ref().unwrap().0,
            "transient",
            "{path} {status}"
        );
        assert_eq!(
            r.zones[0].stock.len(),
            2,
            "every configured size is reported"
        );
        assert!(r.zones[0].stock.values().all(|s| *s == Stock::Unknown));
    }
}

#[tokio::test]
async fn a_clock_that_is_not_an_integer_is_unknown_and_nothing_is_signed() {
    let (base, knobs) = fake().await;
    set(&knobs, TIME, 200, "<html>maintenance</html>");
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(calls(&knobs), 1);
}

#[tokio::test]
async fn an_unknown_project_is_needs_you_with_a_plain_message() {
    let (base, knobs) = fake().await;
    set(&knobs, &project_path(), 404, "{\"message\":\"nope\"}");
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        format!("permanent provider failure: project {PROJECT} does not exist")
    );
}

#[tokio::test]
async fn an_unknown_region_is_needs_you_and_the_other_zones_are_still_read() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("xx9", &["l4-90"]), ("gra11", &["l4-90"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(msg.contains("Region XX9 not found"), "{msg}");
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
    assert_eq!(r.zones[1].stock["l4-90"], Stock::Available);
}

#[tokio::test]
async fn a_needs_you_error_outranks_an_outage_whichever_zone_comes_first() {
    for order in [["xx9", "gra11"], ["gra11", "xx9"]] {
        let (base, _) = fake().await;
        let r = checker(&base, &[(order[0], &["l4-90"]), (order[1], &["l4-90"])])
            .check()
            .await;
        assert_eq!(r.state, CheckState::NeedsYou, "{order:?}");
    }
    let (base, knobs) = fake().await;
    set(&knobs, &flavor_path(), 500, "down");
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
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
async fn an_incomplete_credential_is_needs_you_and_makes_no_call() {
    let cases = [
        (
            pt_with(
                &[("application_key", AK), ("application_secret", AS)],
                Some(PROJECT),
            ),
            "consumer_key",
        ),
        (
            pt_with(
                &[("application_secret", AS), ("consumer_key", CK)],
                Some(PROJECT),
            ),
            "application_key",
        ),
        (
            pt_with(
                &[("application_key", AK), ("consumer_key", CK)],
                Some(PROJECT),
            ),
            "application_secret",
        ),
        (
            pt_with(
                &[
                    ("application_key", AK),
                    ("application_secret", AS),
                    ("consumer_key", CK),
                ],
                None,
            ),
            "project",
        ),
    ];
    for (cred, missing) in cases {
        let (base, knobs) = fake().await;
        let c = mm_fleet::ovh::checker(&cred, zones(&[("gra11", &["l4-90"])]), Some(&base));
        let r = c.check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{missing}");
        let (kind, msg) = r.last_error.clone().unwrap();
        assert_eq!(kind, "permanent");
        assert!(msg.contains(missing), "{missing}: {msg}");
        assert_no_secret(&msg);
        assert_eq!(calls(&knobs), 0, "{missing}");
    }
}

/// A provider that echoes the request it refused: every credential value, and the
/// signature, must be gone from the stored text.
#[tokio::test]
async fn a_provider_body_that_echoes_the_credentials_and_signature_is_redacted() {
    for (path, status) in [
        (project_path(), 500u16),
        (project_path(), 400),
        (flavor_path(), 500),
    ] {
        let (base, knobs) = fake().await;
        set(&knobs, &path, status, "bad request: {ECHO} end");
        let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
        let (_, msg) = r.last_error.clone().unwrap();
        assert_no_secret(&msg);
        assert!(msg.contains("[redacted]"), "{path} {status}: {msg}");
        assert_no_secret(&format!(
            "{:?}",
            r.to_status_row("p", 1, chrono::Utc::now())
        ));
    }
}

#[tokio::test]
async fn a_body_that_is_not_the_documented_shape_is_unknown_not_a_panic() {
    let (base, knobs) = fake().await;
    set(&knobs, &flavor_path(), 200, "<html>maintenance</html>");
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
}

/// The project id and the region come from the operator and are signed as sent: a name with
/// path or query syntax must stay one path segment or one query value.
#[tokio::test]
async fn names_cannot_rewrite_the_signed_url() {
    // A project id with path syntax stays one (encoded) segment.
    let (base, knobs) = fake().await;
    let cred = pt_with(
        &[
            ("application_key", AK),
            ("application_secret", AS),
            ("consumer_key", CK),
        ],
        Some("../../me"),
    );
    let c = mm_fleet::ovh::checker(&cred, zones(&[("gra11", &["l4-90"])]), Some(&base));
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let seen = knobs.lock().unwrap().seen.clone();
    assert!(
        seen.iter()
            .any(|s| s.path_and_query == "/cloud/project/..%2F..%2Fme" && s.signature_ok),
        "{seen:?}"
    );
    assert!(seen.iter().all(|s| !s.path_and_query.starts_with("/me")));

    // A region with query syntax stays one (encoded) query value, and is signed as sent.
    let (base, knobs) = fake().await;
    let r = checker(&base, &[("gra11&x=1", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let seen = knobs.lock().unwrap().seen.clone();
    let flavor_call = seen.last().unwrap();
    assert_eq!(
        flavor_call.path_and_query,
        format!("{}?region=GRA11%26X%3D1", flavor_path())
    );
    assert!(flavor_call.signature_ok);
}
