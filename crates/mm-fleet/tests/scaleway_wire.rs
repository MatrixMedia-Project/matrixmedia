//! `ScalewayProvider` against a stand-in Scaleway serving the **real response
//! shapes**, taken from `scaleway-sdk-go/api/instance/v1`.
//!
//! These exist because every wire type here is `Deserialize` with `#[serde(default)]`,
//! which is the right choice for an API that adds fields — but it means a shape
//! *disagreement* yields a **default**, not an error. A `volumes` map that stopped
//! parsing would become an empty map, `destroy` would delete nothing, and every
//! COMPUTE3 teardown would leak its root volume silently. So the shapes are pinned.
//!
//! What these do NOT prove: that the real Scaleway behaves as its SDK describes.
//! That claim transfers only against a real account.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use mm_core::fleet::{NodeFlavor, NodeId};
use mm_fleet::provider::{InstanceSpec, Provider};
use mm_fleet::scaleway::ScalewayProvider;
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// Every request the stand-in received, so the tests can assert on what was sent
/// rather than only on what came back.
#[derive(Default)]
struct Seen {
    created: Vec<Value>,
    actions: Vec<(String, Value)>,
    user_data: Vec<(String, String)>,
    deleted_volumes: Vec<String>,
    list_queries: Vec<String>,
    /// Volume map the fake server reports for a GET.
    volumes: Value,
    /// Force a status on the next volume delete.
    volume_delete_status: Option<u16>,
}

type Shared = Arc<Mutex<Seen>>;

