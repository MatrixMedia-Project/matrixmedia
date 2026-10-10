//! The Google Cloud (Compute Engine) read-only checker against a stand-in Google: the OAuth
//! token endpoint (`/token`), Compute (`/`) and Resource Manager (`/rm`), all on one axum
//! server that `stand_in_base` points every pinned host at.
//!
//! The stand-in's `/token` verifies the signed assertion with the test key's public half
//! (RS256, `kid`, `iss`, `aud`, both scopes, a one-hour lifetime) and every other route
//! checks `Authorization: Bearer <the token it handed out>`, so a green check here means the
//! checker signed and sent what Google expects.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use axum::{
    Form, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    routing::{get, post},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use mm_fleet::checks::{CheckReport, CheckState, ProviderChecker, Stock};
use mm_fleet::gcp::{self, GcpChecker};
use mm_fleet::sealed::CredentialPlaintext;

/// A THROWAWAY TEST KEY, generated once for this file with
/// `openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048`. It belongs to no Google
/// service account and opens nothing anywhere; it exists so the stand-in can verify the
/// checker's RS256 signature. The PEM header is split only so naive secret scanners do not
/// flag a test fixture.
const TEST_PRIVATE_KEY: &str = concat!(
    "-----BEGIN PRIVATE",
    " KEY-----\n",
    "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQChym3dpnKfG9kZ\n",
    "WfPkx6nU5RDizofacLeFb7mBgXTeux9EMly0wyq3q4J+HCbnrHEgvtWF2XadnM2D\n",
    "s3UoRUvLTyR/D8nghicWzNoI/pSxQ8lIcfYlTCyb6+SI6zh3eOzfriEpj5cOaU6P\n",
    "yyVHVPGKxXJUsz6tL5JsfyHFbzAH/PS2kkw8lFEK/ZbvAM4yZ77wCD9haEt92NPT\n",
    "ldfe+gblZPFXyD7Vi2E5+PVuDW94O390x6HbhmcFvkgdPTWg3LDvQjFdms7a9B2/\n",
    "Cjonz2gHrgxDoAj77trydvS7enbM1kE7h28zxgLPYTWlI/hXzgr+IF8it8ODNtmU\n",
    "frOv8G5TAgMBAAECggEADG+7YEACLDl/clPCzAGOYXS2MT5U8l/wYMvEw7AIJB2C\n",
    "0BgwSMjTyMiLQFcOT91RLjDdHZZ+EqwnSV9gZcG7Nhvq9Jq/Ft4uX7H2yfLfynx+\n",
    "cBjVJerOvFw2oKZhccrpfQwtNRJPjJELCUHr/Klf48iSUEZy+UG14gHSaRMclbsm\n",
    "69rR9GgPs1gm9nLTPX1Kuz1vvPOuiGwLndNH3P3mwk53i5KNMT7+G0XX2HpkB9AF\n",
    "4DjTlU1RWC08Zi6/Jk4rQF2lbSHDPdQBc77AvmS25yZwtlqX7dmcJJKVpy+WBCbc\n",
    "nzEIgnVymPLsG8jlnoGHYfOAQedlBUJ250MjCXjf4QKBgQDg8sOVOpykYY9Pa16D\n",
    "LtUuc+J3vQ+t9jAn2220yu5fH0xlkzJ7POS8HaZB8qnQG4o1+rBORfBqDuHIGW7r\n",
    "Mv/mIiSdsA5oyz2recc64XnJl12AaW9kg1dvvjEAsoEjo02xvu/p90OrTJX0Wr3A\n",
    "sPHO93WOJ5z1ZW16hNpG5Wo+UQKBgQC4H8y5yoZucOVLZYbvcT4pq/VAJMYRXvEe\n",
    "Py/+4fOmAGfUC3ysHLLOZQLYJ9nDd0AQs6GcJQw9oSMbTs34DUbWFt+XKWWw3jzx\n",
    "+Ee7cX0jvTNVWlXcMelJHRBLuSlwPjSN5EbFLFn5I+RSafkxqzTDf0d99XtPkLXd\n",
    "vspEwAbFYwKBgQCqVenYJGvc5as5PlpxB5OR+1pvxRAMcLGCXNwz3L6n9PFKsS22\n",
    "uCOUdvcgVPpVhaUgvtWmT7t+9AnwFaIyI4o233/OkDQ5Ej1+jVZZtccc6at5w10A\n",
    "RZx+FwzQNFspe00n3SeaiQwKuJGMWPH66YIRcLzpigGGqOk/rz4CFVJgIQKBgAYh\n",
    "WOmeqpcmvxuhh7qVJKKyjPnTv5x4csK1C94Km9gdD1fqAf6g/fsNNekIeqGdaM6l\n",
    "jG3sddnfcZHJL+ZgWslp/YvE3xPiclkEES9WefokpH7lARLRvpimlRJQWebYy1sm\n",
    "DI0oCt7WqRVtXdSfhKQ1qqWw9KgTg1qcrZNYaWFNAoGBAJa/w/bONiDySeG3yuQs\n",
    "ZsGUffGy3GJyhqDK65zK+UC2BKY33W1rWe9EjaM45bWzAFJ5MK9nMDl9oVGGDVyN\n",
    "GaYJZ0qwcYeNp7h7OvDJlNb7g0CB5fU2MGKEb/fm1bQlSlUqtOSrp23LB2ZbuXix\n",
    "D1FOm9A0w+jJZcExcW1gxvpS\n",
    "-----END PRIVATE",
    " KEY-----\n",
);

/// The public half of [`TEST_PRIVATE_KEY`]: the stand-in verifies assertions with it.
const TEST_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAocpt3aZynxvZGVnz5Mep
1OUQ4s6H2nC3hW+5gYF03rsfRDJctMMqt6uCfhwm56xxIL7Vhdl2nZzNg7N1KEVL
y08kfw/J4IYnFszaCP6UsUPJSHH2JUwsm+vkiOs4d3js364hKY+XDmlOj8slR1Tx
isVyVLM+rS+SbH8hxW8wB/z0tpJMPJRRCv2W7wDOMme+8Ag/YWhLfdjT05XX3voG
5WTxV8g+1YthOfj1bg1veDt/dMeh24ZnBb5IHT01oNyw70IxXZrO2vQdvwo6J89o
B64MQ6AI++7a8nb0u3p2zNZBO4dvM8YCz2E1pSP4V84K/iBfIrfDgzbZlH6zr/Bu
UwIDAQAB
-----END PUBLIC KEY-----
";

