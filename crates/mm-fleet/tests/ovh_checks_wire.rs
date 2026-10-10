//! The OVH Public Cloud checker against a stand-in OVH API. The stand-in recomputes the
//! documented request signature (python-ovh: `"$1$" + sha1_hex(AS+CK+METHOD+URL+BODY+TS)`,
//! joined with `+`) from the headers it receives and the URL it was reached at, and answers
//! 401 when it does not match, so a wrong signature fails these tests the way it would fail
//! against OVH. Shapes: `GET /auth/time` (a bare integer), `GET /cloud/project/{serviceName}`,
//! `GET /cloud/project/{serviceName}/region` (an array of region names) and
//! `GET /cloud/project/{serviceName}/flavor?region=` (an array of `cloud.flavor.Flavor`).
//! A request path may carry the API's `/1.0` prefix, which the stand-in routes past; the
//! signature is still checked against the URL exactly as it was received.

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
    /// region (upper case) -> (status, body) answered for a correctly signed flavor list of
    /// that region instead of the default.
    region_overrides: HashMap<String, (u16, String)>,
    /// How long `/auth/time` waits before it reads its clock and answers (a slow request).
    time_delay_ms: u64,
}
type Shared = Arc<Mutex<Knobs>>;

const TIME: &str = "/auth/time";

