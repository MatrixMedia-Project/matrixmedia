//! `RunpodProvider` against a stand-in RunPod serving the **real response shapes**,
//! taken from the published REST v2 OpenAPI (the docs pages `runpod.rs` links).
//!
//! A shape disagreement here must surface as an error, never a default: every wire type the
//! provider parses with `#[serde(default)]` would otherwise turn a renamed field into an
//! empty map, and `list` would report no pods — which the orphan sweeper reads as "every
//! instance we know about is an orphan".
//!
//! What these do NOT prove: that the real RunPod behaves as its docs describe.
//! That claim transfers only against a real account.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use mm_core::fleet::{NodeFlavor, NodeId};
use mm_fleet::provider::{InstanceSpec, Provider};
use mm_fleet::runpod::{NODE_ID_ENV, RunpodProvider, USER_DATA_ENV};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const KEY: &str = "RP-TEST-SECRET";
/// A stand-in for a test boot's single-use probe token: 64 lowercase hex.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Every request the stand-in received, and how it should (mis)behave.
#[derive(Default)]
struct Seen {
    created: Vec<Value>,
    authed: Vec<bool>,
    deleted: Vec<String>,
    gets: Vec<String>,
    creates: u32,
    /// All pods the list serves: seeded ones plus every created one.
    pods: Vec<Value>,
    /// Ids a DELETE accepted: a later GET finds them only while they linger.
    gone: Vec<String>,
    /// GETs that still answer RUNNING for a gone pod before it disappears. `-1` is forever.
    linger: i64,
    /// Fail the create with this status and problem body.
    create_failure: Option<(u16, Value)>,
    /// Answer the create with 201 and exactly this body, to model a 2xx whose body is not
    /// a pod. The pod is made all the same, as far as the caller can know.
    create_body: Option<String>,
    /// Force a status on DELETE.
    delete_status: Option<u16>,
    /// DELETE answers 409 (a cluster member pod).
    conflict_delete: bool,
    /// Force a status on the list.
    list_status: Option<u16>,
    /// Answer the list with 200 and exactly this body (a 2xx that is not a pod list).
    list_body: Option<String>,
    /// Serve the list in two cursor pages instead of one.
    pages: Option<(Vec<Value>, Vec<Value>)>,
    /// Accept creates and lists and never answer them (a hung provider).
    hang: bool,
}

type Shared = Arc<Mutex<Seen>>;

/// One pod in the list/get shape, with fields the provider ignores (image, gpu, cloud)
/// so tolerance of additions is also pinned.
fn pod(id: &str, node: Option<&str>, status: &str, created: &str) -> Value {
    let mut env = serde_json::Map::new();
    if let Some(n) = node {
        env.insert(NODE_ID_ENV.to_string(), json!(n));
        env.insert(USER_DATA_ENV.to_string(), json!("#cloud-config\n"));
    }
    json!({
        "id": id,
        "name": node.unwrap_or("someone-elses-pod"),
        "image": "runpod/pytorch:1.0-test",
        "args": "",
        "disk": 50,
        "ports": ["8888/http", "22/tcp"],
        "env": env,
        "status": status,
        "gpu": {"id": "NVIDIA L4", "count": 1},
        "cloud": "SECURE",
        "dataCenterId": "EU-RO-1",
        "createdAt": created,
    })
}

fn problem(status: u16, detail: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({"title": "error", "status": status, "detail": detail})),
    )
}

fn is_authed(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {KEY}"))
}

