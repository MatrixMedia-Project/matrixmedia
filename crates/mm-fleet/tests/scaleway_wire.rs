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
use axum::response::IntoResponse;
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
    deleted_servers: Vec<String>,
    list_queries: Vec<String>,
    /// Every list request's full query, so paging and the absence of a `state`
    /// filter can be asserted rather than assumed.
    list_params: Vec<Vec<(String, String)>>,
    /// When set, the list endpoint serves these pages (1-based) with an
    /// `X-Total-Count` header, which is how the instance API reports its total.
    list_pages: Option<Vec<Vec<Value>>>,
    /// Lie in `X-Total-Count`, to model a list that changes under a paging client.
    list_total_override: Option<usize>,
    /// Serve `list_pages` without the `X-Total-Count` header.
    list_no_total_header: bool,
    /// Volume map the fake server reports for a GET.
    volumes: Value,
    /// Force a status on the next volume delete.
    volume_delete_status: Option<u16>,
    /// `state` the fake reports for a GET of a server. Defaults to `running`.
    server_state: Option<String>,
    /// GET of a server answers 404, as for a server that is already gone.
    server_gone: bool,
    /// Fail the create call with this status and error body.
    create_failure: Option<(u16, Value)>,
    /// Fail this one action (e.g. `poweron`) with this status and error body.
    action_failure: Option<(String, u16, Value)>,
    /// Force a status on the cloud-init PATCH.
    user_data_status: Option<u16>,
    /// States a GET reports, consumed one per GET before `server_state` applies —
    /// to model a server that settles (e.g. `starting` → `running`).
    state_sequence: Vec<String>,
    /// Set once terminate or DELETE was accepted. Removal is asynchronous on
    /// Scaleway, so the server keeps answering GETs for a while.
    removal_requested: bool,
    /// GETs that still find the server (`stopping`) after removal was accepted.
    removal_polls: usize,
    /// Removal was accepted but never completes (the async task failed).
    removal_never_completes: bool,
    /// Volume deletes that answer "still in use" before succeeding.
    volume_in_use_times: usize,
    /// Force a status on DELETE of a server.
    delete_server_status: Option<u16>,
    /// Every mutating call and every GET outcome, in order.
    events: Vec<String>,
}