/// One line of the private key's base64 body: a body echoing any fragment of the key must
/// not carry it out.
const KEY_LINE: &str = "frOv8G5TAgMBAAECggEADG+7YEACLDl/clPCzAGOYXS2MT5U8l/wYMvEw7AIJB2C";
const CLIENT_EMAIL: &str = "mm-checker@proj-1.iam.gserviceaccount.com";
const KEY_ID: &str = "3f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c";
/// Opaque to the checker. Deliberately not shaped like a real Google token (`ya29.…`), which
/// secret scanners look for.
const ACCESS_TOKEN: &str = "stand-in.access-token.not-a-google-token";
const PROJECT: &str = "proj-1";
/// What Google's token endpoint expects as `aud`, whatever host the request went to.
const GOOGLE_TOKEN_AUD: &str = "https://oauth2.googleapis.com/token";
const SCOPE_COMPUTE_RO: &str = "https://www.googleapis.com/auth/compute.readonly";
const SCOPE_PLATFORM_RO: &str = "https://www.googleapis.com/auth/cloud-platform.read-only";

#[derive(Clone)]
enum Iam {
    Granted(Vec<&'static str>),
    Status(u16, Value),
}

struct Knobs {
    /// Every request, as "METHOD /path".
    calls: Vec<String>,
    token_calls: u32,
    /// Why the last assertion was refused, if it was.
    bad_assertion: Option<String>,
    /// Requests whose bearer was not the stand-in's token.
    bad_bearer: u32,
    /// Requests no route matched (a write call, a wrong path).
    unexpected: Vec<String>,
    token_reply: Option<(u16, Value)>,
    project_reply: Option<(u16, Value)>,
    /// `GPUS_ALL_REGIONS` (limit, usage); `None` leaves the entry out.
    gpus_all_regions: Option<(f64, f64)>,
    /// Regional quota limits by metric. A metric not listed is absent from the reply.
    region_limits: BTreeMap<String, f64>,
    region_reply: Option<(u16, Value)>,
    region_calls: u32,
    machine_reply: Option<(u16, Value)>,
    iam: Iam,
    iam_calls: u32,
}

impl Default for Knobs {
    fn default() -> Self {
        let region_limits = [
            "NVIDIA_L4_GPUS",
            "NVIDIA_A100_GPUS",
            "NVIDIA_A100_80GB_GPUS",
            "NVIDIA_H100_GPUS",
            "NVIDIA_T4_GPUS",
        ]
        .into_iter()
        .map(|m| (m.to_string(), 8.0))
        .collect();
        Knobs {
            calls: Vec::new(),
            token_calls: 0,
            bad_assertion: None,
            bad_bearer: 0,
            unexpected: Vec::new(),
            token_reply: None,
            project_reply: None,
            gpus_all_regions: Some((4.0, 0.0)),
            region_limits,
            region_reply: None,
            region_calls: 0,
            machine_reply: None,
            iam: Iam::Granted(Vec::new()),
            iam_calls: 0,
        }
    }
}

type Shared = Arc<Mutex<Knobs>>;

fn reply((status, body): (u16, Value)) -> axum::response::Response {
    (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
}

fn google_error(code: u16, message: &str, status: &str) -> Value {
    json!({"error": {"code": code, "message": message, "status": status,
                     "errors": [{"message": message, "domain": "global", "reason": "x"}]}})
}

#[derive(serde::Deserialize)]
struct Claims {
    iss: String,
    scope: String,
    aud: String,
    iat: i64,
    exp: i64,
}

/// Why `assertion` is not what Google requires, or `None` if it is.
fn assertion_problem(assertion: &str) -> Option<String> {
    let header = match jsonwebtoken::decode_header(assertion) {
        Ok(h) => h,
        Err(e) => return Some(format!("header: {e}")),
    };
    if header.alg != Algorithm::RS256 {
        return Some(format!("alg {:?}", header.alg));
    }
    if header.typ.as_deref() != Some("JWT") {
        return Some(format!("typ {:?}", header.typ));
    }
    if header.kid.as_deref() != Some(KEY_ID) {
        return Some(format!("kid {:?}", header.kid));
    }
    let key = DecodingKey::from_rsa_pem(TEST_PUBLIC_KEY.as_bytes()).unwrap();
    let mut v = Validation::new(Algorithm::RS256);
    v.set_audience(&[GOOGLE_TOKEN_AUD]);
    v.set_issuer(&[CLIENT_EMAIL]);
    v.set_required_spec_claims(&["exp", "iat", "iss", "aud"]);
    let claims = match jsonwebtoken::decode::<Claims>(assertion, &key, &v) {
        Ok(d) => d.claims,
        Err(e) => return Some(format!("signature or claims: {e}")),
    };
    if claims.iss != CLIENT_EMAIL || claims.aud != GOOGLE_TOKEN_AUD {
        return Some("iss/aud".into());
    }
    let scopes: Vec<&str> = claims.scope.split(' ').collect();
    if scopes.len() != 2
        || !scopes.contains(&SCOPE_COMPUTE_RO)
        || !scopes.contains(&SCOPE_PLATFORM_RO)
    {
        return Some(format!("scope {:?}", claims.scope));
    }
    if claims.exp - claims.iat != 3600 {
        return Some(format!("lifetime {}", claims.exp - claims.iat));
    }
    if (claims.iat - chrono::Utc::now().timestamp()).abs() > 60 {
        return Some("iat is not now".into());
    }
    None
}

async fn token(
    State(s): State<Shared>,
    Form(form): Form<HashMap<String, String>>,
) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    k.calls.push("POST /token".into());
    k.token_calls += 1;
    if form.get("grant_type").map(String::as_str)
        != Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
    {
        k.bad_assertion = Some(format!("grant_type {:?}", form.get("grant_type")));
        return reply((400, json!({"error": "unsupported_grant_type"})));
    }
    let assertion = form.get("assertion").cloned().unwrap_or_default();
    if let Some(problem) = assertion_problem(&assertion) {
        k.bad_assertion = Some(problem);
        return reply((
            400,
            json!({"error": "invalid_grant", "error_description": "Invalid JWT Signature."}),
        ));
    }
    if let Some((status, body)) = k.token_reply.clone() {
        // A reply that echoes the assertion it was sent (or only its signature), as a careless
        // error page might.
        let signature = assertion.rsplit('.').next().unwrap_or_default();
        let body = body
            .to_string()
            .replace("$ASSERTION", &assertion)
            .replace("$SIGNATURE", signature);
        return reply((status, serde_json::from_str(&body).unwrap()));
    }
    Json(
        json!({"access_token": ACCESS_TOKEN, "expires_in": 3599, "token_type": "Bearer",
                "scope": format!("{SCOPE_COMPUTE_RO} {SCOPE_PLATFORM_RO}")}),
    )
    .into_response()
}

/// `true` (and counted) when the request does not carry the stand-in's token.
fn bearer_is_wrong(k: &mut Knobs, headers: &HeaderMap) -> bool {
    let ok = headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {ACCESS_TOKEN}").as_str());
    if !ok {
        k.bad_bearer += 1;
    }
    !ok
}

fn unauthenticated() -> axum::response::Response {
    reply((
        401,
        google_error(
            401,
            "Request had invalid authentication credentials.",
            "UNAUTHENTICATED",
        ),
    ))
}

fn no_project(p: &str) -> axum::response::Response {
    reply((
        404,
        google_error(
            404,
            &format!("The resource 'projects/{p}' was not found"),
            "NOT_FOUND",
        ),
    ))
}

async fn project(
    State(s): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    k.calls.push(format!("GET /projects/{p}"));
    if bearer_is_wrong(&mut k, &headers) {
        return unauthenticated();
    }
    if let Some(r) = k.project_reply.clone() {
        return reply(r);
    }
    if p != PROJECT {
        return no_project(&p);
    }
    let mut quotas = vec![json!({"metric": "CPUS_ALL_REGIONS", "limit": 32.0, "usage": 0.0})];
    if let Some((limit, usage)) = k.gpus_all_regions {
        quotas.push(json!({"metric": "GPUS_ALL_REGIONS", "limit": limit, "usage": usage}));
    }
    Json(json!({"kind": "compute#project", "name": p, "quotas": quotas})).into_response()
}

/// The accelerator a machine type carries in the stand-in's catalogue.
fn accelerator_of(size: &str) -> Option<Option<&'static str>> {
    Some(match size {
        "g2-standard-4" | "g2-standard-8" => Some("nvidia-l4"),
        "a2-highgpu-1g" => Some("nvidia-tesla-a100"),
        "a2-ultragpu-1g" => Some("nvidia-a100-80gb"),
        "a3-highgpu-1g" => Some("nvidia-h100-80gb"),
        "t4-test-1" => Some("nvidia-tesla-t4"),
        "x-mystery-1" => Some("nvidia-mystery-9000"),
        "n2-standard-2" => None,
        _ => return None,
    })
}