async fn fake_runpod(state: Shared) -> String {
    let app = Router::new()
        .route(
            "/pods",
            get(
                |State(st): State<Shared>, headers: HeaderMap, RawQuery(query): RawQuery| async move {
                    let hang = {
                        let mut s = st.lock().unwrap();
                        s.authed.push(is_authed(&headers));
                        s.hang
                    };
                    if hang {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return (StatusCode::OK, Json(json!({"pods": []}))).into_response();
                    }
                    let s = st.lock().unwrap();
                    if let Some(status) = s.list_status {
                        let (code, body) = problem(status, "list refused");
                        return (code, body).into_response();
                    }
                    if let Some(raw) = s.list_body.clone() {
                        return (StatusCode::OK, raw).into_response();
                    }
                    let query = query.unwrap_or_default();
                    if let Some((first, second)) = s.pages.clone() {
                        let page = if query.contains("cursor=page2") {
                            (second, Value::Null, false)
                        } else {
                            (first, json!("page2"), true)
                        };
                        return Json(json!({
                            "pods": page.0,
                            "pagination": {"nextCursor": page.1, "hasNextPage": page.2},
                        }))
                        .into_response();
                    }
                    let pods = s.pods.clone();
                    Json(json!({
                        "pods": pods,
                        "pagination": {"nextCursor": null, "hasNextPage": false},
                    }))
                    .into_response()
                },
            )
            .post(
                |State(st): State<Shared>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    let authed = is_authed(&headers);
                    {
                        let mut s = st.lock().unwrap();
                        s.authed.push(authed);
                        s.created.push(body.clone());
                    }
                    let hang = st.lock().unwrap().hang;
                    if hang {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return (StatusCode::CREATED, Json(json!({}))).into_response();
                    }
                    let s = st.lock().unwrap();
                    if let Some((status, err)) = s.create_failure.clone() {
                        let code = StatusCode::from_u16(status).unwrap();
                        return (code, Json(err)).into_response();
                    }
                    if let Some(raw) = s.create_body.clone() {
                        return (StatusCode::CREATED, raw).into_response();
                    }
                    drop(s);
                    let mut s = st.lock().unwrap();
                    s.creates += 1;
                    let id = format!("pod-{}", s.creates);
                    let node = body
                        .get("name")
                        .and_then(|n| n.as_str())
                        .map(str::to_string);
                    let made = pod(&id, node.as_deref(), "PROVISIONING", "2026-06-01T12:00:00Z");
                    s.pods.push(made.clone());
                    (StatusCode::CREATED, Json(made)).into_response()
                },
            ),
        )
        .route(
            "/pods/{id}",
            get(
                |State(st): State<Shared>, Path(id): Path<String>| async move {
                    let mut s = st.lock().unwrap();
                    s.gets.push(id.clone());
                    if s.gone.contains(&id) {
                        if s.linger == -1 || s.linger > 0 {
                            if s.linger > 0 {
                                s.linger -= 1;
                            }
                            let running =
                                pod(&id, Some("tb-abc123"), "RUNNING", "2026-06-01T12:00:00Z");
                            return (StatusCode::OK, Json(running)).into_response();
                        }
                        let (code, body) = problem(404, "pod not found");
                        return (code, body).into_response();
                    }
                    match s.pods.iter().find(|p| p.get("id") == Some(&json!(id))) {
                        Some(found) => (StatusCode::OK, Json(found.clone())).into_response(),
                        None => {
                            let (code, body) = problem(404, "pod not found");
                            (code, body).into_response()
                        }
                    }
                },
            )
            .delete(
                |State(st): State<Shared>, Path(id): Path<String>| async move {
                    let mut s = st.lock().unwrap();
                    s.deleted.push(id.clone());
                    if let Some(status) = s.delete_status {
                        let (code, body) = problem(status, "delete refused");
                        return (code, body).into_response();
                    }
                    if s.conflict_delete {
                        let (code, body) = problem(409, "pod belongs to a cluster");
                        return (code, body).into_response();
                    }
                    if !s.pods.iter().any(|p| p.get("id") == Some(&json!(id))) {
                        let (code, body) = problem(404, "pod not found");
                        return (code, body).into_response();
                    }
                    if !s.gone.contains(&id) {
                        s.gone.push(id);
                    }
                    StatusCode::NO_CONTENT.into_response()
                },
            ),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

fn provider(base: &str) -> RunpodProvider {
    RunpodProvider::new(KEY, "eu-ro-1", "runpod/pytorch:1.0-test")
        .with_base_url(base)
        // Real waits are seconds apart; the stand-in needs none, only a bound.
        .with_settle(Duration::ZERO, 3)
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        mm_node_id: NodeId::new("tb-abc123"),
        flavor: NodeFlavor::Transcode,
        region: "eu-ro-1".into(),
        size: "NVIDIA L4".into(),
        user_data: format!("#cloud-config\nMM_REPORT_TOKEN={TOKEN}\n"),
    }
}