/// A listed server carrying our tag, in our project, in the given state.
fn our_server(id: &str, state: &str) -> Value {
    json!({ "id": id, "state": state, "project": "proj-1", "tags": ["mm-fleet"], "public_ip": null, "volumes": {} })
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
                    let failure = {
                        let mut s = st.lock().unwrap();
                        s.created.push(body);
                        s.create_failure.take()
                    };
                    if let Some((status, err)) = failure {
                        return (StatusCode::from_u16(status).unwrap(), Json(err)).into_response();
                    }
                    // Shape copied from CreateServerResponse / Server in the SDK.
                    Json(json!({
                        "server": {
                            "id": "11111111-2222-3333-4444-555555555555",
                            "name": "bc-b1-fanout-0",
                            "state": "stopped",
                            "tags": ["mm-fleet", "mm-node-id=bc-b1-fanout-0"],
                            "public_ip": { "address": "51.15.0.1", "dynamic": true },
                            "volumes": {}
                        }
                    }))
                    .into_response()
                },
            )
            .get(
                |State(st): State<Shared>, Query(q): Query<Vec<(String, String)>>| async move {
                    let tags = q
                        .iter()
                        .find(|(k, _)| k == "tags")
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    let page: usize = q
                        .iter()
                        .find(|(k, _)| k == "page")
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(1);
                    let mut s = st.lock().unwrap();
                    s.list_queries.push(tags.clone());
                    s.list_params.push(q.clone());

                    if let Some(pages) = &s.list_pages {
                        let total = s
                            .list_total_override
                            .unwrap_or_else(|| pages.iter().map(Vec::len).sum());
                        let servers = pages.get(page - 1).cloned().unwrap_or_default();
                        if s.list_no_total_header {
                            return Json(json!({ "servers": servers })).into_response();
                        }
                        return (
                            [("x-total-count", total.to_string())],
                            Json(json!({ "servers": servers })),
                        )
                            .into_response();
                    }
                    // No X-Total-Count here on purpose: the paging loop must also stop
                    // correctly when the header is absent.
                    Json(json!({ "servers": [
                        { "id": "ours-1", "project": "proj-1", "tags": [tags], "public_ip": { "address": "51.15.0.1" }, "volumes": {} },
                        { "id": "someone-else", "project": "proj-1", "tags": ["not-ours"], "public_ip": null, "volumes": {} }
                    ]}))
                    .into_response()
                },
            ),
        )
        .route(
            "/instance/v1/zones/{zone}/servers/{id}",
            get(|State(st): State<Shared>, Path((_z, id)): Path<(String, String)>| async move {
                let mut s = st.lock().unwrap();
                if s.removal_requested && !s.removal_never_completes {
                    if s.removal_polls == 0 {
                        s.server_gone = true;
                    } else {
                        s.removal_polls -= 1;
                    }
                }
                if s.server_gone {
                    s.events.push("get:404".into());
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "type": "not_found", "resource": "instance_server", "resource_id": id })),
                    )
                        .into_response();
                }
                let state = if s.removal_requested {
                    "stopping".to_string()
                } else if !s.state_sequence.is_empty() {
                    s.state_sequence.remove(0)
                } else {
                    s.server_state.clone().unwrap_or_else(|| "running".into())
                };
                s.events.push(format!("get:{state}"));
                Json(json!({ "server": { "id": id, "state": state, "tags": [], "volumes": s.volumes.clone() } }))
                    .into_response()
            })
            .delete(
                |State(st): State<Shared>, Path((_z, id)): Path<(String, String)>| async move {
                    let mut s = st.lock().unwrap();
                    s.deleted_servers.push(id);
                    s.events.push("delete-server".into());
                    if let Some(status) = s.delete_server_status.take() {
                        return StatusCode::from_u16(status).unwrap();
                    }
                    s.removal_requested = true;
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .route(
            "/instance/v1/zones/{zone}/servers/{id}/action",
            post(
                |State(st): State<Shared>,
                 Path((_z, id)): Path<(String, String)>,
                 Json(body): Json<Value>| async move {
                    let mut s = st.lock().unwrap();
                    let failure = match &s.action_failure {
                        Some((action, status, err)) if body["action"] == json!(action) => {
                            Some((*status, err.clone()))
                        }
                        _ => None,
                    };
                    s.events.push(format!("action:{}", body["action"].as_str().unwrap_or("?")));
                    let terminate = body["action"] == json!("terminate");
                    s.actions.push((id, body));
                    if let Some((status, err)) = failure {
                        return (StatusCode::from_u16(status).unwrap(), Json(err)).into_response();
                    }
                    if terminate {
                        s.removal_requested = true;
                    }
                    Json(json!({ "task": { "id": "t1", "status": "pending" } })).into_response()
                },
            ),
        )
        .route(
            "/instance/v1/zones/{zone}/servers/{id}/user_data/{key}",
            patch(
                |State(st): State<Shared>, Path((_z, id, _k)): Path<(String, String, String)>, body: String| async move {
                    let mut s = st.lock().unwrap();
                    s.user_data.push((id, body));
                    StatusCode::from_u16(s.user_data_status.unwrap_or(204)).unwrap()
                },
            ),
        )
        .route(
            "/block/v1/zones/{zone}/volumes/{id}",
            axum::routing::delete(
                |State(st): State<Shared>, Path((_z, id)): Path<(String, String)>| async move {
                    let mut s = st.lock().unwrap();
                    s.events.push(format!("delete-volume:{id}"));
                    if s.volume_in_use_times > 0 {
                        s.volume_in_use_times -= 1;
                        // The block API refuses an `in_use` volume (block_sdk.go DeleteVolume).
                        return (
                            StatusCode::PRECONDITION_FAILED,
                            Json(json!({ "type": "precondition_failed", "precondition": "resource_still_in_use" })),
                        )
                            .into_response();
                    }
                    s.deleted_volumes.push(format!("block:{id}"));
                    let forced = s.volume_delete_status.take();
                    StatusCode::from_u16(forced.unwrap_or(204)).unwrap().into_response()
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
        // Real waits are seconds apart; the stand-in needs none, only a bound.
        .with_settle(std::time::Duration::ZERO, 5)
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
    assert_eq!(
        handle.provider_id, "nl-ams-1/11111111-2222-3333-4444-555555555555",
        "zoned, as Terraform's scaleway_instance_server.id is"
    );
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

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");

    let s = seen.lock().unwrap();
    assert_eq!(s.actions[0].1["action"], json!("terminate"));
    assert!(s.deleted_servers.is_empty(), "a running server is terminated, not deleted");
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

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");
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
        .destroy("nl-ams-1/srv-1")
        .await
        .expect_err("a volume left billing must not read as success");
    assert!(err.is_transient(), "so the sweeper retries: {err}");
    assert!(err.to_string().contains("billing"), "and says why: {err}");
}

/// The server vanishes between our GET and our terminate: a 404 there is success.
#[tokio::test]
async fn a_server_that_vanishes_before_terminate_is_success() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().action_failure = Some((
        "terminate".into(),
        404,
        json!({ "type": "not_found", "resource": "instance_server" }),
    ));
    provider(&base)
        .destroy("nl-ams-1/never-existed")
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
        vec!["nl-ams-1/ours-1"],
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

// ─── 2026-10-04 adapter fixes ────────────────────────────────────────────────
//
// Sources for the behaviour pinned below (all read 2026-10-04):
// - scaleway-sdk-go `api/instance/v1`: server states, `terminate` semantics, and
//   `ListServersResponse.total_count` filled from the `X-Total-Count` header
//   (`scw/client.go`).
// - scaleway-sdk-go `scw/errors.go`: errors dispatch on the body's `type`
//   (`out_of_stock`, `quotas_exceeded`, `transient_state`, ...), not on status.
// - terraform-provider-scaleway `instance/testfuncs/sweep.go`: lists with NO
//   `state` filter and handles `stopped` / `stopped in place` servers with
//   `DeleteServer`, `running` ones with `terminate`.
// - terraform-provider-scaleway `instance/server.go`: terminate needs a running
//   server ("reach running state (mandatory for termination)").

fn gpu_provider(base: &str) -> ScalewayProvider {
    provider(base).with_gpu_image("ubuntu_noble_gpu_os_13_nvidia")
}

fn transcode_spec() -> InstanceSpec {
    InstanceSpec {
        mm_node_id: NodeId::new("bc-b1-transcode-0"),
        flavor: NodeFlavor::Transcode,
        region: "eu-par".into(),
        size: "L4-1-24G".into(),
        user_data: "#cloud-config\nwrite_files: []\n".into(),
    }
}

// ─── create: the GPU image ───────────────────────────────────────────────────

/// A transcode node on a plain Ubuntu image has no NVIDIA driver, so NVENC is
/// absent and the node bills per minute while doing nothing useful. Scaleway's
/// GPU OS image ships the driver, so no driver build happens on billable boot time.
#[tokio::test]
async fn a_transcode_node_boots_the_gpu_image() {
    let (base, seen) = fake_scaleway(json!({})).await;
    gpu_provider(&base).create(&transcode_spec()).await.expect("create");

    assert_eq!(
        seen.lock().unwrap().created[0]["image"],
        json!("ubuntu_noble_gpu_os_13_nvidia")
    );
}

#[tokio::test]
async fn a_fanout_node_keeps_the_plain_image_when_a_gpu_image_is_configured() {
    let (base, seen) = fake_scaleway(json!({})).await;
    gpu_provider(&base).create(&spec()).await.expect("create");

    assert_eq!(seen.lock().unwrap().created[0]["image"], json!("ubuntu_noble"));
}

/// Refused BEFORE any call: a GPU node on the wrong image is money spent on a
/// machine that cannot encode, and no machine is better than that one.
#[tokio::test]
async fn a_transcode_node_without_a_gpu_image_is_refused_before_any_call() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let err = provider(&base)
        .create(&transcode_spec())
        .await
        .expect_err("no GPU image configured");

    assert!(err.needs_human(), "a missing config value does not fix itself: {err}");
    assert!(seen.lock().unwrap().created.is_empty(), "nothing may be created");
}

// ─── create: failures after the server exists ────────────────────────────────

/// THE LIKELIEST LEAK. Scaleway frees the hypervisor slot on poweroff, so a GPU
/// stock-out can surface at `poweron` rather than at create. The server already
/// exists then — stopped, with an SBS root volume that bills — so create must
/// delete it rather than hand the problem to the orphan sweeper.
#[tokio::test]
async fn a_failed_poweron_deletes_the_server_it_created() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-root", "volume_type": "sbs_volume" }
    }))
    .await;
    {
        let mut s = seen.lock().unwrap();
        s.server_state = Some("stopped".into());
        // The status is not what we classify on; the body's `type` is.
        s.action_failure = Some((
            "poweron".into(),
            412,
            json!({ "type": "out_of_stock", "resource": "L4-1-24G", "message": "out of stock" }),
        ));
    }

    let err = gpu_provider(&base)
        .create(&transcode_spec())
        .await
        .expect_err("poweron failed");

    assert!(err.is_capacity(), "a stock-out is a capacity error, not a page: {err}");
    let s = seen.lock().unwrap();
    assert_eq!(
        s.deleted_servers,
        vec!["11111111-2222-3333-4444-555555555555"],
        "the stopped server must be deleted, not left for the sweeper"
    );
    assert_eq!(s.deleted_volumes, vec!["block:vol-root"], "and its SBS root volume");
    assert!(
        s.actions.iter().all(|(_, a)| a["action"] != json!("terminate")),
        "terminate needs a running server; a stopped one is deleted"
    );
}