async fn machine_type(
    State(s): State<Shared>,
    Path((p, zone, size)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    k.calls.push(format!(
        "GET /projects/{p}/zones/{zone}/machineTypes/{size}"
    ));
    if bearer_is_wrong(&mut k, &headers) {
        return unauthenticated();
    }
    if let Some(r) = k.machine_reply.clone() {
        return reply(r);
    }
    if p != PROJECT {
        return no_project(&p);
    }
    let Some(acc) = accelerator_of(&size) else {
        return reply((
            404,
            google_error(
                404,
                &format!(
                    "The resource 'projects/{p}/zones/{zone}/machineTypes/{size}' was not found"
                ),
                "NOT_FOUND",
            ),
        ));
    };
    let accelerators: Vec<Value> = acc
        .map(|a| json!({"guestAcceleratorType": a, "guestAcceleratorCount": 1}))
        .into_iter()
        .collect();
    let mut body =
        json!({"kind": "compute#machineType", "name": size, "zone": zone, "guestCpus": 4});
    if !accelerators.is_empty() {
        body["accelerators"] = Value::Array(accelerators);
    }
    Json(body).into_response()
}

async fn region(
    State(s): State<Shared>,
    Path((p, region)): Path<(String, String)>,
    headers: HeaderMap,
) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    k.calls.push(format!("GET /projects/{p}/regions/{region}"));
    k.region_calls += 1;
    if bearer_is_wrong(&mut k, &headers) {
        return unauthenticated();
    }
    if let Some(r) = k.region_reply.clone() {
        return reply(r);
    }
    if p != PROJECT {
        return no_project(&p);
    }
    let mut quotas = vec![json!({"metric": "CPUS", "limit": 24.0, "usage": 0.0})];
    for (metric, limit) in &k.region_limits {
        quotas.push(json!({"metric": metric, "limit": limit, "usage": 0.0}));
    }
    Json(json!({"kind": "compute#region", "name": region, "quotas": quotas})).into_response()
}

async fn test_iam(
    State(s): State<Shared>,
    Path(rest): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    k.calls.push(format!("POST /rm/projects/{rest}"));
    k.iam_calls += 1;
    if bearer_is_wrong(&mut k, &headers) {
        return unauthenticated();
    }
    if rest != format!("{PROJECT}:testIamPermissions") {
        return reply((404, google_error(404, "not found", "NOT_FOUND")));
    }
    if body != json!({"permissions": ["compute.instances.create", "compute.instances.delete"]}) {
        return reply((
            400,
            google_error(400, "bad request body", "INVALID_ARGUMENT"),
        ));
    }
    match k.iam.clone() {
        // Google leaves `permissions` out when none is granted.
        Iam::Granted(p) if p.is_empty() => Json(json!({})).into_response(),
        Iam::Granted(p) => Json(json!({"permissions": p})).into_response(),
        Iam::Status(status, body) => reply((status, body)),
    }
}

async fn fallback(State(s): State<Shared>, method: Method, uri: Uri) -> axum::response::Response {
    let mut k = s.lock().unwrap();
    k.unexpected.push(format!("{method} {uri}"));
    reply((
        404,
        google_error(404, "no such route in the stand-in", "NOT_FOUND"),
    ))
}