async fn stand_in() -> (String, Shared) {
    let state: Shared = Arc::new(Mutex::new(Seen::default()));
    let base = fake_runpod(state.clone()).await;
    (base, state)
}

async fn seeded(pods: Vec<Value>) -> (String, Shared) {
    let state: Shared = Arc::new(Mutex::new(Seen {
        pods,
        ..Default::default()
    }));
    let base = fake_runpod(state.clone()).await;
    (base, state)
}

// ─── create ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_names_the_node_pins_the_data_centre_and_marks_the_pod() {
    let (base, state) = stand_in().await;
    let h = provider(&base).create(&spec()).await.expect("create");
    assert_eq!(h.provider_id, "pod-1");
    assert!(h.public_ip.is_none(), "pods have no public IP in the API");
    assert_eq!(
        h.created_at.map(|t| t.to_rfc3339()),
        Some("2026-06-01T12:00:00+00:00".into())
    );

    let seen = state.lock().unwrap();
    assert_eq!(seen.authed, vec![true], "the key goes on the wire");
    assert_eq!(seen.created.len(), 1);
    let body = &seen.created[0];
    assert_eq!(body.get("name"), Some(&json!("tb-abc123")));
    assert_eq!(body.get("image"), Some(&json!("runpod/pytorch:1.0-test")));
    assert_eq!(body.get("gpu"), Some(&json!({"id": "NVIDIA L4", "count": 1})));
    assert_eq!(body.get("cloud"), Some(&json!("SECURE")));
    assert_eq!(body.get("dataCenterIds"), Some(&json!(["EU-RO-1"])));
    let env = body.get("env").expect("env carries the markers");
    assert_eq!(env.get(NODE_ID_ENV), Some(&json!("tb-abc123")));
    assert!(
        env.get(USER_DATA_ENV)
            .and_then(|u| u.as_str())
            .is_some_and(|u| u.contains(TOKEN)),
        "the user data goes to the container as {USER_DATA_ENV}: {env}"
    );
}

#[tokio::test]
async fn create_failures_sort_into_retry_elsewhere_or_human() {
    // (status, body, predicate on the error, fragment the message must hold)
    for (status, body, check, fragment) in [
        (401, json!({"detail": "unauthorized"}), "human", "key rejected"),
        (403, json!({"detail": "forbidden"}), "human", "permission"),
        (402, json!({"detail": "insufficient funds"}), "quota", "402"),
        (422, json!({"detail": "bad gpu id"}), "human", "422"),
        (
            400,
            json!({"detail": "this GPU/data center could not be placed"}),
            "capacity",
            "400",
        ),
        (400, json!({"detail": "name is required"}), "human", "400"),
        (429, json!({"detail": "slow down"}), "transient", "429"),
        (500, json!({"detail": "boom"}), "transient", "500"),
    ] {
        // Each case gets a fresh stand-in serving its failure.
        let state: Shared = Arc::new(Mutex::new(Seen {
            create_failure: Some((status, body)),
            ..Default::default()
        }));
        let base = fake_runpod(state.clone()).await;
        let err = provider(&base).create(&spec()).await.expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains(fragment), "{status}: {msg}");
        match check {
            "human" => assert!(err.needs_human(), "{status}: {msg}"),
            "quota" => assert!(err.is_quota(), "{status}: {msg}"),
            "capacity" => assert!(err.is_capacity(), "{status}: {msg}"),
            _ => assert!(err.is_transient(), "{status}: {msg}"),
        }
    }
}