/// The same cleanup when cloud-init cannot be written: the server exists, is
/// stopped, and would never boot usefully without its auth secret (FR-348).
#[tokio::test]
async fn a_failed_cloud_init_write_deletes_the_server_it_created() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_state = Some("stopped".into());
        s.user_data_status = Some(400);
    }

    provider(&base).create(&spec()).await.expect_err("cloud-init write failed");

    let s = seen.lock().unwrap();
    assert!(s.actions.is_empty(), "never powered on: {:?}", s.actions);
    assert_eq!(s.deleted_servers, vec!["11111111-2222-3333-4444-555555555555"]);
}

/// A quota refusal comes back from create itself, so nothing exists to clean up,
/// and it needs a ticket to Scaleway support — the default is ONE L4 per
/// organisation.
#[tokio::test]
async fn a_quota_refusal_needs_a_human_and_creates_nothing() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().create_failure = Some((
        403,
        json!({
            "type": "quotas_exceeded",
            "message": "Quotas exceeded",
            "details": [{ "resource": "L4-1-24G", "quota": 1, "current": 1 }]
        }),
    ));

    let err = gpu_provider(&base)
        .create(&transcode_spec())
        .await
        .expect_err("over quota");

    assert!(err.is_quota(), "{err}");
    assert!(err.needs_human(), "a quota is raised by a ticket, not a retry: {err}");
    let s = seen.lock().unwrap();
    assert!(s.deleted_servers.is_empty() && s.actions.is_empty());
}