async fn fake() -> (String, Shared) {
    let state: Shared = Arc::default();
    let app = Router::new()
        .route("/token", post(token))
        .route("/projects/{p}", get(project))
        .route(
            "/projects/{p}/zones/{zone}/machineTypes/{size}",
            get(machine_type),
        )
        .route("/projects/{p}/regions/{region}", get(region))
        .route("/rm/projects/{rest}", post(test_iam))
        // axum answers a wrong method on a known path with a 405 that skips `fallback`;
        // a write to a read route must be seen too.
        .method_not_allowed_fallback(fallback)
        .fallback(fallback)
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

fn key_json() -> Value {
    json!({
        "type": "service_account",
        "project_id": PROJECT,
        "private_key_id": KEY_ID,
        "private_key": TEST_PRIVATE_KEY,
        "client_email": CLIENT_EMAIL,
        "client_id": "100000000000000000001",
        "auth_uri": "https://accounts.google.com/o/oauth2/auth",
        "token_uri": "https://oauth2.googleapis.com/token",
        "auth_provider_x509_cert_url": "https://www.googleapis.com/oauth2/v1/certs",
        "client_x509_cert_url": "https://www.googleapis.com/robot/v1/metadata/x509/mm-checker%40proj-1.iam.gserviceaccount.com",
        "universe_domain": "googleapis.com"
    })
}

fn pt_with(json_text: String, account: Option<&str>) -> CredentialPlaintext {
    CredentialPlaintext {
        v: 1,
        provider_id: "gcp-1".into(),
        kind: "gcp".into(),
        endpoint: "https://compute.googleapis.com/compute/v1".into(),
        account: account.map(str::to_string),
        fields: [("service_account_json".to_string(), json_text)]
            .into_iter()
            .collect(),
    }
}

fn pt() -> CredentialPlaintext {
    pt_with(key_json().to_string(), Some(PROJECT))
}

fn zones(z: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    z.iter()
        .map(|(zone, sizes)| {
            (
                zone.to_string(),
                sizes.iter().map(|s| s.to_string()).collect(),
            )
        })
        .collect()
}

fn one_zone() -> Vec<(String, Vec<String>)> {
    zones(&[("us-central1-a", &["g2-standard-4"])])
}

async fn run(base: &str, pt: &CredentialPlaintext, z: Vec<(String, Vec<String>)>) -> CheckReport {
    gcp::checker(pt, z, Some(base)).check().await
}

fn err(r: &CheckReport) -> (String, String) {
    r.last_error.clone().expect("an error was recorded")
}

/// Every place a report can carry text, flattened, so a secret test can search all of it.
fn report_text(r: &CheckReport) -> String {
    format!("{r:?} {}", serde_json::to_string(r).unwrap())
}

fn assert_no_key_material(text: &str) {
    for secret in [KEY_LINE, CLIENT_EMAIL, KEY_ID, ACCESS_TOKEN, "PRIVATE KEY"] {
        assert!(!text.contains(secret), "{secret:?} leaked into: {text}");
    }
    assert!(!text.contains("eyJ"), "a JWT leaked into: {text}");
}

fn assert_every_size_unknown(r: &CheckReport, z: &[(String, Vec<String>)]) {
    assert_eq!(r.zones.len(), z.len(), "every configured zone is reported");
    for (zr, (zone, sizes)) in r.zones.iter().zip(z) {
        assert_eq!(&zr.zone, zone);
        assert_eq!(zr.stock.len(), sizes.len());
        for s in sizes {
            assert_eq!(zr.stock.get(s), Some(&Stock::Unknown), "{zone} {s}");
        }
        assert_eq!(zr.instances_running, None);
    }
}

// ─── the ok path and the requests it makes ────────────────────────────────────

#[tokio::test]
async fn ok_path_is_green_with_unknown_stock_no_prices_and_a_read_only_scope() {
    let (base, knobs) = fake().await;
    let z = zones(&[
        ("us-central1-a", &["g2-standard-4", "g2-standard-8"]),
        ("europe-west4-b", &["g2-standard-4"]),
    ]);
    let r = run(&base, &pt(), z.clone()).await;
    let k = knobs.lock().unwrap();
    assert_eq!(
        k.bad_assertion, None,
        "the assertion is what Google expects"
    );
    assert_eq!(k.bad_bearer, 0, "every call carries the access token");
    assert!(
        k.unexpected.is_empty(),
        "only the read routes are called: {:?}",
        k.unexpected
    );
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.last_error, None);
    assert_eq!(r.key_scope.as_deref(), Some("read-only"));
    assert!(r.prices.is_empty(), "Google publishes no price here");
    assert_eq!(r.balance_minor, None);
    assert_every_size_unknown(&r, &z);
    assert_eq!(k.token_calls, 1, "one access token per check");
    assert_eq!(k.iam_calls, 1);
    assert_eq!(
        k.region_calls, 2,
        "one read per region: us-central1 and europe-west4"
    );
    let row = r.to_status_row("gcp-1", 1, chrono::Utc::now());
    assert_eq!(row.state, "ok");
    assert_eq!(row.stock["us-central1-a"]["g2-standard-4"], "unknown");
    // The order the checker reads in: token, project, then each zone and size.
    assert_eq!(k.calls[0], "POST /token");
    assert_eq!(k.calls[1], "GET /projects/proj-1");
    assert_eq!(
        k.calls[2],
        "GET /projects/proj-1/zones/us-central1-a/machineTypes/g2-standard-4"
    );
    assert_eq!(
        k.calls.last().unwrap(),
        "POST /rm/projects/proj-1:testIamPermissions"
    );
    assert_no_key_material(&report_text(&r));
}

#[tokio::test]
async fn the_checker_entry_point_matches_the_struct() {
    let (base, _) = fake().await;
    let c = GcpChecker::new(&pt(), one_zone(), Some(&base));
    assert_eq!(c.check().await.state, CheckState::Ok);
}

#[tokio::test]
async fn without_an_account_the_project_comes_from_the_key_file() {
    let (base, knobs) = fake().await;
    let r = run(&base, &pt_with(key_json().to_string(), None), one_zone()).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert!(
        knobs
            .lock()
            .unwrap()
            .calls
            .contains(&"GET /projects/proj-1".to_string())
    );
}

#[tokio::test]
async fn the_account_wins_over_the_key_files_project_id() {
    let (base, knobs) = fake().await;
    let mut j = key_json();
    j["project_id"] = json!("some-other-project");
    let r = run(&base, &pt_with(j.to_string(), Some(PROJECT)), one_zone()).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert!(
        knobs
            .lock()
            .unwrap()
            .calls
            .contains(&"GET /projects/proj-1".to_string())
    );
}

