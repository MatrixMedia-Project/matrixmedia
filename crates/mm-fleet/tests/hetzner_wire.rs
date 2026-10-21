//! `HetznerProvider` against a stand-in Hetzner Cloud API v1 serving the **real
//! response shapes** from the published reference
//! (`https://docs.hetzner.cloud/reference/cloud`): `POST /servers` answering 201 with
//! the server, `GET /servers/{id}` for the delete settle, `DELETE /servers/{id}`
//! answering 200 with the action, and `GET /servers?label_selector=...` paged.
//!
//! These exist because every wire type here is `Deserialize` with `#[serde(default)]`,
//! which is the right choice for an API that adds fields — but it means a shape
//! *disagreement* yields a **default**, not an error. A `labels` map that stopped
//! parsing would become an empty map, `list` would report nothing, and the orphan
//! sweeper would pass a whole project of orphans as clean. So the shapes are pinned.
//!
//! What these do NOT prove: that the real Hetzner behaves as its docs describe.
//! That claim transfers only against a real account.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use mm_core::fleet::{NodeFlavor, NodeId};
use mm_fleet::hetzner::HetznerProvider;
use mm_fleet::provider::{InstanceSpec, Provider, ProviderError};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Every request the stand-in received, so the tests can assert on what was sent
/// rather than only on what came back.
#[derive(Default)]
struct Seen {
    created: Vec<Value>,
    deletes: Vec<String>,
    gets: Vec<String>,
    list_queries: Vec<String>,
    label_selectors: Vec<Option<String>>,
    /// Fail the create call with this status and error body.
    create_failure: Option<(u16, Value)>,
    /// Answer the create with 201 and exactly this body, to model a 2xx whose body is not
    /// a server. The server is made all the same, as far as the caller can know.
    create_body: Option<String>,
    /// GETs that still find a deleted server before it reads gone.
    gets_before_gone: usize,
    /// A delete that never settles: GET keeps finding the server.
    delete_never_settles: bool,
    /// Servers the fake holds: (id, location, labels).
    servers: Vec<(u64, String, Value)>,
    next_id: u64,
    deleted: Vec<u64>,
}

type Shared = Arc<Mutex<Seen>>;

fn server_json(id: u64, location: &str, labels: &Value) -> Value {
    json!({
        "id": id, "name": "bc-b1-fanout-0", "status": "running",
        "created": "2026-10-20T12:00:00Z",
        "public_net": {"ipv4": {"ip": "203.0.113.7", "blocked": false, "dns_ptr": ""},
                       "ipv6": {"ip": "2a01:4f8::1/64", "blocked": false, "dns_ptr": ""},
                       "floating_ips": []},
        "server_type": {"id": 22, "name": "cx22", "cores": 2, "memory": 4},
        "datacenter": {"id": 1, "name": "fsn1-dc14",
                       "location": {"id": 1, "name": location, "country": "DE"}},
        "image": {"id": 1, "name": "ubuntu-24.04"},
        "labels": labels,
        "future_field": {"nested": [1, 2]},
    })
}

async fn create(State(s): State<Shared>, Json(body): Json<Value>) -> axum::response::Response {
    let mut seen = s.lock().unwrap();
    if let Some((status, body)) = seen.create_failure.clone() {
        return (StatusCode::from_u16(status).unwrap(), body.to_string()).into_response();
    }
    if let Some(text) = seen.create_body.clone() {
        return (StatusCode::CREATED, text).into_response();
    }
    seen.created.push(body.clone());
    seen.next_id += 1;
    let id = seen.next_id;
    let labels = body.get("labels").cloned().unwrap_or(json!({}));
    seen.servers.push((id, "fsn1".to_string(), labels.clone()));
    (
        StatusCode::CREATED,
        json!({"server": server_json(id, "fsn1", &labels), "action": {"id": 1}}).to_string(),
    )
        .into_response()
}