// ─── destroy: every server state ─────────────────────────────────────────────

/// `terminate` needs a RUNNING server. Powering a stopped GPU node on just to
/// terminate it would bill a minute and can itself fail on a stock-out, so a
/// stopped server is deleted directly — what Scaleway's own sweeper does.
#[tokio::test]
async fn destroy_deletes_a_stopped_server_instead_of_terminating_it() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-sbs-1", "volume_type": "sbs_volume" }
    }))
    .await;
    seen.lock().unwrap().server_state = Some("stopped".into());

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");

    let s = seen.lock().unwrap();
    assert!(s.actions.is_empty(), "no poweron, no terminate: {:?}", s.actions);
    assert_eq!(s.deleted_servers, vec!["srv-1"]);
    assert_eq!(s.deleted_volumes, vec!["block:vol-sbs-1"]);
}

/// Standby bills like a running instance, so it matters most that this path works.
#[tokio::test]
async fn destroy_deletes_a_server_in_standby() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().server_state = Some("stopped in place".into());

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");

    let s = seen.lock().unwrap();
    assert!(s.actions.is_empty(), "{:?}", s.actions);
    assert_eq!(s.deleted_servers, vec!["srv-1"]);
}

/// Deleting a server is not `terminate`: nothing documents it removing local
/// volumes, so they are deleted explicitly. A 404 there is success, so the
/// cautious direction costs one call at most.
#[tokio::test]
async fn destroy_of_a_stopped_server_also_deletes_its_local_volumes() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-local-1", "volume_type": "l_ssd" }
    }))
    .await;
    seen.lock().unwrap().server_state = Some("stopped".into());

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");
    assert_eq!(seen.lock().unwrap().deleted_volumes, vec!["instance:vol-local-1"]);
}