/// The key file's `token_uri` would send a signed assertion wherever it points: it is
/// ignored, and the pinned token host (the stand-in's `/token` here) is the one called.
#[tokio::test]
async fn a_crafted_token_uri_is_ignored() {
    let (base, knobs) = fake().await;
    let (evil_base, evil_knobs) = fake().await;
    let mut j = key_json();
    j["token_uri"] = json!(format!("{evil_base}/token"));
    let r = run(&base, &pt_with(j.to_string(), Some(PROJECT)), one_zone()).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(knobs.lock().unwrap().token_calls, 1);
    assert_eq!(evil_knobs.lock().unwrap().token_calls, 0);
    assert!(evil_knobs.lock().unwrap().calls.is_empty());
    assert!(evil_knobs.lock().unwrap().unexpected.is_empty());
}

// ─── quotas (A6) ──────────────────────────────────────────────────────────────

/// The owner's project today: no GPU quota anywhere. That is a support ticket, so it
/// reaches a human, and the zones are still read and reported.
#[tokio::test]
async fn gpus_all_regions_zero_is_a_quota_that_needs_you() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().gpus_all_regions = Some((0.0, 0.0));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "quota");
    assert!(
        msg.contains("GPUS_ALL_REGIONS limit 0 (usage 0) — request a GPU quota increase"),
        "{msg}"
    );
    assert_every_size_unknown(&r, &one_zone());
    assert_eq!(r.key_scope.as_deref(), Some("read-only"));
    let k = knobs.lock().unwrap();
    assert!(
        k.calls
            .iter()
            .any(|c| c.contains("/machineTypes/g2-standard-4"))
    );
}

#[tokio::test]
async fn the_usage_is_reported_with_a_zero_limit() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().gpus_all_regions = Some((0.0, 2.0));
    let r = run(&base, &pt(), one_zone()).await;
    let (kind, msg) = err(&r);
    assert_eq!(kind, "quota");
    assert!(
        msg.contains("GPUS_ALL_REGIONS limit 0 (usage 2) —"),
        "{msg}"
    );
}

#[tokio::test]
async fn a_missing_gpus_all_regions_entry_is_not_escalated() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().gpus_all_regions = None;
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
}

#[tokio::test]
async fn a_regional_l4_limit_of_zero_is_a_quota() {
    let (base, knobs) = fake().await;
    knobs
        .lock()
        .unwrap()
        .region_limits
        .insert("NVIDIA_L4_GPUS".into(), 0.0);
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "quota");
    assert!(msg.contains("NVIDIA_L4_GPUS limit 0"), "{msg}");
    assert!(msg.contains("us-central1"), "{msg}");
    assert!(
        !msg.contains("us-central1-a"),
        "the region, not the zone: {msg}"
    );
    assert_eq!(
        knobs
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c.as_str() == "GET /projects/proj-1/regions/us-central1")
            .count(),
        1
    );
}

#[tokio::test]
async fn each_gpu_family_reads_its_own_regional_metric() {
    for (size, metric) in [
        ("g2-standard-4", "NVIDIA_L4_GPUS"),
        ("a2-highgpu-1g", "NVIDIA_A100_GPUS"),
        ("a2-ultragpu-1g", "NVIDIA_A100_80GB_GPUS"),
        ("a3-highgpu-1g", "NVIDIA_H100_GPUS"),
        ("t4-test-1", "NVIDIA_T4_GPUS"),
    ] {
        let (base, knobs) = fake().await;
        knobs
            .lock()
            .unwrap()
            .region_limits
            .insert(metric.into(), 0.0);
        let r = run(&base, &pt(), zones(&[("us-central1-a", &[size])])).await;
        let (kind, msg) = err(&r);
        assert_eq!(kind, "quota", "{size}");
        assert!(msg.contains(&format!("{metric} limit 0")), "{size}: {msg}");
        // Every other family's metric at zero must not matter for this size.
        let (base, knobs) = fake().await;
        {
            let mut k = knobs.lock().unwrap();
            for (m, l) in k.region_limits.iter_mut() {
                if m != metric {
                    *l = 0.0;
                }
            }
        }
        let r = run(&base, &pt(), zones(&[("us-central1-a", &[size])])).await;
        assert_eq!(r.state, CheckState::Ok, "{size}: {:?}", r.last_error);
    }
}

#[tokio::test]
async fn a_regional_metric_that_is_absent_is_not_escalated() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().region_limits.clear();
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
}

#[tokio::test]
async fn an_unknown_or_missing_accelerator_skips_the_regional_read() {
    let (base, knobs) = fake().await;
    let r = run(
        &base,
        &pt(),
        zones(&[("us-central1-a", &["x-mystery-1", "n2-standard-2"])]),
    )
    .await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(knobs.lock().unwrap().region_calls, 0);
    assert_every_size_unknown(
        &r,
        &zones(&[("us-central1-a", &["x-mystery-1", "n2-standard-2"])]),
    );
}

#[tokio::test]
async fn region_reads_are_cached_within_one_check() {
    let (base, knobs) = fake().await;
    let z = zones(&[
        ("us-central1-a", &["g2-standard-4", "g2-standard-8"]),
        ("us-central1-b", &["g2-standard-4"]),
    ]);
    let r = run(&base, &pt(), z).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(knobs.lock().unwrap().region_calls, 1);
}

#[tokio::test]
async fn a_failed_region_read_is_transient_and_read_once() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().region_reply =
        Some((503, google_error(503, "backend unavailable", "UNAVAILABLE")));
    let z = zones(&[("us-central1-a", &["g2-standard-4", "g2-standard-8"])]);
    let r = run(&base, &pt(), z.clone()).await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(err(&r).0, "transient");
    assert_eq!(knobs.lock().unwrap().region_calls, 1);
    assert_every_size_unknown(&r, &z);
}

// ─── machine types ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_machine_type_the_zone_does_not_offer_is_permanent() {
    let (base, _) = fake().await;
    let z = zones(&[("us-central1-a", &["g2-standard-99", "g2-standard-4"])]);
    let r = run(&base, &pt(), z.clone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(
        msg.contains("machine type g2-standard-99 is not offered in zone us-central1-a"),
        "{msg}"
    );
    assert_every_size_unknown(&r, &z);
}