async fn get_server(State(s): State<Shared>, Path(id): Path<u64>) -> axum::response::Response {
    let mut seen = s.lock().unwrap();
    seen.gets.push(id.to_string());
    if seen.deleted.contains(&id) {
        if seen.delete_never_settles {
            // The delete was accepted but never completes.
        } else if seen.gets_before_gone > 0 {
            seen.gets_before_gone -= 1;
        } else {
            return (
                StatusCode::NOT_FOUND,
                json!({"error": {"code": "not_found", "message": "server not found"}}).to_string(),
            )
                .into_response();
        }
    }
    match seen.servers.iter().find(|(i, _, _)| *i == id) {
        Some((_, location, labels)) => {
            axum::Json(json!({"server": server_json(id, location, labels)})).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            json!({"error": {"code": "not_found", "message": "server not found"}}).to_string(),
        )
            .into_response(),
    }
}

async fn delete_server(State(s): State<Shared>, Path(id): Path<u64>) -> axum::response::Response {
    let mut seen = s.lock().unwrap();
    seen.deletes.push(id.to_string());
    if seen.servers.iter().any(|(i, _, _)| *i == id) {
        seen.deleted.push(id);
        (StatusCode::OK, json!({"action": {"id": 2}}).to_string()).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            json!({"error": {"code": "not_found", "message": "server not found"}}).to_string(),
        )
            .into_response()
    }
}

async fn list_servers(State(s): State<Shared>, uri: axum::http::Uri) -> axum::response::Response {
    let mut seen = s.lock().unwrap();
    let query = uri.query().unwrap_or("").to_string();
    seen.list_queries.push(query.clone());
    let selector = query.split('&').find_map(|p| {
        p.strip_prefix("label_selector=")
            .map(|v| v.replace("%3D%3D", "==").replace("%3D", "="))
    });
    seen.label_selectors.push(selector.clone());
    let tag_only = selector
        .as_deref()
        .is_some_and(|v| v == "mm-fleet-api==true");
    let servers: Vec<Value> = seen
        .servers
        .iter()
        .filter(|(id, _, _)| !seen.deleted.contains(id))
        .filter(|(_, _, labels)| {
            if !tag_only {
                return true;
            }
            labels.get("mm-fleet-api").and_then(|v| v.as_str()) == Some("true")
        })
        .map(|(id, location, labels)| server_json(*id, location, labels))
        .collect();
    let total = servers.len();
    // The stand-in honors the fleet-tag half of the selector, the way the real API does;
    // the client enforces the location half itself.
    axum::Json(json!({
        "servers": servers,
        "meta": {"pagination": {"page": 1, "per_page": 50, "total_entries": total}}
    }))
    .into_response()
}