/// Mid-transition: neither terminate nor delete is valid yet. Retry, don't page.
#[tokio::test]
async fn destroy_of_a_server_in_transition_is_transient_and_touches_nothing() {
    for state in ["starting", "stopping"] {
        let (base, seen) = fake_scaleway(json!({})).await;
        seen.lock().unwrap().server_state = Some(state.into());

        let err = provider(&base).destroy("nl-ams-1/srv-1").await.expect_err(state);
        assert!(err.is_transient(), "{state}: {err}");
        let s = seen.lock().unwrap();
        assert!(s.actions.is_empty() && s.deleted_servers.is_empty(), "{state}");
    }
}

/// `locked` is Scaleway's hold (abuse, billing); retrying cannot clear it.
#[tokio::test]
async fn destroy_of_a_locked_server_needs_a_human() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().server_state = Some("locked".into());

    let err = provider(&base).destroy("nl-ams-1/srv-1").await.expect_err("locked");
    assert!(err.needs_human(), "{err}");
    assert!(seen.lock().unwrap().deleted_servers.is_empty());
}

/// Already gone is success, and nothing more is sent: a terminate against a
/// missing server can only fail or, worse, hit an id that was reused.
#[tokio::test]
async fn destroy_of_a_server_that_is_already_gone_sends_nothing_more() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().server_gone = true;

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("idempotent");
    let s = seen.lock().unwrap();
    assert!(s.actions.is_empty() && s.deleted_servers.is_empty(), "{:?}", s.actions);
}

// ─── list: every page, every state ───────────────────────────────────────────

/// One page is 100 servers. A sweeper that stops there never sees the 101st —
/// and an invisible machine is one nobody destroys.
#[tokio::test]
async fn list_follows_every_page() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let page = |from: usize, n: usize| -> Vec<Value> {
        (from..from + n).map(|i| our_server(&format!("srv-{i}"), "running")).collect()
    };
    seen.lock().unwrap().list_pages = Some(vec![page(0, 100), page(100, 100), page(200, 5)]);

    let listed = provider(&base).list().await.expect("list");
    assert_eq!(listed.len(), 205);

    let pages: Vec<String> = seen
        .lock()
        .unwrap()
        .list_params
        .iter()
        .map(|q| q.iter().find(|(k, _)| k == "page").map(|(_, v)| v.clone()).unwrap_or_default())
        .collect();
    assert_eq!(pages, vec!["1", "2", "3"]);
}

/// A list that ends before the advertised total is an ERROR, never a short Ok —
/// a short list reads as "these machines are not ours any more" to nobody, and
/// as "these machines do not exist" to the sweeper.
#[tokio::test]
async fn a_list_that_ends_before_its_total_is_an_error() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.list_pages = Some(vec![(0..100).map(|i| our_server(&format!("srv-{i}"), "running")).collect()]);
        s.list_total_override = Some(205);
    }

    let err = provider(&base).list().await.expect_err("short list");
    assert!(err.is_transient(), "the list may be changing under us; retry: {err}");
}