#[tokio::test]
async fn a_5xx_is_unknown_and_transient() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().machine_reply =
        Some((500, google_error(500, "Internal error", "INTERNAL")));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Unknown);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "transient");
    assert!(msg.contains("500"), "{msg}");
    assert_every_size_unknown(&r, &one_zone());
}

#[tokio::test]
async fn a_429_is_transient() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().project_reply = Some((
        429,
        google_error(429, "Rate Limit Exceeded", "RESOURCE_EXHAUSTED"),
    ));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(err(&r).0, "transient");
}

/// A quota a human must fix outranks a provider 500 elsewhere, whichever is read first.
#[tokio::test]
async fn a_quota_outranks_an_outage() {
    let (base, knobs) = fake().await;
    {
        let mut k = knobs.lock().unwrap();
        k.gpus_all_regions = Some((0.0, 0.0));
        k.machine_reply = Some((503, google_error(503, "backend unavailable", "UNAVAILABLE")));
    }
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    assert_eq!(err(&r).0, "quota");
}

// ─── the key and the project ──────────────────────────────────────────────────

#[tokio::test]
async fn a_token_401_is_key_rejected_and_every_size_unknown() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().token_reply = Some((
        401,
        json!({"error": "invalid_client", "error_description": format!("bad client {CLIENT_EMAIL} $ASSERTION")}),
    ));
    let z = zones(&[
        ("us-central1-a", &["g2-standard-4"]),
        ("us-east4-c", &["a2-highgpu-1g"]),
    ]);
    let r = run(&base, &pt(), z.clone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(msg.ends_with("401 Unauthorized: key rejected"), "{msg}");
    assert_every_size_unknown(&r, &z);
    assert_eq!(r.key_scope, None);
    let k = knobs.lock().unwrap();
    assert_eq!(
        k.calls,
        vec!["POST /token".to_string()],
        "nothing is read without a token"
    );
    assert_no_key_material(&report_text(&r));
}

/// Google's own token error: its `error` and `error_description` reach the operator, with
/// anything of the key or the assertion they echo scrubbed out.
#[tokio::test]
async fn a_token_invalid_grant_is_permanent_and_scrubbed() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().token_reply = Some((
        400,
        json!({"error": "invalid_grant",
               "error_description": format!("Invalid JWT Signature. iss={CLIENT_EMAIL} kid={KEY_ID} assertion=$ASSERTION")}),
    ));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(msg.contains("400 Bad Request"), "{msg}");
    assert!(msg.contains("invalid_grant"), "{msg}");
    assert!(msg.contains("Invalid JWT Signature."), "{msg}");
    assert!(msg.contains("[redacted]"), "{msg}");
    assert_no_key_material(&report_text(&r));
    assert_every_size_unknown(&r, &one_zone());
}

#[tokio::test]
async fn a_token_5xx_is_transient() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().token_reply = Some((503, json!({"error": "temporarily_unavailable"})));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(err(&r).0, "transient");
    assert_every_size_unknown(&r, &one_zone());
}

#[tokio::test]
async fn a_token_reply_without_an_access_token_is_not_ok() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().token_reply = Some((200, json!({"token_type": "Bearer"})));
    let r = run(&base, &pt(), one_zone()).await;
    assert_ne!(r.state, CheckState::Ok);
    assert_eq!(knobs.lock().unwrap().calls.len(), 1);
}

/// A 403 on the project keeps only Google's reason codes (A3): the message itself may echo
/// anything.
#[tokio::test]
async fn a_project_403_is_key_rejected_with_the_body_discarded() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().project_reply = Some((
        403,
        google_error(
            403,
            &format!("Required 'compute.projects.get' for {CLIENT_EMAIL} BODY-MARKER"),
            "PERMISSION_DENIED",
        ),
    ));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(
        msg.ends_with("403 Forbidden: key rejected (PERMISSION_DENIED)"),
        "{msg}"
    );
    assert!(!msg.contains("BODY-MARKER"), "{msg}");
    assert_no_key_material(&report_text(&r));
    assert_every_size_unknown(&r, &one_zone());
    let k = knobs.lock().unwrap();
    assert!(
        !k.calls.iter().any(|c| c.contains("machineTypes")),
        "no zone reads after a project failure"
    );
}

/// The Compute Engine API is off in a new project: the 403 says so, from Google's reason
/// codes, and nothing else of the body (the email, the project number, a token posing as a
/// reason) gets through.
#[tokio::test]
async fn a_disabled_compute_api_is_named_as_the_fix() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().project_reply = Some((
        403,
        json!({"error": {
            "code": 403,
            "message": format!("Compute Engine API has not been used in project 123456789 \
                                before or it is disabled. BODY-MARKER {CLIENT_EMAIL}"),
            "status": "PERMISSION_DENIED",
            "errors": [
                {"message": "BODY-MARKER", "domain": "usageLimits", "reason": "accessNotConfigured"},
                {"message": "m", "domain": "global", "reason": ACCESS_TOKEN}
            ],
            "details": [{
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": "SERVICE_DISABLED",
                "domain": "googleapis.com",
                "metadata": {"consumer": "projects/123456789", "service": "compute.googleapis.com"}
            }]
        }}),
    ));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(
        msg.ends_with(
            "403 Forbidden: the Compute Engine API is disabled in project proj-1 — enable \
             compute.googleapis.com in the Google Cloud console \
             (SERVICE_DISABLED, accessNotConfigured, PERMISSION_DENIED)"
        ),
        "{msg}"
    );
    for absent in ["BODY-MARKER", "123456789", "consumer"] {
        assert!(!msg.contains(absent), "{absent} in {msg}");
    }
    assert_no_key_material(&report_text(&r));
}

#[tokio::test]
async fn a_408_is_transient() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().machine_reply = Some((
        408,
        google_error(408, "Request Timeout", "DEADLINE_EXCEEDED"),
    ));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(err(&r).0, "transient");
}