async fn fake_with(seen: Shared) -> String {
    let app = Router::new()
        .route("/servers", post(create).get(list_servers))
        .route("/servers/{id}", get(get_server).delete(delete_server))
        .with_state(seen);
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

fn provider(seen: &Shared, base: &str) -> HetznerProvider {
    let _ = seen;
    HetznerProvider::new("test-token", "fsn1", "ubuntu-24.04", "true").with_base_url(base)
}

fn spec(id: &str) -> InstanceSpec {
    InstanceSpec {
        mm_node_id: NodeId::new(id),
        flavor: NodeFlavor::Fanout,
        region: "fsn1".into(),
        size: "cx22".into(),
        user_data: "#cloud-config\n".into(),
    }
}

#[tokio::test]
async fn create_sends_the_documented_body_and_returns_the_handle() {
    let seen: Shared = Arc::default();
    let base = fake_with(seen.clone()).await;
    let h = provider(&seen, &base)
        .create(&spec("bc-b1-fanout-0"))
        .await
        .unwrap();
    assert_eq!(h.provider_id, "1");
    assert_eq!(h.public_ip.as_deref(), Some("203.0.113.7"));
    assert!(h.created_at.is_some(), "the sweeper's grace needs the age");
    let body = &seen.lock().unwrap().created[0];
    assert_eq!(body["name"], "bc-b1-fanout-0");
    assert_eq!(body["server_type"], "cx22");
    assert_eq!(body["image"], "ubuntu-24.04");
    assert_eq!(body["location"], "fsn1");
    assert_eq!(body["labels"]["mm-fleet-api"], "true");
    assert_eq!(body["labels"]["mm-node-id"], "bc-b1-fanout-0");
    assert_eq!(body["user_data"], "#cloud-config\n");
    // A fan-out box without a public address serves no viewers: both families are
    // asked for explicitly, never left to the default.
    assert_eq!(
        body["public_net"],
        json!({"enable_ipv4": true, "enable_ipv6": true})
    );
}

#[tokio::test]
async fn create_refuses_a_transcode_node_and_a_bad_name_before_any_call() {
    let seen: Shared = Arc::default();
    let base = fake_with(seen.clone()).await;
    let p = provider(&seen, &base);
    // A transcode node needs a GPU the Hetzner catalogue does not have.
    let mut transcode = spec("bc-b1-transcode-0");
    transcode.flavor = NodeFlavor::Transcode;
    let err = p.create(&transcode).await.expect_err("must refuse");
    assert!(err.needs_human(), "{err:?}");
    assert!(err.to_string().contains("fan-out"), "{err}");
    // A name the API would refuse is refused first: a Transient here would trigger a
    // lookup for a machine that cannot exist.
    let err = p
        .create(&spec("NOT A NAME"))
        .await
        .expect_err("must refuse");
    assert!(err.needs_human(), "{err:?}");
    assert_eq!(seen.lock().unwrap().created.len(), 0, "nothing was sent");
}

#[tokio::test]
async fn create_maps_a_stock_out_to_capacity_and_a_limit_to_quota() {
    for (code, is_capacity, is_quota) in [
        ("resource_unavailable", true, false),
        ("resource_limit_exceeded", false, true),
    ] {
        let seen: Shared = Arc::default();
        let base = fake_with(seen.clone()).await;
        seen.lock().unwrap().create_failure =
            Some((422, json!({"error": {"code": code, "message": "nope"}})));
        let err = provider(&seen, &base)
            .create(&spec("bc-b1-fanout-0"))
            .await
            .expect_err("must fail");
        assert_eq!(err.is_capacity(), is_capacity, "{code}: {err:?}");
        assert_eq!(err.is_quota(), is_quota, "{code}: {err:?}");
        assert!(
            !err.is_transient(),
            "{code}: retrying the same zone is the trap"
        );
        assert!(
            !err.to_string().contains("nope"),
            "the body is never quoted: {err}"
        );
    }
}

#[tokio::test]
async fn a_201_with_an_unreadable_body_is_transient_so_the_lookup_runs() {
    let seen: Shared = Arc::default();
    let base = fake_with(seen.clone()).await;
    seen.lock().unwrap().create_body = Some("not a server".to_string());
    let err = provider(&seen, &base)
        .create(&spec("bc-b1-fanout-0"))
        .await
        .expect_err("must fail");
    // A 201 means the server was made: Permanent would skip the lookup for it.
    assert!(err.is_transient(), "{err:?}");
}

#[tokio::test]
async fn destroy_deletes_and_returns_ok_once_a_get_says_gone() {
    let seen: Shared = Arc::default();
    let base = fake_with(seen.clone()).await;
    let p = provider(&seen, &base);
    let h = p.create(&spec("bc-b1-fanout-0")).await.unwrap();
    p.destroy(&h.provider_id).await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.deletes, vec!["1".to_string()]);
    assert!(
        !seen.gets.is_empty(),
        "the delete is confirmed, not assumed"
    );
}

#[tokio::test]
async fn destroying_an_unknown_server_is_success_not_failure() {
    let seen: Shared = Arc::default();
    let base = fake_with(seen.clone()).await;
    provider(&seen, &base)
        .destroy("999")
        .await
        .expect("destroy must be idempotent");
}