/// Pins the verified behaviour: no `state` filter is sent, so stopped and standby
/// servers — which still bill for volumes, or in full — reach the sweeper.
#[tokio::test]
async fn list_sends_no_state_filter_and_returns_stopped_servers() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().list_pages = Some(vec![vec![
        our_server("srv-run", "running"),
        our_server("srv-stop", "stopped"),
        our_server("srv-standby", "stopped in place"),
    ]]);

    let listed = provider(&base).list().await.expect("list");
    assert_eq!(listed.len(), 3);
    assert!(
        seen.lock().unwrap().list_params.iter().all(|q| q.iter().all(|(k, _)| k != "state")),
        "a state filter would hide stopped servers from the sweeper"
    );
}

// ─── review round: ids, projects, asynchronous removal ───────────────────────

/// The provider id is `zone/uuid` — the form Terraform's `scaleway_instance_server.id`
/// takes (`zonal.NewIDString`) — so both paths store the same string, and an id
/// carries the zone it lives in.
#[tokio::test]
async fn list_and_create_return_zoned_ids() {
    let (base, _seen) = fake_scaleway(json!({})).await;
    let p = provider(&base);
    let created = p.create(&spec()).await.expect("create");
    let listed = p.list().await.expect("list");

    assert!(created.provider_id.starts_with("nl-ams-1/"), "{}", created.provider_id);
    assert!(listed.iter().all(|h| h.provider_id.starts_with("nl-ams-1/")));
}

/// THE CROSS-ZONE LEAK. A server in fr-par-2 GETs as 404 from nl-ams-1, and a 404
/// is "already gone" — so without the zone in the id, destroying it through the
/// wrong provider would report success while the GPU bills on.
#[tokio::test]
async fn destroying_an_id_from_another_zone_is_refused_and_sends_nothing() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let err = provider(&base)
        .destroy("fr-par-2/11111111-2222-3333-4444-555555555555")
        .await
        .expect_err("another zone's server");

    assert!(err.needs_human(), "routing a destroy to the wrong zone is a bug: {err}");
    let s = seen.lock().unwrap();
    assert!(s.events.is_empty(), "not even a GET: {:?}", s.events);
}

/// A bare UUID does not say which zone it is in, so it cannot be destroyed safely.
#[tokio::test]
async fn destroying_an_id_without_a_zone_is_refused_and_sends_nothing() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let err = provider(&base)
        .destroy("11111111-2222-3333-4444-555555555555")
        .await
        .expect_err("ambiguous id");
    assert!(err.needs_human(), "{err}");
    assert!(seen.lock().unwrap().events.is_empty());
}

/// Without a project filter, the list covers every project the key can reach —
/// and a stage fleet in another project of the same organisation, carrying the
/// same default tag, would be destroyed by prod's orphan sweeper.
#[tokio::test]
async fn list_is_scoped_to_our_project_on_both_sides() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let mut stage = our_server("stage-1", "running");
    stage["project"] = json!("proj-STAGE");
    seen.lock().unwrap().list_pages = Some(vec![vec![our_server("prod-1", "running"), stage]]);

    let listed = provider(&base).list().await.expect("list");
    let ids: Vec<&str> = listed.iter().map(|h| h.provider_id.as_str()).collect();
    assert_eq!(ids, vec!["nl-ams-1/prod-1"], "another project's server must never be ours");
    assert!(
        seen.lock().unwrap().list_params[0].contains(&("project".into(), "proj-1".into())),
        "and the filter is sent server-side too"
    );
}