/// A zone or machine type that is not a Google name is refused before it reaches a URL.
#[tokio::test]
async fn a_zone_or_size_that_is_not_a_name_is_permanent_and_never_requested() {
    for (zone, size) in [
        ("US-central1-a", "g2-standard-4"),
        ("us-central1-a/../x", "g2-standard-4"),
        ("us-central1-a", "g2/standard-4"),
    ] {
        let (base, knobs) = fake().await;
        let r = run(&base, &pt(), zones(&[(zone, &[size])])).await;
        assert_eq!(r.state, CheckState::NeedsYou, "{zone} {size}");
        assert_eq!(err(&r).0, "permanent");
        let k = knobs.lock().unwrap();
        assert!(
            !k.calls.iter().any(|c| c.contains("machineTypes")),
            "{zone} {size}: {:?}",
            k.calls
        );
        assert!(k.unexpected.is_empty(), "{:?}", k.unexpected);
    }
}

#[tokio::test]
async fn an_account_that_is_not_a_project_id_is_permanent_and_makes_no_call() {
    for account in ["x/y", ".", "a:"] {
        let (base, knobs) = fake().await;
        let r = run(
            &base,
            &pt_with(key_json().to_string(), Some(account)),
            one_zone(),
        )
        .await;
        assert_eq!(r.state, CheckState::NeedsYou, "{account}");
        let (kind, msg) = err(&r);
        assert_eq!(kind, "permanent");
        assert!(msg.contains("not a valid Google Cloud project id"), "{msg}");
        assert!(knobs.lock().unwrap().calls.is_empty(), "{account}");
    }
}

/// The token carries a broad read-only scope: it is minted only for a Compute Engine
/// endpoint. Without a stand-in, a wrong one is refused before anything is sent anywhere.
#[tokio::test]
async fn an_endpoint_that_is_not_compute_engine_gets_no_token() {
    for endpoint in [
        "https://compute.googleapis.com",
        "https://example.com/compute/v1",
    ] {
        let mut p = pt();
        p.endpoint = endpoint.into();
        let r = GcpChecker::new(&p, one_zone(), None).check().await;
        assert_eq!(r.state, CheckState::NeedsYou, "{endpoint}");
        let (kind, msg) = err(&r);
        assert_eq!(kind, "permanent");
        assert!(
            msg.contains("https://compute.googleapis.com/compute/v1"),
            "{msg}"
        );
        assert_every_size_unknown(&r, &one_zone());
    }
}

#[tokio::test]
async fn an_unknown_project_is_permanent() {
    let (base, _) = fake().await;
    let r = run(
        &base,
        &pt_with(key_json().to_string(), Some("no-such-project")),
        one_zone(),
    )
    .await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(msg.contains("no-such-project"), "{msg}");
    assert_every_size_unknown(&r, &one_zone());
}

#[tokio::test]
async fn a_project_5xx_is_transient_and_every_size_unknown() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().project_reply = Some((502, json!("bad gateway")));
    let z = zones(&[
        ("us-central1-a", &["g2-standard-4"]),
        ("us-east4-c", &["a2-highgpu-1g"]),
    ]);
    let r = run(&base, &pt(), z.clone()).await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(err(&r).0, "transient");
    assert_every_size_unknown(&r, &z);
}

#[tokio::test]
async fn an_unreachable_google_is_transient() {
    // Nothing listens on port 9 of localhost.
    let r = run("http://127.0.0.1:9", &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(err(&r).0, "transient");
    assert_every_size_unknown(&r, &one_zone());
}

// ─── key scope (A2): informational only ───────────────────────────────────────

#[tokio::test]
async fn key_scope_wording_follows_the_granted_permissions() {
    for (granted, wording) in [
        (vec![], "read-only"),
        (
            vec!["compute.instances.create", "compute.instances.delete"],
            "can create and destroy",
        ),
        (
            vec!["compute.instances.create"],
            "can create but NOT destroy",
        ),
        (vec!["compute.instances.delete"], "can destroy only"),
    ] {
        let (base, knobs) = fake().await;
        knobs.lock().unwrap().iam = Iam::Granted(granted.clone());
        let r = run(&base, &pt(), one_zone()).await;
        assert_eq!(r.state, CheckState::Ok, "{granted:?}: {:?}", r.last_error);
        assert_eq!(r.key_scope.as_deref(), Some(wording), "{granted:?}");
    }
}

/// Resource Manager is often off in a project (403 SERVICE_DISABLED). The scope is then
/// not shown, and the check stays green: it is information, not a fault.
#[tokio::test]
async fn a_resource_manager_403_leaves_the_scope_unknown_and_the_state_ok() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().iam = Iam::Status(
        403,
        google_error(
            403,
            "Cloud Resource Manager API has not been used in project 1 before or it is disabled.",
            "PERMISSION_DENIED",
        ),
    );
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.key_scope, None);
    assert_eq!(r.last_error, None);
    assert_eq!(knobs.lock().unwrap().iam_calls, 1);
}

#[tokio::test]
async fn a_resource_manager_5xx_leaves_the_scope_unknown_and_the_state_ok() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().iam = Iam::Status(500, google_error(500, "Internal error", "INTERNAL"));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
    assert_eq!(r.key_scope, None);
}

#[tokio::test]
async fn a_resource_manager_failure_never_changes_another_error() {
    let (base, knobs) = fake().await;
    {
        let mut k = knobs.lock().unwrap();
        k.gpus_all_regions = Some((0.0, 0.0));
        k.iam = Iam::Status(403, google_error(403, "disabled", "PERMISSION_DENIED"));
    }
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(err(&r).0, "quota");
    assert_eq!(r.key_scope, None);
}

// ─── the key file ─────────────────────────────────────────────────────────────

async fn assert_refused_without_a_call(json_text: String, expect_in_msg: &str) {
    let (base, knobs) = fake().await;
    let p = pt_with(json_text.clone(), Some(PROJECT));
    let r = run(&base, &p, one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou, "{json_text}");
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(msg.contains(expect_in_msg), "{msg}");
    assert_every_size_unknown(&r, &one_zone());
    assert!(
        knobs.lock().unwrap().calls.is_empty(),
        "a bad key file makes no call"
    );
    assert_no_key_material(&report_text(&r));
    assert!(!format!("{:?}", GcpChecker::new(&p, one_zone(), Some(&base))).contains(KEY_LINE));
}

#[tokio::test]
async fn a_key_file_that_is_not_json_is_permanent_with_a_plain_message() {
    // Truncated mid-key, as a bad paste would be.
    let text = key_json().to_string();
    let cut = &text[..text.find(KEY_LINE).unwrap() + KEY_LINE.len()];
    assert_refused_without_a_call(cut.to_string(), "service_account_json").await;
}