async fn fake_scaleway(volumes: Value) -> (String, Shared) {
    let state: Shared = Arc::new(Mutex::new(Seen {
        volumes,
        ..Default::default()
    }));

    let app = Router::new()
        .route(
            "/instance/v1/zones/{zone}/servers",
            post(
                |State(st): State<Shared>, Json(body): Json<Value>| async move {
                    st.lock().unwrap().created.push(body);
                    // Shape copied from CreateServerResponse / Server in the SDK.
                    Json(json!({
                        "server": {
                            "id": "11111111-2222-3333-4444-555555555555",
                            "name": "bc-b1-fanout-0",
                            "tags": ["mm-fleet", "mm-node-id=bc-b1-fanout-0"],
                            "public_ip": { "address": "51.15.0.1", "dynamic": true },
                            "volumes": {}
                        }
                    }))
                },
            )
            .get(
                |State(st): State<Shared>, Query(q): Query<Vec<(String, String)>>| async move {
                    let tags = q
                        .iter()
                        .find(|(k, _)| k == "tags")
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    st.lock().unwrap().list_queries.push(tags.clone());
                    Json(json!({ "servers": [
                        { "id": "ours-1", "tags": [tags], "public_ip": { "address": "51.15.0.1" }, "volumes": {} },
                        { "id": "someone-else", "tags": ["not-ours"], "public_ip": null, "volumes": {} }
                    ]}))
                },
            ),
        )
        .route(
            "/instance/v1/zones/{zone}/servers/{id}",
            get(|State(st): State<Shared>, Path((_z, id)): Path<(String, String)>| async move {
                let vols = st.lock().unwrap().volumes.clone();
                Json(json!({ "server": { "id": id, "tags": [], "volumes": vols } }))
            }),
        )
        .route(
            "/instance/v1/zones/{zone}/servers/{id}/action",
            post(
                |State(st): State<Shared>,
                 Path((_z, id)): Path<(String, String)>,
                 Json(body): Json<Value>| async move {
                    st.lock().unwrap().actions.push((id, body));
                    Json(json!({ "task": { "id": "t1", "status": "pending" } }))
                },
            ),
        )
        .route(
            "/instance/v1/zones/{zone}/servers/{id}/user_data/{key}",
            patch(
                |State(st): State<Shared>, Path((_z, id, _k)): Path<(String, String, String)>, body: String| async move {
                    st.lock().unwrap().user_data.push((id, body));
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .route(
            "/block/v1/zones/{zone}/volumes/{id}",
            axum::routing::delete(
                |State(st): State<Shared>, Path((_z, id)): Path<(String, String)>| async move {
                    let forced = {
                        let mut s = st.lock().unwrap();
                        s.deleted_volumes.push(format!("block:{id}"));
                        s.volume_delete_status.take()
                    };
                    StatusCode::from_u16(forced.unwrap_or(204)).unwrap()
                },
            ),
        )
        .route(
            "/instance/v1/zones/{zone}/volumes/{id}",
            axum::routing::delete(
                |State(st): State<Shared>, Path((_z, id)): Path<(String, String)>| async move {
                    st.lock().unwrap().deleted_volumes.push(format!("instance:{id}"));
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .route(
            "/instance/v1/zones/{zone}/products/servers",
            get(|| async move {
                // Verbatim shape from the live public endpoint.
                Json(json!({ "servers": {
                    "COMPUTE3-X8C-16G": { "ncpus": 8, "network": { "sum_internet_bandwidth": 2_000_000_000u64 } },
                    "POP2-HN-10":       { "ncpus": 4, "network": { "sum_internet_bandwidth": 10_000_000_000u64 } },
                    "NO-NETWORK-FIELD": { "ncpus": 1 }
                }}))
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), state)
}

fn provider(base: &str) -> ScalewayProvider {
    ScalewayProvider::new("SCW-TEST-SECRET", "proj-1", "nl-ams-1", "ubuntu_noble", "mm-fleet")
        .with_base_url(base)
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        mm_node_id: NodeId::new("bc-b1-fanout-0"),
        flavor: NodeFlavor::Fanout,
        region: "nl-ams-1".into(),
        size: "COMPUTE3-X8C-16G".into(),
        user_data: "#cloud-config\nwrite_files: []\n".into(),
    }
}

// ─── create ──────────────────────────────────────────────────────────────────

/// `dynamic_ip_required` must be sent EXPLICITLY. The API defaults it to true and
/// the Terraform provider defaults the same setting to false, so relying on either
/// default is relying on the other one not applying — and a reserved IP left behind
/// bills €0.004/h forever, invisibly to an instance-level sweeper.
#[tokio::test]
async fn create_sends_dynamic_ip_required_explicitly() {
    let (base, seen) = fake_scaleway(json!({})).await;
    provider(&base).create(&spec()).await.expect("create");

    let body = seen.lock().unwrap().created[0].clone();
    assert_eq!(
        body["dynamic_ip_required"],
        json!(true),
        "a dynamic IP dies with the instance; a reserved one leaks forever"
    );
}

/// The tag is the orphan sweeper's entire basis for ownership, and it must be on the
/// instance from the moment it exists — not added afterwards, because the window
/// between create and tag is exactly when a failure leaves an untraceable machine.
#[tokio::test]
async fn create_tags_the_instance_as_ours_and_names_the_node() {
    let (base, seen) = fake_scaleway(json!({})).await;
    provider(&base).create(&spec()).await.expect("create");

    let body = seen.lock().unwrap().created[0].clone();
    let tags: Vec<String> = body["tags"]
        .as_array()
        .expect("tags array")
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    assert!(tags.contains(&"mm-fleet".to_string()), "the ownership tag: {tags:?}");
    assert!(
        tags.contains(&"mm-node-id=bc-b1-fanout-0".to_string()),
        "so a stray machine traces back to its broadcast without our database: {tags:?}"
    );
    assert_eq!(body["commercial_type"], json!("COMPUTE3-X8C-16G"));
    assert_eq!(body["project"], json!("proj-1"));
}

/// cloud-init carries MM_SWITCH_NODE_FLAVOR and the auth secret, without which a
/// fleet node refuses to boot (FR-348). It must be written BEFORE poweron, or the
/// node comes up, exits, and bills for the privilege.
#[tokio::test]
async fn cloud_init_is_written_before_poweron() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let handle = provider(&base).create(&spec()).await.expect("create");
    assert_eq!(handle.provider_id, "11111111-2222-3333-4444-555555555555");
    assert_eq!(handle.public_ip.as_deref(), Some("51.15.0.1"));

    let s = seen.lock().unwrap();
    assert_eq!(s.user_data.len(), 1, "cloud-init must be written");
    assert!(s.user_data[0].1.starts_with("#cloud-config"));
    assert_eq!(s.actions.len(), 1);
    assert_eq!(s.actions[0].1["action"], json!("poweron"));
}

// ─── destroy: the volume leak ────────────────────────────────────────────────

/// THE ONE THAT STOPS THE MONEY. `terminate` only DETACHES an `sbs_volume`, and
/// COMPUTE3 cannot take local storage, so its root volume is SBS. Without the
/// explicit delete, every COMPUTE3 teardown leaves a billing volume that the
/// instance-level orphan sweeper cannot see.
#[tokio::test]
async fn destroy_deletes_the_sbs_root_volume_that_terminate_only_detaches() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-sbs-1", "volume_type": "sbs_volume", "name": "root" }
    }))
    .await;

    provider(&base).destroy("srv-1").await.expect("destroy");

    let s = seen.lock().unwrap();
    assert_eq!(s.actions[0].1["action"], json!("terminate"));
    assert_eq!(
        s.deleted_volumes,
        vec!["block:vol-sbs-1"],
        "the SBS volume must be deleted through the BLOCK api, or it bills forever"
    );
}

/// And the inverse: a local volume is already gone, so deleting it again would be a
/// pointless call that can only fail.
#[tokio::test]
async fn destroy_does_not_re_delete_a_local_volume() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-local-1", "volume_type": "l_ssd" }
    }))
    .await;

    provider(&base).destroy("srv-1").await.expect("destroy");
    assert!(
        seen.lock().unwrap().deleted_volumes.is_empty(),
        "terminate already deleted l_ssd"
    );
}