/// The list is newest-first, so a server created between two page fetches pushes
/// one we already saw onto the next page. A repeated id means the list moved under
/// us — retry, rather than report a list whose completeness is a coincidence.
#[tokio::test]
async fn an_id_repeated_across_pages_is_an_error() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let mut first: Vec<Value> = (0..100).map(|i| our_server(&format!("srv-{i}"), "running")).collect();
    first[99] = our_server("srv-dup", "running");
    seen.lock().unwrap().list_pages = Some(vec![first, vec![our_server("srv-dup", "running")]]);

    let err = provider(&base).list().await.expect_err("duplicate across pages");
    assert!(err.is_transient(), "{err}");
}

/// Without the header, a full page means "there may be more".
#[tokio::test]
async fn without_a_total_a_full_page_fetches_the_next() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.list_pages = Some(vec![(0..100).map(|i| our_server(&format!("srv-{i}"), "running")).collect()]);
        s.list_no_total_header = true;
    }

    let listed = provider(&base).list().await.expect("list");
    assert_eq!(listed.len(), 100);
    assert_eq!(seen.lock().unwrap().list_params.len(), 2, "a full page is not the last page");
}

/// `terminate` is ASYNCHRONOUS: a 2xx only means it was queued. The SBS volume
/// stays `in_use` until it finishes, and the block API refuses an `in_use`
/// volume — so the volume delete must wait for the server to be gone.
#[tokio::test]
async fn destroy_waits_for_the_server_to_go_before_deleting_its_volumes() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-sbs-1", "volume_type": "sbs_volume" }
    }))
    .await;
    seen.lock().unwrap().removal_polls = 2;

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");

    let events = seen.lock().unwrap().events.clone();
    let gone = events.iter().position(|e| e == "get:404").expect("polled until gone");
    let deleted = events.iter().position(|e| e == "delete-volume:vol-sbs-1").expect("volume deleted");
    assert!(gone < deleted, "volume deleted before the server was gone: {events:?}");
}

/// An accepted terminate can still fail. Ok here would mark the node Gone, and a
/// Gone node is "known" to the orphan sweeper — so nothing would ever retry.
#[tokio::test]
async fn a_server_that_never_goes_away_is_a_transient_failure() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-sbs-1", "volume_type": "sbs_volume" }
    }))
    .await;
    seen.lock().unwrap().removal_never_completes = true;

    let err = provider(&base).destroy("nl-ams-1/srv-1").await.expect_err("still there");
    assert!(err.is_transient(), "{err}");
    assert!(
        seen.lock().unwrap().deleted_volumes.is_empty(),
        "an attached volume cannot be deleted; try again with the server"
    );
}

#[tokio::test]
async fn a_volume_still_in_use_is_retried_until_it_can_be_deleted() {
    let (base, seen) = fake_scaleway(json!({
        "0": { "id": "vol-sbs-1", "volume_type": "sbs_volume" }
    }))
    .await;
    seen.lock().unwrap().volume_in_use_times = 2;

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");
    assert_eq!(seen.lock().unwrap().deleted_volumes, vec!["block:vol-sbs-1"]);
}

/// A server caught `starting` — e.g. a poweron that was accepted but whose reply
/// was lost — is waited out and then terminated, not abandoned.
#[tokio::test]
async fn a_starting_server_is_waited_out_and_then_terminated() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().state_sequence = vec!["starting".into(), "starting".into(), "running".into()];

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");
    let s = seen.lock().unwrap();
    assert_eq!(s.actions.len(), 1);
    assert_eq!(s.actions[0].1["action"], json!("terminate"));
}

/// When the cleanup itself fails, the caller still gets the error that STOPPED the
/// create — a stock-out must stay a capacity error so the planner tries elsewhere.
#[tokio::test]
async fn a_failed_cleanup_keeps_the_original_create_error() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_state = Some("stopped".into());
        s.delete_server_status = Some(500);
        s.action_failure = Some((
            "poweron".into(),
            412,
            json!({ "type": "out_of_stock", "resource": "L4-1-24G" }),
        ));
    }

    let err = gpu_provider(&base).create(&transcode_spec()).await.expect_err("poweron failed");
    assert!(err.is_capacity(), "{err}");
}