#[tokio::test]
async fn destroy_waits_out_a_settling_delete_and_gives_up_loudly() {
    let seen: Shared = Arc::default();
    let base = fake_with(seen.clone()).await;
    let p = provider(&seen, &base).with_settle(std::time::Duration::from_millis(1), 60);
    let h = p.create(&spec("bc-b1-fanout-0")).await.unwrap();
    // The delete is accepted but the server reads back twice before it is gone.
    seen.lock().unwrap().gets_before_gone = 2;
    p.destroy(&h.provider_id).await.unwrap();
    assert!(seen.lock().unwrap().gets.len() >= 3, "polled until gone");

    // A delete that never settles is a Transient — retried, and never a silent Ok that
    // would mark the node gone behind a live machine.
    let seen: Shared = Arc::default();
    let base = fake_with(seen.clone()).await;
    let p = provider(&seen, &base).with_settle(std::time::Duration::from_millis(1), 3);
    let h = p.create(&spec("bc-b1-fanout-0")).await.unwrap();
    seen.lock().unwrap().delete_never_settles = true;
    let err = p.destroy(&h.provider_id).await.expect_err("must fail");
    assert!(err.is_transient(), "{err:?}");
    assert!(err.to_string().contains('1'), "{err}");
}

#[tokio::test]
async fn list_reports_only_our_tag_in_our_location_with_ip_and_age() {
    let seen: Shared = Arc::default();
    {
        let mut s = seen.lock().unwrap();
        s.servers
            .push((1, "fsn1".into(), json!({"mm-fleet-api": "true"})));
        s.servers
            .push((2, "nbg1".into(), json!({"mm-fleet-api": "true"})));
        s.servers.push((3, "fsn1".into(), json!({"other": "true"})));
        s.deleted.push(2);
    }
    let base = fake_with(seen.clone()).await;
    let listed = provider(&seen, &base).list().await.unwrap();
    // Ours in fsn1 only: not nbg1's (another zone's client lists it), not someone else's.
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provider_id, "1");
    assert_eq!(listed[0].public_ip.as_deref(), Some("203.0.113.7"));
    assert!(listed[0].created_at.is_some());
    let selectors = &seen.lock().unwrap().label_selectors;
    assert!(
        selectors
            .iter()
            .any(|q| q.as_deref() == Some("mm-fleet-api==true")),
        "{selectors:?}"
    );
}

#[tokio::test]
async fn find_returns_the_oldest_match_for_the_node_label_and_none_when_absent() {
    let seen: Shared = Arc::default();
    {
        let mut s = seen.lock().unwrap();
        s.servers.push((
            7,
            "fsn1".into(),
            json!({"mm-fleet-api": "true", "mm-node-id": "bc-b1-fanout-0"}),
        ));
        s.servers.push((
            9,
            "fsn1".into(),
            json!({"mm-fleet-api": "true", "mm-node-id": "bc-b1-fanout-9"}),
        ));
    }
    let base = fake_with(seen.clone()).await;
    let p = provider(&seen, &base);
    let found = p.find(&NodeId::new("bc-b1-fanout-0")).await.unwrap();
    // The oldest match: the caller records this handle and the orphan sweep reaps the rest.
    assert_eq!(found.map(|h| h.provider_id), Some("7".to_string()));
    assert_eq!(
        p.find(&NodeId::new("bc-b1-fanout-missing")).await.unwrap(),
        None,
        "no such machine is Ok(None) — never an error the sweeper would treat as empty"
    );
    let err = p
        .find(&NodeId::new("NOT A NAME"))
        .await
        .expect_err("must refuse");
    assert!(err.needs_human(), "{err:?}");
}

#[tokio::test]
async fn error_bodies_echoing_the_request_carry_no_secret_out() {
    let seen: Shared = Arc::default();
    let base = fake_with(seen.clone()).await;
    seen.lock().unwrap().create_failure = Some((
        422,
        json!({"error": {"code": "invalid_input",
                         "message": "user_data contained SECRET-MARKER"}}),
    ));
    let err = provider(&seen, &base)
        .create(&spec("bc-b1-fanout-0"))
        .await
        .expect_err("must fail");
    let shown = err.to_string();
    assert!(!shown.contains("SECRET-MARKER"), "{shown}");
    assert!(shown.contains("invalid_input"), "{shown}");
    assert!(matches!(err, ProviderError::Permanent(_)));
}