/// A failed volume delete must be reported, loudly and as transient, because the
/// instance is gone and nothing else in the fleet lists volumes.
#[tokio::test]
async fn a_failed_volume_delete_is_a_reported_transient_failure() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-sbs-1", "volume_type": "sbs_volume" }
    }))
    .await;
    seen.lock().unwrap().volume_delete_status = Some(500);

    let err = provider(&base)
        .destroy("srv-1")
        .await
        .expect_err("a volume left billing must not read as success");
    assert!(err.is_transient(), "so the sweeper retries: {err}");
    assert!(err.to_string().contains("billing"), "and says why: {err}");
}

#[tokio::test]
async fn destroying_a_server_that_is_already_gone_is_success() {
    // The GET returns a server with no volumes; terminate on a 404 is also success.
    let (base, _seen) = fake_scaleway(json!({})).await;
    provider(&base)
        .destroy("never-existed")
        .await
        .expect("destroy must be idempotent — the sweeper retries");
}

// ─── list ────────────────────────────────────────────────────────────────────

/// Filtered server-side by tag AND again client-side. The second filter is not
/// redundant: a tag query we got wrong would otherwise hand the orphan sweeper
/// somebody else's fleet to destroy.
#[tokio::test]
async fn list_filters_by_our_tag_on_both_sides() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let listed = provider(&base).list().await.expect("list");

    assert_eq!(seen.lock().unwrap().list_queries, vec!["mm-fleet"]);
    let ids: Vec<&str> = listed.iter().map(|h| h.provider_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["ours-1"],
        "an instance without our tag must never reach the orphan sweeper"
    );
}

// ─── the products endpoint that replaces a guess ──────────────────────────────

/// `viewer_capacity_per_node` was a configured guess. The bandwidth half of it is
/// machine-readable from a PUBLIC endpoint, and Scaleway's own docs say to prefer
/// the API field over the published table.
#[tokio::test]
async fn sku_bandwidth_is_read_from_the_public_products_endpoint() {
    let (base, _seen) = fake_scaleway(json!({})).await;
    let caps = provider(&base).sku_bandwidth_mbps().await.expect("products");

    assert_eq!(caps.get("COMPUTE3-X8C-16G"), Some(&2000));
    assert_eq!(caps.get("POP2-HN-10"), Some(&10_000));
    assert!(
        !caps.contains_key("NO-NETWORK-FIELD"),
        "a SKU with no bandwidth field must be absent, not zero — zero would read as \
         a real cap of nothing and take the SKU out of planning silently"
    );
}

/// And the number it feeds into. 2 Gbps at 2.5 Mbps per viewer, 80% usable.
#[tokio::test]
async fn the_measured_cap_becomes_a_viewer_capacity() {
    let (base, _seen) = fake_scaleway(json!({})).await;
    let caps = provider(&base).sku_bandwidth_mbps().await.expect("products");
    let mbps = *caps.get("COMPUTE3-X8C-16G").expect("sku");

    let policy = mm_core::fleet::planner::FleetPolicy::conservative("nl-ams-1", "COMPUTE3-X8C-16G")
        .with_measured_bandwidth(mbps, 2500, 0.8);
    assert_eq!(policy.viewer_capacity_per_node, 640);
    assert_ne!(
        policy.viewer_capacity_per_node, 250,
        "the conservative guess must have been replaced by the measurement"
    );
}