fn project_path() -> String {
    format!("/cloud/project/{PROJECT}")
}
fn flavor_path() -> String {
    format!("/cloud/project/{PROJECT}/flavor")
}
fn region_path() -> String {
    format!("/cloud/project/{PROJECT}/region")
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
             "type": "ovh.gpu", "available": false, "quota": 1, "planCodes": {}}
        ])),
        // In stock, but the project may not launch one: a new project's default quota is
        // below the L4. The windows entry has room, which must not count.
        "DE1" => Some(json!([
            {"id": "f3-l4-90", "name": "l4-90", "region": "DE1", "osType": "linux",
             "type": "ovh.gpu", "available": true, "quota": 0, "planCodes": {}},
            {"id": "f3-l4-90-win", "name": "l4-90", "region": "DE1", "osType": "windows",
             "type": "ovh.gpu", "available": true, "quota": 5, "planCodes": {}},
            {"id": "f3-l4-180", "name": "l4-180", "region": "DE1", "osType": "linux",
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
    // The API lives under `/1.0`; routing ignores it, the signature check does not.
    let path = uri
        .path()
        .strip_prefix("/1.0")
        .unwrap_or(uri.path())
        .to_string();
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
    let (over, time_delay_ms) = {
        let mut k = s.lock().unwrap();
        k.seen.push(Seen {
            method: method.to_string(),
            path_and_query: path_and_query.clone(),
            had_ovh_headers: !ovh_headers.is_empty(),
            signature_ok,
            timestamp,
        });
        (k.overrides.get(&path).cloned(), k.time_delay_ms)
    };
    if path == TIME {
        if let Some((status, body)) = over {
            return (StatusCode::from_u16(status).unwrap(), body).into_response();
        }
        // A slow request: the provider reads its clock just before it answers.
        if time_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(time_delay_ms)).await;
        }
        let now = chrono::Utc::now().timestamp() + SKEW;
        return now.to_string().into_response();
    }
    if !signature_ok {
        // OVH's answer to a bad signature is a 400 (an OVH community thread and OVH's own n8n
        // guide show this exact body), not a 401/403.
        return (
            StatusCode::BAD_REQUEST,
            json!({"message": "Invalid signature", "httpCode": "400 Bad Request",
                   "errorCode": "INVALID_SIGNATURE"})
            .to_string(),
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
    if path == region_path() {
        return axum::Json(json!(["GRA11", "SBG5", "BHS5", "DE1"])).into_response();
    }
    if path == flavor_path() {
        let region = uri
            .query()
            .and_then(|q| q.strip_prefix("region="))
            .unwrap_or("");
        let region_over = s.lock().unwrap().region_overrides.get(region).cloned();
        if let Some((status, body)) = region_over {
            return (StatusCode::from_u16(status).unwrap(), body).into_response();
        }
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

fn set_region(knobs: &Shared, region: &str, status: u16, body: &str) {
    knobs
        .lock()
        .unwrap()
        .region_overrides
        .insert(region.to_string(), (status, body.to_string()));
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
            region_path(),
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
    assert_eq!(signed.len(), 3, "the project, the regions, the flavors");
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
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    // The stand-in answers OVH's 400 `INVALID_SIGNATURE` body; the words shown are ours.
    assert_eq!(
        msg,
        "permanent provider failure: 400 Bad Request: the signature was refused — check the \
         application secret"
    );
    for quoted in ["INVALID_SIGNATURE", "httpCode", "Invalid signature"] {
        assert!(!msg.contains(quoted), "{quoted} in {msg}");
    }
    assert_no_secret(&msg);
    let seen = knobs.lock().unwrap().seen.clone();
    assert!(
        seen.iter()
            .any(|s| !s.signature_ok && s.path_and_query != TIME)
    );
}

#[tokio::test]
async fn availability_follows_the_flavor_name_case_insensitively_and_the_linux_entry() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("gra11", &["L4-90", "l4-180", "win-only"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let s = &r.zones[0].stock;
    // The windows entry of l4-90 is unavailable; the linux one is available.
    assert_eq!(s["L4-90"], Stock::Available);
    // `available: false` is a stock-out.
    assert_eq!(s["l4-180"], Stock::Shortage);
    // Only a windows entry exists: its flag is the only signal there is.
    assert_eq!(s["win-only"], Stock::Available);
}

#[tokio::test]
async fn a_flavor_the_region_does_not_list_is_needs_you_and_stays_unknown() {
    let (base, _) = fake().await;
    // `l4-9O` is a typo (letter O) in a region that lists flavors: nothing to wait for.
    let r = checker(&base, &[("gra11", &["l4-90", "l4-9O"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: flavor l4-9O is not offered in region GRA11"
    );
    assert_eq!(r.zones[0].stock["l4-9O"], Stock::Unknown);
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Available);
    // The match is case-insensitive, so a differently cased name is not a typo.
    let r = checker(&base, &[("gra11", &["L4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
}

#[tokio::test]
async fn each_zone_is_read_in_its_own_region_in_failover_order() {
    let (base, _) = fake().await;
    let r = checker(&base, &[("sbg5", &["l4-90"]), ("gra11", &["l4-90"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
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
        // Not JSON, so nothing in it can be read as an `errorCode` or a `message`: the phrase
        // that would name the consumer key is in the raw text only.
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
            !msg.contains("credential is not valid") && !msg.contains("consumer key"),
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
    assert_eq!(
        msg,
        "permanent provider failure: 403 Forbidden: the consumer key's access rules do not \
         allow this call — create keys with GET /cloud/project/* on the createToken page"
    );
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
}

/// OVH says why it refused in `errorCode` or in a fixed `message`. The words shown are ours,
/// never the body's, and each reason has its own.
#[tokio::test]
async fn a_refusal_is_worded_by_its_reason_and_never_quotes_the_body() {
    let cases: [(u16, &str, &str); 6] = [
        // Live: a fake application key.
        (
            403,
            r#"{"class":"Client::Forbidden","message":"This application key is invalid"}"#,
            "403 Forbidden: the application key is not valid on this endpoint — keys only work \
             on the OVHcloud platform (EU, CA or US) they were created on",
        ),
        (
            403,
            r#"{"errorCode":"NOT_GRANTED_CALL","message":"Mind: THE-BODY-TEXT"}"#,
            "403 Forbidden: the consumer key's access rules do not allow this call — create \
             keys with GET /cloud/project/* on the createToken page",
        ),
        (
            403,
            r#"{"errorCode":"INVALID_CREDENTIAL","message":"Mind: THE-BODY-TEXT"}"#,
            "403 Forbidden: the consumer key is not valid (expired, revoked or never \
             validated) — create new keys",
        ),
        (
            401,
            r#"{"errorCode":"INVALID_SIGNATURE","message":"Mind: THE-BODY-TEXT"}"#,
            "401 Unauthorized: the signature was refused — check the application secret",
        ),
        // Live: an unsigned call. Nothing in it says more than "refused".
        (
            401,
            r#"{"class":"Client::Unauthorized","message":"You must login first"}"#,
            "401 Unauthorized: key rejected",
        ),
        (
            403,
            r#"{"errorCode":"FORBIDDEN","message":"Mind: THE-BODY-TEXT"}"#,
            "403 Forbidden: key rejected",
        ),
    ];
    for (status, body, want) in cases {
        let (base, knobs) = fake().await;
        set(&knobs, &project_path(), status, body);
        let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{body}");
        let (kind, msg) = r.last_error.clone().unwrap();
        assert_eq!(kind, "permanent", "{body}");
        assert_eq!(msg, format!("permanent provider failure: {want}"), "{body}");
        for leaked in [
            "THE-BODY-TEXT",
            "Client::",
            "NOT_GRANTED_CALL",
            "INVALID_",
            "You must login",
            "This application key is invalid",
        ] {
            assert!(!msg.contains(leaked), "{body}: {msg}");
        }
        assert_no_secret(&msg);
        assert_eq!(calls(&knobs), 2, "{body}: the clock and the project");
    }
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
    let (base, knobs) = fake().await;
    // The project lists XX9; it is the flavor read that does not know it.
    set(&knobs, &region_path(), 200, r#"["GRA11","XX9"]"#);
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
    // SBG5 is down (500, transient); XX9 does not exist (400, permanent).
    for order in [["sbg5", "xx9"], ["xx9", "sbg5"]] {
        let (base, knobs) = fake().await;
        set(&knobs, &region_path(), 200, r#"["SBG5","XX9"]"#);
        set_region(&knobs, "SBG5", 500, "down");
        let r = checker(&base, &[(order[0], &["l4-90"]), (order[1], &["l4-90"])])
            .check()
            .await;
        assert_eq!(r.state, CheckState::NeedsYou, "{order:?}");
        let (kind, msg) = r.last_error.clone().unwrap();
        assert_eq!(kind, "permanent", "{order:?}");
        assert!(msg.contains("Region XX9 not found"), "{order:?}: {msg}");
        // Both zones were read and both are reported.
        assert_eq!(r.zones.len(), 2, "{order:?}");
        assert!(r.zones.iter().all(|z| z.stock["l4-90"] == Stock::Unknown));
    }
    // An outage alone stays an outage.
    let (base, knobs) = fake().await;
    set_region(&knobs, "GRA11", 500, "down");
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(r.last_error.as_ref().unwrap().0, "transient");
}

/// The new needs-you kinds outrank an outage too: a zero quota (DE1) and a region the project
/// lacks (FR1), whichever zone is read first, next to a zone that is down (SBG5).
#[tokio::test]
async fn a_zero_quota_or_a_missing_region_outranks_an_outage_whichever_zone_comes_first() {
    for (other, kind, phrase) in [
        ("de1", "quota", "the project quota allows 0 instances"),
        (
            "fr1",
            "permanent",
            "region FR1 is not enabled in this project",
        ),
    ] {
        for order in [["sbg5", other], [other, "sbg5"]] {
            let (base, knobs) = fake().await;
            set_region(&knobs, "SBG5", 500, "down");
            let r = checker(&base, &[(order[0], &["l4-90"]), (order[1], &["l4-90"])])
                .check()
                .await;
            assert_eq!(r.state, CheckState::NeedsYou, "{order:?}");
            let (got_kind, msg) = r.last_error.clone().unwrap();
            assert_eq!(got_kind, kind, "{order:?}");
            assert!(msg.contains(phrase), "{order:?}: {msg}");
            assert_eq!(r.zones.len(), 2, "{order:?}");
        }
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
    set(&knobs, &region_path(), 200, r#"["GRA11&X=1"]"#);
    let r = checker(&base, &[("gra11&x=1", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let seen = knobs.lock().unwrap().seen.clone();
    let flavor_call = seen.last().unwrap();
    assert_eq!(
        flavor_call.path_and_query,
        format!("{}?region=GRA11%26X%3D1", flavor_path())
    );
    assert!(flavor_call.signature_ok);

    // The URL library strips an embedded tab, LF or CR from a segment before it applies the dot
    // rules, so `.\t.` would climb a level in the signed (and sent) path unless a control
    // character is refused: the project id is operator text and a segment of every signed call.
    for bad in [".\t.", ".\n.", "\t..", ".\r"] {
        let (base, knobs) = fake().await;
        let cred = pt_with(
            &[
                ("application_key", AK),
                ("application_secret", AS),
                ("consumer_key", CK),
            ],
            Some(bad),
        );
        let c = mm_fleet::ovh::checker(&cred, zones(&[("gra11", &["l4-90"])]), Some(&base));
        let r = c.check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{bad:?}");
        let seen = knobs.lock().unwrap().seen.clone();
        // Only the unsigned clock read is made: nothing is signed, least of all `/cloud/`.
        assert!(
            seen.iter().all(|s| s.path_and_query == TIME),
            "{bad:?}: {seen:?}"
        );
    }
}

/// Every other project id the URL library would rewrite or that carries a control character is
/// refused with a plain message before any signed call.
#[tokio::test]
async fn a_project_id_with_control_characters_or_dots_never_reaches_the_wire() {
    for bad in ["..", ".", "a\tb", "a\u{7f}b", ".\t.", ".\n.", "\t..", ".\r"] {
        let (base, knobs) = fake().await;
        let cred = pt_with(
            &[
                ("application_key", AK),
                ("application_secret", AS),
                ("consumer_key", CK),
            ],
            Some(bad),
        );
        let c = mm_fleet::ovh::checker(&cred, zones(&[("gra11", &["l4-90"])]), Some(&base));
        let r = c.check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{bad:?}");
        let (kind, msg) = r.last_error.clone().unwrap();
        assert_eq!(kind, "permanent", "{bad:?}");
        assert!(msg.contains("not a valid name"), "{bad:?}: {msg}");
        assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown, "{bad:?}");
        let seen = knobs.lock().unwrap().seen.clone();
        // Only the unsigned clock read is made; no signed call goes anywhere, least of all to
        // `/cloud/` or `/cloud/flavor`.
        let paths: Vec<&str> = seen.iter().map(|s| s.path_and_query.as_str()).collect();
        assert_eq!(paths, [TIME], "{bad:?}: {seen:?}");
    }
}

#[tokio::test]
async fn a_clock_the_provider_reports_as_absurd_is_unknown_and_nothing_is_signed() {
    let local = chrono::Utc::now().timestamp();
    for body in [
        i64::MIN.to_string(),
        i64::MAX.to_string(),
        (local + 86_401 + 5).to_string(),
        (local - 86_401 - 5).to_string(),
        "0".to_string(),
    ] {
        let (base, knobs) = fake().await;
        set(&knobs, TIME, 200, &body);
        let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
        assert_eq!(r.state, CheckState::Unknown, "{body}");
        let (kind, msg) = r.last_error.clone().unwrap();
        assert_eq!(kind, "transient", "{body}");
        assert_eq!(
            msg, "transient provider failure: clock: OVH answered an implausible server time",
            "{body}"
        );
        assert_eq!(
            calls(&knobs),
            1,
            "{body}: nothing is signed with that clock"
        );
        assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
    }
    // A day or less of skew is believed.
    let (base, knobs) = fake().await;
    set(&knobs, TIME, 200, &(local + 86_000).to_string());
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let seen = knobs.lock().unwrap().seen.clone();
    let signed: Vec<i64> = seen.iter().filter_map(|s| s.timestamp).collect();
    assert_eq!(signed.len(), 3);
    for ts in signed {
        assert!((ts - (local + 86_000)).abs() <= 5, "{ts}");
    }
}

/// python-ovh takes its local time after the answer has arrived. A slow `/auth/time` must not
/// push every timestamp into the future by the time the request took.
#[tokio::test]
async fn the_clock_delta_is_taken_after_the_answer_arrives() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().time_delay_ms = 4_000;
    let started = chrono::Utc::now().timestamp();
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let seen = knobs.lock().unwrap().seen.clone();
    let signed: Vec<i64> = seen.iter().filter_map(|s| s.timestamp).collect();
    assert_eq!(signed.len(), 3);
    for ts in signed {
        // The stand-in took 4 s to answer and read its clock at the end. The delta is measured
        // against the local clock when the answer has arrived (as python-ovh does), so a
        // request signed right after carries the stand-in's clock at that moment:
        // started + SKEW + 4. Measured before the request, the delta would include the 4 s
        // of waiting and the timestamp would be started + SKEW + 8. A slow machine only adds,
        // so the window is -1 to +2 s: wide enough for a stall, far from the wrong answer.
        let want = started + SKEW + 4;
        assert!(
            (want - 1..=want + 2).contains(&ts),
            "ts {ts}, want about {want} (started {started})"
        );
    }
}

/// A fragment on the endpoint is not sent, so it must not be signed either.
#[tokio::test]
async fn a_fragment_or_query_on_the_endpoint_is_neither_signed_nor_sent() {
    let (base, knobs) = fake().await;
    let c = mm_fleet::ovh::checker(
        &pt(),
        zones(&[("gra11", &["l4-90"])]),
        Some(&format!("{base}/#frag")),
    );
    let r = c.check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let seen = knobs.lock().unwrap().seen.clone();
    assert!(seen.iter().skip(1).all(|s| s.signature_ok), "{seen:?}");

    let (base, knobs) = fake().await;
    let c = mm_fleet::ovh::checker(
        &pt(),
        zones(&[("gra11", &["l4-90"])]),
        Some(&format!("{base}/?x=1#frag")),
    );
    let r = c.check().await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    let seen = knobs.lock().unwrap().seen.clone();
    let paths: Vec<&str> = seen.iter().map(|s| s.path_and_query.as_str()).collect();
    assert_eq!(
        paths,
        [
            TIME.to_string(),
            project_path(),
            region_path(),
            format!("{}?region=GRA11", flavor_path()),
        ]
    );
    assert!(seen.iter().skip(1).all(|s| s.signature_ok), "{seen:?}");
}

#[tokio::test]
async fn a_credential_a_header_cannot_carry_is_needs_you_and_nothing_is_signed() {
    let (base, knobs) = fake().await;
    let cred = pt_with(
        &[
            ("application_key", "OVH-BAD\nKEY"),
            ("application_secret", AS),
            ("consumer_key", CK),
        ],
        Some(PROJECT),
    );
    let c = mm_fleet::ovh::checker(&cred, zones(&[("gra11", &["l4-90"])]), Some(&base));
    let r = c.check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert!(
        msg.contains("characters an HTTP header cannot carry") && msg.contains("re-enter it"),
        "{msg}"
    );
    assert!(!msg.contains("OVH-BAD"), "{msg}");
    let seen = knobs.lock().unwrap().seen.clone();
    assert!(seen.iter().all(|s| !s.had_ovh_headers), "{seen:?}");
}

/// A new project's default quota (20 vCores, 40 GB) is below an L4 (22 vCores, 90 GB), and the
/// flavor list says so with `quota: 0` while `available` still says "in stock". Green here
/// would be followed by a rental that fails.
#[tokio::test]
async fn a_flavor_the_project_quota_does_not_allow_is_a_quota_error_and_keeps_its_stock() {
    let (base, _) = fake().await;
    // DE1: the linux entry of l4-90 has quota 0 (the windows entry's room does not count).
    // `l4-180` has quota 0 too, and is out of stock.
    let r = checker(&base, &[("de1", &["l4-90", "l4-180"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "quota");
    assert_eq!(
        msg,
        "provider quota exhausted: flavor l4-90 in region DE1: the project quota allows 0 \
         instances — raise the Public Cloud quota (project Settings → Quota & Regions)"
    );
    // The stock signal is the provider's, as before.
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Available);
    assert_eq!(r.zones[0].stock["l4-180"], Stock::Shortage);
    let row = r.to_status_row("p-1", 1, chrono::Utc::now());
    assert_eq!(row.state, "needs_you");
    assert_eq!(row.stock["de1"]["l4-90"], "available");

    // A zone that does have quota is untouched, and the quota error is the report's.
    let r = checker(&base, &[("gra11", &["l4-90"]), ("de1", &["l4-90"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    assert_eq!(r.last_error.as_ref().unwrap().0, "quota");
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Available);
    assert_eq!(r.zones[1].stock["l4-90"], Stock::Available);
}

/// Only a number that is zero escalates: a missing, null or non-numeric `quota` says nothing.
#[tokio::test]
async fn only_a_numeric_zero_quota_escalates() {
    let cases: [(&str, bool); 11] = [
        (r#""quota": 0"#, true),
        (r#""quota": 0.0"#, true),
        (r#""quota": 1"#, false),
        (r#""quota": 3"#, false),
        (r#""quota": -1"#, false),
        (r#""quota": null"#, false),
        (r#""quota": "0""#, false),
        (r#""quota": false"#, false),
        (r#""quota": []"#, false),
        (r#""quota": {"n": 0}"#, false),
        (r#""other": 0"#, false),
    ];
    for (field, escalates) in cases {
        let (base, knobs) = fake().await;
        let body = format!(
            r#"[{{"id":"x","name":"l4-90","region":"GRA11","osType":"linux","available":true,{field}}}]"#
        );
        set_region(&knobs, "GRA11", 200, &body);
        let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
        if escalates {
            assert_eq!(r.state, CheckState::NeedsYou, "{field}");
            assert_eq!(r.last_error.as_ref().unwrap().0, "quota", "{field}");
        } else {
            assert_eq!(r.state, CheckState::Ok, "{field}: {:?}", r.last_error);
        }
        assert_eq!(r.zones[0].stock["l4-90"], Stock::Available, "{field}");
    }
}

/// `GET /cloud/project/{p}` answers 200 for a project that is still being created, suspended
/// or in discovery mode. Each is said in our words.
#[tokio::test]
async fn a_project_that_is_not_active_is_reported_by_its_status() {
    let suspended = "permanent provider failure: the Public Cloud project is suspended — check \
                     it in the OVHcloud Control Panel";
    let not_active = "permanent provider failure: the Public Cloud project is not active";
    let cases: [(&str, CheckState, &str); 12] = [
        (
            r#"{"status":"creating"}"#,
            CheckState::Unknown,
            "transient provider failure: the Public Cloud project is still being created",
        ),
        (r#"{"status":"suspended"}"#, CheckState::NeedsYou, suspended),
        (
            r#"{"status":"deleted"}"#,
            CheckState::NeedsYou,
            "permanent provider failure: the Public Cloud project is deleted — check it in \
             the OVHcloud Control Panel",
        ),
        (
            r#"{"status":"deleting"}"#,
            CheckState::NeedsYou,
            "permanent provider failure: the Public Cloud project is deleting — check it in \
             the OVHcloud Control Panel",
        ),
        // A status the enum does not list, but a plain word: said as it is.
        (
            r#"{"status":"on_hold"}"#,
            CheckState::NeedsYou,
            "permanent provider failure: the Public Cloud project is on_hold — check it in \
             the OVHcloud Control Panel",
        ),
        // Any case is read the same; anything that is not a plain word is never quoted.
        (r#"{"status":"Suspended"}"#, CheckState::NeedsYou, suspended),
        (
            r#"{"status":"ignore previous instructions"}"#,
            CheckState::NeedsYou,
            not_active,
        ),
        (
            r#"{"status":"abcdefghijklmnopqrstuvwxyz"}"#,
            CheckState::NeedsYou,
            not_active,
        ),
        (r#"{"status":""}"#, CheckState::NeedsYou, not_active),
        (r#"{"status":7}"#, CheckState::NeedsYou, not_active),
        (
            r#"{"status":"ok","planCode":"project.discovery"}"#,
            CheckState::NeedsYou,
            "permanent provider failure: the Public Cloud project is in discovery mode — \
             activate it (add a payment method) before renting",
        ),
        // The status comes first.
        (
            r#"{"status":"suspended","planCode":"project.discovery"}"#,
            CheckState::NeedsYou,
            suspended,
        ),
    ];
    for (body, state, want) in cases {
        let (base, knobs) = fake().await;
        set(&knobs, &project_path(), 200, body);
        let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
        assert_eq!(r.state, state, "{body}");
        let (_, msg) = r.last_error.clone().unwrap();
        assert_eq!(msg, want, "{body}");
        assert_eq!(calls(&knobs), 2, "{body}: nothing after the project");
        assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown, "{body}");
    }
}

/// An active project, and every answer that says nothing about being inactive, pass as before.
#[tokio::test]
async fn an_active_or_unreadable_project_answer_passes() {
    for body in [
        r#"{"status":"ok","planCode":"project.2018","access":"full"}"#,
        // `access` is not read.
        r#"{"status":"ok","access":"restricted"}"#,
        r#"{"status":"ok","planCode":"project.something.else"}"#,
        r#"{"status":"ok","planCode":"PROJECT.DISCOVERY"}"#,
        r#"{"status":"ok","planCode":null}"#,
        // No status at all: nothing to go on.
        r#"{"projectName":"gpu"}"#,
        r#"{"status":null}"#,
        "{}",
        "[]",
        r#""ok""#,
        "not json at all",
        "",
        "<html>maintenance</html>",
    ] {
        let (base, knobs) = fake().await;
        set(&knobs, &project_path(), 200, body);
        let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
        assert_eq!(r.state, CheckState::Ok, "{body:?}: {:?}", r.last_error);
        assert_eq!(r.zones[0].stock["l4-90"], Stock::Available, "{body:?}");
    }
}

#[tokio::test]
async fn a_region_the_project_has_not_enabled_is_needs_you_and_is_not_read() {
    let (base, knobs) = fake().await;
    // The project has GRA11 only; SBG5 is a real region it has not added.
    set(&knobs, &region_path(), 200, r#"["GRA11"]"#);
    let r = checker(&base, &[("sbg5", &["l4-90"]), ("gra11", &["l4-90"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = r.last_error.clone().unwrap();
    assert_eq!(kind, "permanent");
    assert_eq!(
        msg,
        "permanent provider failure: region SBG5 is not enabled in this project — add it \
         under project Settings → Quota & Regions"
    );
    assert_eq!(r.zones.len(), 2);
    assert_eq!(r.zones[0].zone, "sbg5");
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
    // The other zone is read as usual.
    assert_eq!(r.zones[1].stock["l4-90"], Stock::Available);
    let seen = knobs.lock().unwrap().seen.clone();
    let paths: Vec<&str> = seen.iter().map(|s| s.path_and_query.as_str()).collect();
    assert_eq!(
        paths,
        [
            TIME.to_string(),
            project_path(),
            region_path(),
            format!("{}?region=GRA11", flavor_path()),
        ],
        "no flavor call for SBG5"
    );
    assert!(seen.iter().skip(1).all(|s| s.signature_ok), "{seen:?}");

    // The names are compared whatever their case.
    set(&knobs, &region_path(), 200, r#"["gra11","Sbg5"]"#);
    let r = checker(&base, &[("sbg5", &["l4-90"]), ("gra11", &["l4-90"])])
        .check()
        .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);

    // An empty list is an answer: no region is enabled.
    set(&knobs, &region_path(), 200, "[]");
    let r = checker(&base, &[("gra11", &["l4-90"])]).check().await;
    assert_eq!(r.state, CheckState::NeedsYou);
    assert_eq!(
        r.last_error.as_ref().unwrap().1,
        "permanent provider failure: region GRA11 is not enabled in this project — add it under \
         project Settings → Quota & Regions"
    );
    assert_eq!(r.zones[0].stock["l4-90"], Stock::Unknown);
}

/// The region list is informational: the operator's keys may lack the `GET .../region` rule,
/// and any failure of the call must leave the check as it was without it.
#[tokio::test]
async fn a_region_list_that_cannot_be_read_changes_nothing() {
    for (status, body) in [
        (
            403u16,
            r#"{"errorCode":"NOT_GRANTED_CALL","message":"This call has not been granted"}"#,
        ),
        (401, r#"{"message":"You must login first"}"#),
        (404, r#"{"message":"nope"}"#),
        (429, "slow down"),
        (500, "down"),
        (503, ""),
        (200, "<html>maintenance</html>"),
        (200, r#"{"regions":["GRA11"]}"#),
        (200, "[1, 2]"),
        (200, "null"),
        (200, ""),
    ] {
        let (base, knobs) = fake().await;
        set(&knobs, &region_path(), status, body);
        let r = checker(&base, &[("gra11", &["l4-90"]), ("sbg5", &["l4-90"])])
            .check()
            .await;
        let why = format!("{status} {body}");
        assert_eq!(r.state, CheckState::Ok, "{why}: {:?}", r.last_error);
        assert_eq!(r.last_error, None, "{why}");
        assert_eq!(r.zones[0].stock["l4-90"], Stock::Available, "{why}");
        assert_eq!(r.zones[1].stock["l4-90"], Stock::Shortage, "{why}");
        let seen = knobs.lock().unwrap().seen.clone();
        let flavor_calls = seen
            .iter()
            .filter(|s| s.path_and_query.starts_with(&flavor_path()))
            .count();
        assert_eq!(flavor_calls, 2, "{why}: the flavor reads still happen");
    }
}

/// A trailing slash on the endpoint must not double up in the path: the URL that is signed is
/// the URL that is sent. The stand-in sits under the API's `/1.0` and checks the signature
/// against the URL as it received it.
#[tokio::test]
async fn a_trailing_slash_on_the_endpoint_does_not_change_the_signed_url() {
    for suffix in ["/1.0", "/1.0/", "/1.0//", "/1.0/#frag", "/1.0/?x=1"] {
        let (base, knobs) = fake().await;
        let c = mm_fleet::ovh::checker(
            &pt(),
            zones(&[("gra11", &["l4-90"])]),
            Some(&format!("{base}{suffix}")),
        );
        let r = c.check().await;
        assert_eq!(r.state, CheckState::Ok, "{suffix}: {:?}", r.last_error);
        let seen = knobs.lock().unwrap().seen.clone();
        let paths: Vec<&str> = seen.iter().map(|s| s.path_and_query.as_str()).collect();
        assert_eq!(
            paths,
            [
                format!("/1.0{TIME}"),
                format!("/1.0{}", project_path()),
                format!("/1.0{}", region_path()),
                format!("/1.0{}?region=GRA11", flavor_path()),
            ],
            "{suffix}"
        );
        assert!(
            seen.iter().skip(1).all(|s| s.signature_ok),
            "{suffix}: {seen:?}"
        );
    }
}