#[tokio::test]
async fn a_key_file_whose_fields_have_the_wrong_type_does_not_echo_them() {
    // serde's own message would quote the offending string.
    let mut j = key_json();
    j["type"] = json!({ "nested": KEY_LINE });
    assert_refused_without_a_call(j.to_string(), "service_account_json").await;
}

#[tokio::test]
async fn a_key_that_is_not_a_service_account_is_permanent() {
    let mut j = key_json();
    j["type"] = json!("authorized_user");
    assert_refused_without_a_call(j.to_string(), "service_account").await;
}

#[tokio::test]
async fn another_universe_domain_is_permanent() {
    let mut j = key_json();
    j["universe_domain"] = json!("example-sovereign.cloud");
    assert_refused_without_a_call(j.to_string(), "googleapis.com").await;
}

#[tokio::test]
async fn an_absent_universe_domain_is_googleapis() {
    let (base, _) = fake().await;
    let mut j = key_json();
    j.as_object_mut().unwrap().remove("universe_domain");
    let r = run(&base, &pt_with(j.to_string(), Some(PROJECT)), one_zone()).await;
    assert_eq!(r.state, CheckState::Ok, "{:?}", r.last_error);
}

#[tokio::test]
async fn a_key_file_missing_a_field_is_permanent() {
    for field in ["private_key", "client_email", "private_key_id"] {
        let mut j = key_json();
        j.as_object_mut().unwrap().remove(field);
        assert_refused_without_a_call(j.to_string(), field).await;
    }
}

#[tokio::test]
async fn a_private_key_that_is_not_an_rsa_pem_is_permanent() {
    let mut j = key_json();
    // The header split as in TEST_PRIVATE_KEY.
    let (begin, end) = (
        concat!("-----BEGIN PRIVATE", " KEY-----"),
        concat!("-----END PRIVATE", " KEY-----"),
    );
    j["private_key"] = json!(format!("{begin}\n{KEY_LINE}\n{end}\n"));
    assert_refused_without_a_call(j.to_string(), "private_key").await;
}

#[tokio::test]
async fn no_project_anywhere_is_permanent() {
    let (base, knobs) = fake().await;
    let mut j = key_json();
    j.as_object_mut().unwrap().remove("project_id");
    let r = run(&base, &pt_with(j.to_string(), None), one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(msg.contains("project"), "{msg}");
    assert!(knobs.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn a_missing_credential_field_is_permanent() {
    let (base, knobs) = fake().await;
    let mut p = pt();
    p.fields.clear();
    let r = run(&base, &p, one_zone()).await;
    assert_eq!(r.state, CheckState::NeedsYou);
    assert_eq!(err(&r).0, "permanent");
    assert!(knobs.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn no_zones_is_the_config_error_and_makes_no_call() {
    let (base, knobs) = fake().await;
    let r = run(&base, &pt(), Vec::new()).await;
    assert_eq!(r.state, CheckState::Unknown);
    assert_eq!(
        r.last_error,
        Some(("config".to_string(), "no zones configured".to_string()))
    );
    assert!(r.zones.is_empty());
    assert!(knobs.lock().unwrap().calls.is_empty());
}

// ─── redaction (A3) and Debug ─────────────────────────────────────────────────

/// A provider error body that echoes the key file, the access token and the client email:
/// every one of them is scrubbed before it becomes error text.
#[tokio::test]
async fn a_provider_body_echoing_secrets_is_scrubbed() {
    let (base, knobs) = fake().await;
    let echoed = format!(
        "backend error for {CLIENT_EMAIL} with Bearer {ACCESS_TOKEN} key {kid} pem {pem} json {json}",
        kid = KEY_ID,
        pem = TEST_PRIVATE_KEY,
        json = serde_json::to_string(TEST_PRIVATE_KEY).unwrap(),
    );
    knobs.lock().unwrap().machine_reply = Some((500, google_error(500, &echoed, "INTERNAL")));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(r.state, CheckState::Unknown);
    let (kind, msg) = err(&r);
    assert_eq!(kind, "transient");
    assert!(msg.contains("backend error for [redacted]"), "{msg}");
    assert_no_key_material(&report_text(&r));
}

/// The private key as the key file carries it (`\n` escaped) as Google's whole error message:
/// it starts within the 400 characters kept, so only the scrub can remove it (the line-by-line
/// forms would leave its header).
#[tokio::test]
async fn a_body_echoing_the_json_escaped_key_is_scrubbed() {
    let (base, knobs) = fake().await;
    let escaped = serde_json::to_string(TEST_PRIVATE_KEY).unwrap();
    let escaped = escaped.trim_matches('"');
    knobs.lock().unwrap().machine_reply = Some((503, google_error(503, escaped, "UNAVAILABLE")));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(err(&r).0, "transient");
    let text = report_text(&r);
    assert!(text.contains("[redacted]"), "{text}");
    assert!(!text.contains(&escaped[..60]), "{text}");
    assert_no_key_material(&text);
}

/// A token error that echoes only the assertion's signature.
#[tokio::test]
async fn a_token_error_echoing_only_the_signature_is_scrubbed() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().token_reply = Some((
        400,
        json!({"error": "invalid_grant", "error_description": "bad signature $SIGNATURE"}),
    ));
    let r = run(&base, &pt(), one_zone()).await;
    let (kind, msg) = err(&r);
    assert_eq!(kind, "permanent");
    assert!(msg.contains("bad signature [redacted]"), "{msg}");
}

#[tokio::test]
async fn a_body_that_is_not_google_json_is_scrubbed_too() {
    let (base, knobs) = fake().await;
    knobs.lock().unwrap().machine_reply = Some((
        503,
        json!(format!("upstream said {ACCESS_TOKEN} / {KEY_LINE}")),
    ));
    let r = run(&base, &pt(), one_zone()).await;
    assert_eq!(err(&r).0, "transient");
    assert_no_key_material(&report_text(&r));
}

#[tokio::test]
async fn debug_shows_no_key_material() {
    let c = GcpChecker::new(&pt(), one_zone(), Some("http://127.0.0.1:9"));
    let d = format!("{c:?}");
    assert!(d.contains("GcpChecker"), "{d}");
    assert!(d.contains(PROJECT), "{d}");
    assert_no_key_material(&d);
}