#[tokio::test]
async fn a_create_error_never_carries_the_key_or_the_probe_token() {
    let token_body = json!({"detail": format!("echo {TOKEN} and {KEY}")});
    let state: Shared = Arc::new(Mutex::new(Seen {
        create_failure: Some((500, token_body)),
        ..Default::default()
    }));
    let base = fake_runpod(state.clone()).await;
    let err = provider(&base).create(&spec()).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(!msg.contains(KEY), "{msg}");
    assert!(!msg.contains(TOKEN), "{msg}");
    assert!(msg.contains("[redacted]"), "{msg}");
}

#[tokio::test]
async fn a_201_whose_body_is_not_a_pod_is_transient_not_a_handle() {
    for raw in [
        "this is not json".to_string(),
        // Valid JSON, but no pod: without an id there is no handle.
        json!({"status": "PROVISIONING"}).to_string(),
        json!({"id": ""}).to_string(),
    ] {
        let state: Shared = Arc::new(Mutex::new(Seen {
            create_body: Some(raw),
            ..Default::default()
        }));
        let base = fake_runpod(state.clone()).await;
        let err = provider(&base).create(&spec()).await.expect_err("must fail");
        assert!(
            err.is_transient() && !err.needs_human(),
            "the pod may exist, so the caller looks it up: {err}"
        );
    }
}

#[tokio::test]
async fn a_hung_create_is_a_timeout_the_caller_looks_up_first() {
    let state: Shared = Arc::new(Mutex::new(Seen {
        hang: true,
        ..Default::default()
    }));
    let base = fake_runpod(state.clone()).await;
    let p = provider(&base).with_request_timeout(Duration::from_millis(150));
    let err = p.create(&spec()).await.expect_err("must time out");
    assert!(
        err.is_timeout() && err.is_transient(),
        "a create that timed out may have made a pod: {err}"
    );
}

// ─── destroy ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn destroy_terminates_and_a_second_destroy_is_already_gone() {
    let (base, state) = stand_in().await;
    let p = provider(&base);
    let h = p.create(&spec()).await.expect("create");
    p.destroy(&h.provider_id).await.expect("destroy");
    // Idempotent as the trait requires: already gone is Ok, not an error.
    p.destroy(&h.provider_id).await.expect("second destroy");
    // Unknown ids are gone too.
    p.destroy("pod-never-existed").await.expect("unknown destroy");

    let seen = state.lock().unwrap();
    assert_eq!(seen.deleted, vec!["pod-1", "pod-1", "pod-never-existed"]);
}

#[tokio::test]
async fn destroy_of_a_cluster_member_needs_a_human() {
    let state: Shared = Arc::new(Mutex::new(Seen {
        pods: vec![pod("pod-9", Some("tb-abc123"), "RUNNING", "2026-06-01T12:00:00Z")],
        conflict_delete: true,
        ..Default::default()
    }));
    let base = fake_runpod(state.clone()).await;
    let err = provider(&base)
        .destroy("pod-9")
        .await
        .expect_err("must fail");
    assert!(err.needs_human(), "{err}");
    assert!(err.to_string().contains("cluster"), "{err}");
}

#[tokio::test]
async fn destroy_waits_for_a_lingering_pod_to_go() {
    let (base, state) = stand_in().await;
    state.lock().unwrap().linger = 2;
    let p = provider(&base);
    let h = p.create(&spec()).await.expect("create");
    p.destroy(&h.provider_id).await.expect("destroy");
    assert!(
        state.lock().unwrap().gets.len() >= 3,
        "the terminate was followed by polls, not one GET"
    );
}

#[tokio::test]
async fn destroy_of_a_pod_that_never_goes_is_transient_not_gone() {
    let (base, state) = stand_in().await;
    state.lock().unwrap().linger = -1;
    let p = provider(&base);
    let h = p.create(&spec()).await.expect("create");
    let err = p.destroy(&h.provider_id).await.expect_err("must fail");
    assert!(
        err.is_transient() && !err.needs_human(),
        "retried, never reported gone: {err}"
    );
}

// ─── list ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_only_the_pods_carrying_our_marker() {
    let (base, _) = seeded(vec![
        pod("pod-ours", Some("tb-abc123"), "RUNNING", "2026-06-01T12:00:00Z"),
        // Someone else's pod on the same account: no marker, never touched.
        pod("pod-theirs", None, "RUNNING", "2026-06-01T12:00:00Z"),
        // A marker with nothing in it is no marker.
        pod("pod-empty", Some("  "), "RUNNING", "2026-06-01T12:00:00Z"),
    ])
    .await;
    let listed = provider(&base).list().await.expect("list");
    assert_eq!(
        listed.iter().map(|h| h.provider_id.as_str()).collect::<Vec<_>>(),
        vec!["pod-ours"]
    );
}

#[tokio::test]
async fn a_list_that_is_not_a_pod_list_is_an_error_never_empty() {
    let state: Shared = Arc::new(Mutex::new(Seen {
        list_body: Some("[1, 2, 3]".into()),
        ..Default::default()
    }));
    let base = fake_runpod(state.clone()).await;
    let err = provider(&base).list().await.expect_err("must fail");
    assert!(
        err.is_transient(),
        "a shape disagreement is never an empty list: {err}"
    );
}

#[tokio::test]
async fn list_follows_the_cursor() {
    let state: Shared = Arc::new(Mutex::new(Seen {
        pages: Some((
            vec![pod("pod-a", Some("tb-a"), "RUNNING", "2026-06-01T12:00:00Z")],
            vec![pod("pod-b", Some("tb-b"), "RUNNING", "2026-06-01T12:00:00Z")],
        )),
        ..Default::default()
    }));
    let base = fake_runpod(state.clone()).await;
    let listed = provider(&base).list().await.expect("list");
    let mut ids: Vec<_> = listed.iter().map(|h| h.provider_id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["pod-a", "pod-b"]);
}

// ─── find ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn find_matches_the_marker_and_returns_the_oldest() {
    let mut seeds = vec![
        pod("pod-new", Some("tb-abc123"), "RUNNING", "2026-06-03T12:00:00Z"),
        pod("pod-old", Some("tb-abc123"), "RUNNING", "2026-06-01T12:00:00Z"),
        // Unknown age sorts last, even against a newer pod.
        pod("pod-ageless", Some("tb-abc123"), "RUNNING", "not-a-time"),
    ];
    // Our exact name, but another node's marker: the name alone must not match.
    let mut impostor = pod(
        "pod-impostor",
        Some("tb-other"),
        "RUNNING",
        "2026-05-01T12:00:00Z",
    );
    impostor["name"] = json!("tb-abc123");
    seeds.push(impostor);
    // No marker at all: never matched, whatever its name.
    let mut stray = pod("pod-stray", None, "RUNNING", "2026-05-01T12:00:00Z");
    stray["name"] = json!("tb-abc123");
    seeds.push(stray);
    let (base, _) = seeded(seeds).await;
    let found = provider(&base)
        .find(&NodeId::new("tb-abc123"))
        .await
        .expect("find")
        .expect("a match");
    assert_eq!(found.provider_id, "pod-old");

    assert!(
        provider(&base)
            .find(&NodeId::new("tb-nothing"))
            .await
            .expect("find")
            .is_none()
    );
}

// ─── names, redaction, plumbing ────────────────────────────────────────────────

#[tokio::test]
async fn provider_name_is_a_stable_label() {
    let (base, _) = stand_in().await;
    assert_eq!(provider(&base).name(), "runpod");
}

#[tokio::test]
async fn debug_never_prints_the_key() {
    let (base, _) = stand_in().await;
    let shown = format!("{:?}", provider(&base));
    assert!(!shown.contains(KEY), "{shown}");
    assert!(shown.contains("eu-ro-1"), "{shown}");
}

#[tokio::test]
async fn a_trailing_slash_on_the_base_never_doubles_in_the_url() {
    let (base, state) = stand_in().await;
    provider(&format!("{base}/"))
        .create(&spec())
        .await
        .expect("create");
    assert_eq!(state.lock().unwrap().created.len(), 1);
}
