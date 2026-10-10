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
use mm_fleet::provider::{InstanceSpec, Provider, ProviderError};
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
    /// Answer the create with 201 and exactly this body, to model a 2xx whose body is not a
    /// server. The server is made all the same, as far as the caller can know.
    create_body: Option<String>,
    /// Accept a create or a server list and never answer it (a hung provider).
    hang_requests: bool,
    /// Accept this action (e.g. `poweron`) and never answer it.
    hang_action: Option<String>,
    /// Fail this one action (e.g. `poweron`) with this status and error body.
    action_failure: Option<(String, u16, Value)>,
    /// Force a status on the cloud-init PATCH.
    user_data_status: Option<u16>,
    /// The body the cloud-init PATCH answers with (an error body that echoes the request).
    user_data_body: Option<String>,
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
    /// The volume map a create reports for the new server.
    create_volumes: Value,
    /// Block volumes the block API lists (shape: block/v1 `Volume`).
    block_volumes: Vec<Value>,
    /// Query of every block-volume list request.
    volume_list_params: Vec<Vec<(String, String)>>,
    /// Fail the block-volume list with this status.
    volume_list_status: Option<u16>,
    /// Every block-volume PATCH: (volume id, body).
    volume_patches: Vec<(String, Value)>,
    /// Fail the next block-volume PATCH with this status.
    volume_patch_status: Option<u16>,
    /// PATCHes that answer `transient_state` (volume still being created) first.
    volume_patch_in_use_times: usize,
    /// A block DELETE leaves the volume in this status instead of removing it —
    /// deletion is asynchronous and can fail after the 2xx.
    volume_delete_leaves: Option<String>,
    /// The block list ignores its project and tag filters (a server-side filter
    /// we got wrong), so the client-side filters are what is tested.
    volume_list_ignores_filters: bool,
}

/// A block volume as block/v1 lists it, tagged as ours for `server`. `attached`
/// gives it a live reference to that server, as a root volume has.
fn our_volume(id: &str, server: &str, attached: bool) -> Value {
    let references = if attached {
        json!([{ "id": "ref-1", "product_resource_type": "instance_server",
                 "product_resource_id": server, "status": "attached", "type": "exclusive" }])
    } else {
        json!([])
    };
    json!({
        "id": id, "type": "sbs_5k", "project_id": "proj-1",
        "status": if attached { "in_use" } else { "available" },
        "references": references,
        "tags": ["mm-fleet", "mm-node-id=bc-b1-transcode-0", format!("mm-server={server}")]
    })
}

/// A listed server carrying our tag, in our project, in the given state.
fn our_server(id: &str, state: &str) -> Value {
    json!({ "id": id, "state": state, "project": "proj-1", "tags": ["mm-fleet"], "public_ip": null, "volumes": {} })
}

type Shared = Arc<Mutex<Seen>>;

async fn fake_scaleway(volumes: Value) -> (String, Shared) {
    let state: Shared = Arc::new(Mutex::new(Seen {
        volumes,
        create_volumes: json!({}),
        ..Default::default()
    }));

    let app = Router::new()
        .route(
            "/instance/v1/zones/{zone}/servers",
            post(
                |State(st): State<Shared>, Json(body): Json<Value>| async move {
                    let hang = st.lock().unwrap().hang_requests;
                    if hang {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                    let (failure, raw) = {
                        let mut s = st.lock().unwrap();
                        s.created.push(body);
                        (s.create_failure.take(), s.create_body.clone())
                    };
                    if let Some((status, err)) = failure {
                        return (StatusCode::from_u16(status).unwrap(), Json(err)).into_response();
                    }
                    if let Some(raw) = raw {
                        return (StatusCode::CREATED, raw).into_response();
                    }
                    // Shape copied from CreateServerResponse / Server in the SDK.
                    Json(json!({
                        "server": {
                            "id": "11111111-2222-3333-4444-555555555555",
                            "name": "bc-b1-fanout-0",
                            "state": "stopped",
                            "creation_date": "2026-10-04T10:00:00+00:00",
                            "tags": ["mm-fleet", "mm-node-id=bc-b1-fanout-0"],
                            "public_ip": { "address": "51.15.0.1", "dynamic": true },
                            "volumes": st.lock().unwrap().create_volumes.clone()
                        }
                    }))
                    .into_response()
                },
            )
            .get(
                |State(st): State<Shared>, Query(q): Query<Vec<(String, String)>>| async move {
                    let hang = st.lock().unwrap().hang_requests;
                    if hang {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
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
                    s.events.push("list-servers".into());
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
                    let hang = matches!(&st.lock().unwrap().hang_action, Some(a) if body["action"] == json!(a));
                    if hang {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
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
                    s.events.push("user-data".into());
                    s.user_data.push((id, body));
                    let status = StatusCode::from_u16(s.user_data_status.unwrap_or(204)).unwrap();
                    match s.user_data_body.clone() {
                        Some(body) => (status, body).into_response(),
                        None => status.into_response(),
                    }
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
                    if forced.is_none() {
                        match s.volume_delete_leaves.clone() {
                            Some(status) => {
                                for v in s.block_volumes.iter_mut().filter(|v| v["id"] == json!(id)) {
                                    v["status"] = json!(status);
                                }
                            }
                            None => s.block_volumes.retain(|v| v["id"] != json!(id)),
                        }
                    }
                    StatusCode::from_u16(forced.unwrap_or(204)).unwrap().into_response()
                },
            )
            .patch(
                |State(st): State<Shared>,
                 Path((_z, id)): Path<(String, String)>,
                 Json(body): Json<Value>| async move {
                    let mut s = st.lock().unwrap();
                    s.events.push(format!("tag-volume:{id}"));
                    s.volume_patches.push((id.clone(), body.clone()));
                    if let Some(status) = s.volume_patch_status.take() {
                        return StatusCode::from_u16(status).unwrap().into_response();
                    }
                    if s.volume_patch_in_use_times > 0 {
                        s.volume_patch_in_use_times -= 1;
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({ "type": "transient_state", "resource": "volume", "current_state": "creating" })),
                        )
                            .into_response();
                    }
                    for v in s.block_volumes.iter_mut().filter(|v| v["id"] == json!(id)) {
                        v["tags"] = body["tags"].clone();
                    }
                    Json(json!({ "id": id })).into_response()
                },
            )
            .get(
                |State(st): State<Shared>, Path((_z, id)): Path<(String, String)>| async move {
                    let s = st.lock().unwrap();
                    match s.block_volumes.iter().find(|v| v["id"] == json!(id)) {
                        Some(v) => Json(v.clone()).into_response(),
                        None => (StatusCode::NOT_FOUND, Json(json!({ "type": "not_found" }))).into_response(),
                    }
                },
            ),
        )
        .route(
            "/block/v1/zones/{zone}/volumes",
            get(
                |State(st): State<Shared>, Query(q): Query<Vec<(String, String)>>| async move {
                    let mut s = st.lock().unwrap();
                    s.events.push("list-volumes".into());
                    s.volume_list_params.push(q.clone());
                    if let Some(status) = s.volume_list_status {
                        return StatusCode::from_u16(status).unwrap().into_response();
                    }
                    // OR semantics: "Only volumes with one or more matching tags".
                    let wanted: Vec<&String> =
                        q.iter().filter(|(k, _)| k == "tags").map(|(_, v)| v).collect();
                    let project = q.iter().find(|(k, _)| k == "project_id").map(|(_, v)| v.clone());
                    let matching: Vec<Value> = s
                        .block_volumes
                        .iter()
                        .filter(|v| {
                            s.volume_list_ignores_filters
                                || wanted.is_empty()
                                || v["tags"].as_array().is_some_and(|tags| {
                                    tags.iter().any(|t| wanted.iter().any(|w| t == &json!(w)))
                                })
                        })
                        .filter(|v| {
                            s.volume_list_ignores_filters
                                || project.as_ref().is_none_or(|p| v["project_id"] == json!(p))
                        })
                        .cloned()
                        .collect();
                    let page: usize = q.iter().find(|(k, _)| k == "page").and_then(|(_, v)| v.parse().ok()).unwrap_or(1);
                    let size: usize = q.iter().find(|(k, _)| k == "page_size").and_then(|(_, v)| v.parse().ok()).unwrap_or(50);
                    let slice: Vec<Value> = matching.iter().skip((page - 1) * size).take(size).cloned().collect();
                    Json(json!({ "volumes": slice, "total_count": matching.len() })).into_response()
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
            )
            .get(|| async { (StatusCode::NOT_FOUND, Json(json!({ "type": "not_found" }))) }),
        )
        .route(
            "/instance/v1/zones/{zone}/products/servers",
            get(|| async move {
                // Verbatim shape from the live public endpoint, which pages and gives the
                // total in `x-total-count`.
                (
                    [("x-total-count", "3")],
                    Json(json!({ "servers": {
                        "COMPUTE3-X8C-16G": { "ncpus": 8, "network": { "sum_internet_bandwidth": 2_000_000_000u64 } },
                        "POP2-HN-10":       { "ncpus": 4, "network": { "sum_internet_bandwidth": 10_000_000_000u64 } },
                        "NO-NETWORK-FIELD": { "ncpus": 1 }
                    }})),
                )
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

// ─── find ────────────────────────────────────────────────────────────────────

fn tagged_server(id: &str, project: &str, tags: &[&str]) -> Value {
    json!({ "id": id, "state": "stopped", "project": project, "tags": tags,
            "creation_date": "2026-10-07T10:00:00+00:00", "public_ip": null, "volumes": {} })
}

#[tokio::test]
async fn find_returns_the_server_tagged_with_the_node_id() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().list_pages = Some(vec![vec![tagged_server(
        "srv-1",
        "proj-1",
        &["mm-fleet", "mm-node-id=tb-abc"],
    )]]);
    let h = provider(&base)
        .find(&NodeId::new("tb-abc"))
        .await
        .unwrap()
        .expect("found");
    assert_eq!(h.provider_id, "nl-ams-1/srv-1");
    let q = seen.lock().unwrap().list_params.last().cloned().unwrap();
    assert!(
        q.contains(&("tags".to_string(), "mm-node-id=tb-abc".to_string())),
        "{q:?}"
    );
    assert!(
        q.contains(&("project".to_string(), "proj-1".to_string())),
        "{q:?}"
    );
    assert_eq!(
        q.iter().filter(|(k, _)| k == "tags").count(),
        1,
        "exactly one tags pair: {q:?}"
    );
}

#[tokio::test]
async fn find_ignores_servers_of_another_project_or_without_our_fleet_tag() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().list_pages = Some(vec![vec![
        tagged_server(
            "srv-other-project",
            "proj-2",
            &["mm-fleet", "mm-node-id=tb-abc"],
        ),
        tagged_server("srv-not-ours", "proj-1", &["mm-node-id=tb-abc"]),
    ]]);
    assert_eq!(
        provider(&base).find(&NodeId::new("tb-abc")).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn a_failed_lookup_is_an_error_never_none() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().list_pages = None;
    // Point at a path the fake does not serve for this zone's list: any non-2xx must be Err.
    let p = ScalewayProvider::new(
        "SCW-TEST-SECRET",
        "proj-1",
        "nl-ams-1",
        "ubuntu_noble",
        "mm-fleet",
    )
    .with_base_url(format!("{base}/nowhere"));
    assert!(p.find(&NodeId::new("tb-abc")).await.is_err());
}

/// The fake ignores the `tags` query, as a server-side filter we got wrong would: the
/// node tag must be checked client-side too, or `find` hands back another node's machine.
#[tokio::test]
async fn find_picks_the_server_with_this_node_tag_among_the_fleets_servers() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().list_pages = Some(vec![vec![
        tagged_server(
            "srv-other-node",
            "proj-1",
            &["mm-fleet", "mm-node-id=tb-zzz"],
        ),
        tagged_server("srv-ours", "proj-1", &["mm-fleet", "mm-node-id=tb-abc"]),
    ]]);
    let h = provider(&base)
        .find(&NodeId::new("tb-abc"))
        .await
        .unwrap()
        .expect("found");
    assert_eq!(h.provider_id, "nl-ams-1/srv-ours");
    assert_eq!(
        provider(&base).find(&NodeId::new("tb-none")).await.unwrap(),
        None,
        "fleet servers of other nodes are not a match"
    );
}

/// A fleet server for node `tb-abc`, created at `created` (`None` = the API gave no date).
fn abc_server(id: &str, created: Option<&str>) -> Value {
    let mut s = tagged_server(id, "proj-1", &["mm-fleet", "mm-node-id=tb-abc"]);
    s["creation_date"] = created.map_or(Value::Null, |c| json!(c));
    s
}

/// The caller records the handle `find` returns, so the sweep reaps the others as orphans:
/// the one kept must be the oldest, whatever order the API listed them in.
#[tokio::test]
async fn several_servers_for_one_node_return_the_oldest() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().list_pages = Some(vec![vec![
        abc_server("srv-newest", Some("2026-10-07T11:00:00+00:00")),
        abc_server("srv-oldest", Some("2026-10-07T09:00:00+00:00")),
        abc_server("srv-middle", Some("2026-10-07T10:00:00+00:00")),
    ]]);
    let h = provider(&base)
        .find(&NodeId::new("tb-abc"))
        .await
        .unwrap()
        .expect("found");
    assert_eq!(h.provider_id, "nl-ams-1/srv-oldest");
}

#[tokio::test]
async fn a_server_of_unknown_age_loses_to_one_with_a_date() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().list_pages = Some(vec![vec![
        abc_server("srv-undated", None),
        abc_server("srv-dated", Some("2026-10-07T11:00:00+00:00")),
    ]]);
    let h = provider(&base)
        .find(&NodeId::new("tb-abc"))
        .await
        .unwrap()
        .expect("found");
    assert_eq!(h.provider_id, "nl-ams-1/srv-dated");
}

/// Same creation time: the provider id decides, so two runs over the same servers agree
/// even when the pages arrive in a different order.
#[tokio::test]
async fn servers_created_at_the_same_moment_are_told_apart_by_provider_id() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let at = Some("2026-10-07T10:00:00+00:00");
    for listed in [["srv-b", "srv-a"], ["srv-a", "srv-b"]] {
        seen.lock().unwrap().list_pages =
            Some(vec![listed.iter().map(|id| abc_server(id, at)).collect()]);
        let h = provider(&base)
            .find(&NodeId::new("tb-abc"))
            .await
            .unwrap()
            .expect("found");
        assert_eq!(h.provider_id, "nl-ams-1/srv-a", "listed as {listed:?}");
    }
}

/// The older duplicate is on the second page: stopping after the first page keeps the wrong one.
#[tokio::test]
async fn find_reads_every_page_so_an_older_match_on_a_later_page_is_seen() {
    let (base, seen) = fake_scaleway(json!({})).await;
    let first: Vec<Value> = (0..99)
        .map(|i| {
            tagged_server(
                &format!("srv-x{i}"),
                "proj-1",
                &["mm-fleet", "mm-node-id=tb-other"],
            )
        })
        .chain([abc_server("srv-1", Some("2026-10-07T11:00:00+00:00"))])
        .collect();
    seen.lock().unwrap().list_pages = Some(vec![
        first,
        vec![abc_server("srv-2", Some("2026-10-07T09:00:00+00:00"))],
    ]);
    let h = provider(&base)
        .find(&NodeId::new("tb-abc"))
        .await
        .unwrap()
        .expect("found");
    assert_eq!(h.provider_id, "nl-ams-1/srv-2");
    assert_eq!(
        seen.lock().unwrap().list_params.len(),
        2,
        "a full page is not the last page"
    );
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

/// When the cleanup itself fails the server is still there and billing, so telling the
/// caller "no capacity" (nothing was made) would be a lie that skips the lookup by node tag.
/// The answer is Transient and names both causes; the caller's lookup finds the machine.
#[tokio::test]
async fn a_failed_cleanup_is_transient_and_names_both_causes() {
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
    assert!(err.is_transient() && !err.is_capacity(), "{err}");
    let text = err.to_string();
    assert!(text.contains("out_of_stock"), "the create's own cause: {text}");
    assert!(text.contains("500"), "and the cleanup's: {text}");
    assert!(
        text.contains("11111111-2222-3333-4444-555555555555"),
        "and which machine may remain: {text}"
    );
    assert!(!text.contains("SCW-TEST-SECRET"), "{text}");
}

// ─── a provider body that echoes the request must not carry its secret out ───────────────────

/// What a test boot's boot token looks like: 64 lowercase hex characters.
const ECHOED_TOKEN: &str = "5ec2e7a3b19d40f68c1a7e30d5b4f2896a0c3e71d4b85f29a6c07e13b8d94f50";

/// Collects every event logged on this thread, fields included.
struct CapturedLogs(Arc<Mutex<String>>);

impl tracing::Subscriber for CapturedLogs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields<'a>(&'a mut String);
        impl tracing::field::Visit for Fields<'_> {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                let _ = write!(self.0, "{}={:?} ", f.name(), v);
            }
        }
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        let mut all = self.0.lock().unwrap();
        all.push_str(&line);
        all.push('\n');
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn echoing_body() -> String {
    json!({ "type": "invalid_arguments",
            "message": format!("cloud-init rejected: MM_REPORT_TOKEN={ECHOED_TOKEN}") })
    .to_string()
}

/// `create` made the server, the cloud-init PATCH failed and the provider's error body echoes
/// the cloud-init (a test boot's carries its boot token). The server is deleted again and the
/// create's cause is logged by `discard_unbooted`: neither the returned error nor any log line
/// may carry the token.
#[tokio::test]
async fn a_failing_body_that_echoes_the_boot_token_reaches_neither_the_error_nor_the_log() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_state = Some("stopped".into());
        s.user_data_status = Some(400);
        s.user_data_body = Some(echoing_body());
    }
    let logs = Arc::new(Mutex::new(String::new()));
    let _logging = tracing::subscriber::set_default(CapturedLogs(logs.clone()));

    let err = provider(&base).create(&spec()).await.expect_err("cloud-init write failed");

    let text = err.to_string();
    assert!(text.contains("[redacted]"), "{text}");
    assert!(!text.contains(ECHOED_TOKEN), "the error: {text}");
    let logged = logs.lock().unwrap().clone();
    assert!(
        logged.contains("create failed after the server existed; deleted it again"),
        "the line under test was logged: {logged}"
    );
    assert!(logged.contains("[redacted]"), "{logged}");
    assert!(!logged.contains(ECHOED_TOKEN), "the log: {logged}");
    assert_eq!(seen.lock().unwrap().deleted_servers.len(), 1, "the server was discarded");
}

/// The same body when the cleanup fails too: both causes are named in the error and logged.
#[tokio::test]
async fn a_failed_cleanup_after_an_echoing_body_leaks_the_token_nowhere() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_state = Some("stopped".into());
        s.user_data_status = Some(400);
        s.user_data_body = Some(echoing_body());
        s.delete_server_status = Some(500);
    }
    let logs = Arc::new(Mutex::new(String::new()));
    let _logging = tracing::subscriber::set_default(CapturedLogs(logs.clone()));

    let err = provider(&base).create(&spec()).await.expect_err("cloud-init write failed");

    let text = err.to_string();
    assert!(err.is_transient() && text.contains("[redacted]"), "{text}");
    assert!(!text.contains(ECHOED_TOKEN), "the error: {text}");
    let logged = logs.lock().unwrap().clone();
    assert!(logged.contains("AND deleting it failed"), "{logged}");
    assert!(!logged.contains(ECHOED_TOKEN), "the log: {logged}");
}

/// Any failing body is cut to 400 characters, after the redaction.
#[tokio::test]
async fn a_long_failing_body_is_cut_and_a_token_across_the_cut_is_still_redacted() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().create_failure = Some((
        400,
        json!({ "type": "invalid_arguments",
                "message": format!("{}{ECHOED_TOKEN}{}", "x".repeat(350), "y".repeat(500)) }),
    ));
    let err = provider(&base).create(&spec()).await.expect_err("create refused");
    let text = err.to_string();
    assert!(!text.contains(ECHOED_TOKEN) && !text.contains(&ECHOED_TOKEN[..16]), "{text}");
    assert!(text.contains("[redacted]"), "{text}");
}

/// A 2xx means the server was made. A body that is not a server leaves the caller with no
/// handle for a machine that exists, so it must be Transient (the caller looks it up by node
/// tag); Permanent would skip the lookup and strand a billing machine.
#[tokio::test]
async fn a_2xx_create_whose_body_is_not_a_server_is_transient() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().create_body = Some("<html>gateway hiccup</html>".into());

    let err = provider(&base).create(&spec()).await.expect_err("unparsable answer");
    assert!(err.is_transient(), "the server may exist: {err}");
    assert!(err.to_string().contains("not a server"), "{err}");
    assert_eq!(seen.lock().unwrap().created.len(), 1);
}

/// The same when the body cannot even be read: the connection ends after a 201's headers.
#[tokio::test]
async fn a_2xx_create_whose_body_cannot_be_read_is_transient() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.expect("accept");
        // Read the request through its body so the client is not reset mid-send.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = conn.read(&mut chunk).await.expect("read");
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_lowercase();
            if let Some(head_end) = text.find("\r\n\r\n") {
                let wanted = text
                    .split("content-length:")
                    .nth(1)
                    .and_then(|r| r.split("\r\n").next())
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= head_end + 4 + wanted || n == 0 {
                    break;
                }
            }
        }
        // Promises 500 bytes, sends 10, hangs up.
        conn.write_all(b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 500\r\n\r\n{\"server\":")
            .await
            .expect("write");
        conn.shutdown().await.ok();
    });

    let err = provider(&base).create(&spec()).await.expect_err("unreadable answer");
    assert!(err.is_transient(), "the server may exist: {err}");
    assert!(err.to_string().contains("could not be read"), "{err}");
}

// ─── a call that was sent and never answered ─────────────────────────────────
//
// The real client gives up after 60 s, long before the renter's own 10-minute timer, so the
// adapter must say "may have landed" itself: a plain Transient would send the renter back
// for a second create beside a server that is still being made.

fn impatient(base: &str) -> ScalewayProvider {
    provider(base).with_request_timeout(std::time::Duration::from_millis(150))
}

#[tokio::test]
async fn a_create_that_is_accepted_and_never_answered_is_a_timeout() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().hang_requests = true;

    let err = impatient(&base).create(&spec()).await.expect_err("no answer");
    assert!(matches!(err, ProviderError::Timeout(_)), "{err}");
    assert!(err.is_transient() && !err.needs_human(), "retryable, never a page: {err}");
    assert!(err.to_string().contains("create request failed"), "{err}");
}

/// A lookup or a listing that timed out is an error. An empty answer would read as "no such
/// machine" and let the caller send the create again.
#[tokio::test]
async fn a_lookup_or_listing_that_times_out_is_an_error_never_an_empty_answer() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().hang_requests = true;
    let p = impatient(&base);

    let found = p.find(&NodeId::new("bc-b1-fanout-0")).await;
    assert!(matches!(found, Err(ProviderError::Timeout(_))), "{found:?}");
    let listed = p.list().await;
    assert!(matches!(listed, Err(ProviderError::Timeout(_))), "{listed:?}");
}

/// A connection that never opened sent nothing, so there is nothing that may have landed.
#[tokio::test]
async fn a_refused_connection_is_transient_not_a_timeout() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    drop(listener);

    let err = impatient(&base).create(&spec()).await.expect_err("refused");
    assert!(matches!(err, ProviderError::Transient(_)), "{err}");
}

/// A poweron that times out after the server was made is cleaned up like any other failed
/// poweron. The server is confirmed gone, so the create is settled: reporting it as a timeout
/// would send the renter looking for a machine that was just removed.
#[tokio::test]
async fn a_poweron_that_times_out_is_cleaned_up_and_reported_as_transient() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_state = Some("stopped".into());
        s.hang_action = Some("poweron".into());
    }

    let err = gpu_provider(&base)
        .with_request_timeout(std::time::Duration::from_millis(150))
        .create(&transcode_spec())
        .await
        .expect_err("poweron never answered");

    assert!(
        matches!(err, ProviderError::Transient(_)),
        "settled, so not a timeout: {err}"
    );
    assert_eq!(
        seen.lock().unwrap().deleted_servers,
        vec!["11111111-2222-3333-4444-555555555555"]
    );
}

// ─── volumes that outlive their server ───────────────────────────────────────
//
// `terminate` only detaches an SBS volume, and a destroy can fail AFTER the server
// is gone and BEFORE its volume is deleted. A retry then finds a 404 and no record
// of the volume. So volumes are tagged at create — `mm-server=<uuid>` — and found
// again by tag.

/// Tagged before anything else can fail, so every later failure is recoverable.
#[tokio::test]
async fn create_tags_the_root_volume_before_anything_else() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().create_volumes =
        json!({ "0": { "id": "vol-root", "volume_type": "sbs_volume" } });

    gpu_provider(&base).create(&transcode_spec()).await.expect("create");

    let s = seen.lock().unwrap();
    assert_eq!(s.volume_patches.len(), 1);
    let (id, body) = &s.volume_patches[0];
    assert_eq!(id, "vol-root");
    let tags: Vec<&str> = body["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
    assert!(tags.contains(&"mm-fleet"), "{tags:?}");
    assert!(tags.contains(&"mm-node-id=bc-b1-transcode-0"), "{tags:?}");
    assert!(tags.contains(&"mm-server=11111111-2222-3333-4444-555555555555"), "{tags:?}");
    let tagged = s.events.iter().position(|e| e == "tag-volume:vol-root").unwrap();
    let cloud_init = s.events.iter().position(|e| e == "user-data").unwrap();
    assert!(tagged < cloud_init, "tagged before anything else can fail: {:?}", s.events);
}

/// An untagged volume is exactly the leak the tag exists to prevent, so a node
/// whose volume could not be tagged is not handed out.
#[tokio::test]
async fn a_volume_that_cannot_be_tagged_discards_the_server() {
    let (base, seen) = fake_scaleway(json!({ "0": { "id": "vol-root", "volume_type": "sbs_volume" } })).await;
    {
        let mut s = seen.lock().unwrap();
        s.create_volumes = json!({ "0": { "id": "vol-root", "volume_type": "sbs_volume" } });
        s.server_state = Some("stopped".into());
        s.volume_patch_status = Some(400);
    }

    gpu_provider(&base).create(&transcode_spec()).await.expect_err("untaggable volume");
    let s = seen.lock().unwrap();
    assert!(s.actions.is_empty(), "never powered on: {:?}", s.actions);
    assert_eq!(s.deleted_servers, vec!["11111111-2222-3333-4444-555555555555"]);
    assert_eq!(s.deleted_volumes, vec!["block:vol-root"]);
}

/// THE AMNESIA CASE. The server is already gone (a previous attempt removed it and
/// then failed on the volume). The retry must still find and delete the volume.
#[tokio::test]
async fn destroying_a_gone_server_still_deletes_its_tagged_volumes() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_gone = true;
        s.block_volumes = vec![our_volume("vol-left", "srv-1", false)];
    }

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");

    let s = seen.lock().unwrap();
    assert_eq!(s.deleted_volumes, vec!["block:vol-left"]);
    assert!(
        s.volume_list_params[0].contains(&("tags".into(), "mm-server=srv-1".into())),
        "looked up by the server's own tag: {:?}",
        s.volume_list_params
    );
    assert!(s.volume_list_params[0].contains(&("project_id".into(), "proj-1".into())));
}

/// A volume seen on the server AND by tag is deleted once.
#[tokio::test]
async fn a_volume_found_both_ways_is_deleted_once() {
    let (base, seen) = fake_scaleway(json!({ "0": { "id": "vol-root", "volume_type": "sbs_volume" } })).await;
    seen.lock().unwrap().block_volumes = vec![our_volume("vol-root", "srv-1", false)];

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");
    assert_eq!(seen.lock().unwrap().deleted_volumes, vec!["block:vol-root"]);
}

/// If the tag lookup fails, the server may be gone already — Ok would forget the
/// volume for good. Transient, so the retry looks again.
#[tokio::test]
async fn a_failed_tag_lookup_is_transient_not_ok() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_gone = true;
        s.volume_list_status = Some(503);
    }
    let err = provider(&base).destroy("nl-ams-1/srv-1").await.expect_err("lookup failed");
    assert!(err.is_transient(), "{err}");
}

/// THE SWEEP. A detached volume of ours whose server no longer exists is reported
/// by `list()` under that server's id, so the orphan sweeper's `destroy` finishes
/// the job — no trait change, no second sweeper.
#[tokio::test]
async fn list_reports_the_server_id_of_a_stranded_volume() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.list_pages = Some(vec![vec![our_server("srv-live", "running")]]);
        // The fake's GET is not per-id; only the stranded candidate is fetched.
        s.server_gone = true;
        s.block_volumes = vec![
            our_volume("vol-live", "srv-live", true),
            our_volume("vol-stranded", "srv-gone", false),
        ];
    }

    let listed = provider(&base).list().await.expect("list");
    let mut ids: Vec<&str> = listed.iter().map(|h| h.provider_id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["nl-ams-1/srv-gone", "nl-ams-1/srv-live"]);
}

/// An attached volume belongs to a live server — reporting its server as gone
/// would hand a running node to the orphan sweeper.
#[tokio::test]
async fn an_attached_volume_never_produces_a_handle() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.list_pages = Some(vec![vec![]]);
        // Attached to a server the list did not show (created after we listed).
        s.block_volumes = vec![our_volume("vol-new", "srv-just-created", true)];
    }
    let listed = provider(&base).list().await.expect("list");
    assert!(listed.is_empty(), "{listed:?}");
}

#[tokio::test]
async fn a_stranded_volume_in_another_project_is_not_ours() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.list_pages = Some(vec![vec![]]);
        let mut v = our_volume("vol-stage", "srv-stage", false);
        v["project_id"] = json!("proj-STAGE");
        s.block_volumes = vec![v];
    }
    let listed = provider(&base).list().await.expect("list");
    assert!(listed.is_empty(), "{listed:?}");
    assert!(seen.lock().unwrap().volume_list_params[0].contains(&("project_id".into(), "proj-1".into())));
}

/// A volume list that fails is a list that fails — never the servers alone.
#[tokio::test]
async fn a_failed_volume_list_fails_the_whole_list() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().volume_list_status = Some(503);
    let err = provider(&base).list().await.expect_err("volume list failed");
    assert!(err.is_transient(), "{err}");
}

#[tokio::test]
async fn the_volume_list_follows_every_page() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.list_pages = Some(vec![vec![]]);
        s.server_gone = true;
        s.block_volumes = (0..150).map(|i| our_volume(&format!("vol-{i}"), &format!("srv-{i}"), false)).collect();
    }
    let listed = provider(&base).list().await.expect("list");
    assert_eq!(listed.len(), 150);
}

// ─── review round 2: the stranded-volume sweep must never name a live server ──

/// Volumes FIRST, servers second: a tagged volume exists only after its server's
/// create returned, so that server is in any server list read afterwards.
#[tokio::test]
async fn list_reads_volumes_before_servers() {
    let (base, seen) = fake_scaleway(json!({})).await;
    seen.lock().unwrap().list_pages = Some(vec![vec![]]);
    provider(&base).list().await.expect("list");

    let events = seen.lock().unwrap().events.clone();
    let vols = events.iter().position(|e| e == "list-volumes").unwrap();
    let srvs = events.iter().position(|e| e == "list-servers").unwrap();
    assert!(vols < srvs, "{events:?}");
}

/// A volume with no references is not proof of detachment while it is still
/// being created — references settle asynchronously.
#[tokio::test]
async fn only_an_available_volume_can_be_stranded() {
    for status in ["creating", "in_use", "error", "updating"] {
        let (base, seen) = fake_scaleway(json!({})).await;
        {
            let mut s = seen.lock().unwrap();
            s.list_pages = Some(vec![vec![]]);
            s.server_gone = true;
            let mut v = our_volume("vol-x", "srv-x", false);
            v["status"] = json!(status);
            s.block_volumes = vec![v];
        }
        let listed = provider(&base).list().await.expect("list");
        assert!(listed.is_empty(), "{status}: {listed:?}");
    }
}

/// And the last word belongs to the server itself: if it still answers, it is
/// not gone, whatever the lists said.
#[tokio::test]
async fn a_stranded_candidate_whose_server_still_answers_is_not_reported() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.list_pages = Some(vec![vec![]]); // the list missed it
        s.block_volumes = vec![our_volume("vol-x", "srv-x", false)];
        // server_gone stays false: GET finds it running
    }
    let listed = provider(&base).list().await.expect("list");
    assert!(listed.is_empty(), "{listed:?}");
}

/// Both client-side filters, with a server-side filter that does nothing.
#[tokio::test]
async fn stranded_volumes_are_filtered_client_side_too() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.list_pages = Some(vec![vec![]]);
        s.server_gone = true;
        s.volume_list_ignores_filters = true;
        let mut stage = our_volume("vol-stage", "srv-stage", false);
        stage["project_id"] = json!("proj-STAGE");
        let mut foreign = our_volume("vol-foreign", "srv-foreign", false);
        foreign["tags"] = json!(["someone-else", "mm-server=srv-foreign"]);
        s.block_volumes = vec![stage, foreign];
    }
    let listed = provider(&base).list().await.expect("list");
    assert!(listed.is_empty(), "{listed:?}");
}

/// Before a server is removed, its block volumes must carry the server tag — an
/// untagged volume of a gone server is unfindable. Existing tags are kept.
#[tokio::test]
async fn destroy_tags_untagged_volumes_before_removing_the_server() {
    let (base, seen) = fake_scaleway(json!({ "0": { "id": "vol-root", "volume_type": "sbs_volume" } })).await;
    {
        let mut s = seen.lock().unwrap();
        let mut v = our_volume("vol-root", "srv-1", true);
        v["tags"] = json!(["terraform-made"]);
        s.block_volumes = vec![v];
    }

    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");

    let s = seen.lock().unwrap();
    let tagged = s.events.iter().position(|e| e == "tag-volume:vol-root").expect("tagged");
    let removed = s.events.iter().position(|e| e == "action:terminate").expect("terminated");
    assert!(tagged < removed, "{:?}", s.events);
    let tags = s.volume_patches[0].1["tags"].as_array().unwrap().clone();
    assert!(tags.contains(&json!("terraform-made")), "existing tags kept: {tags:?}");
    assert!(tags.contains(&json!("mm-server=srv-1")), "{tags:?}");
    assert!(tags.contains(&json!("mm-fleet")), "{tags:?}");
}

/// If the tag cannot be written, the server stays: it is still findable by its
/// own tag, and the volume would not be once the server is gone.
#[tokio::test]
async fn destroy_leaves_the_server_when_its_volumes_cannot_be_tagged() {
    let (base, seen) = fake_scaleway(json!({ "0": { "id": "vol-root", "volume_type": "sbs_volume" } })).await;
    {
        let mut s = seen.lock().unwrap();
        let mut v = our_volume("vol-root", "srv-1", true);
        v["tags"] = json!([]);
        s.block_volumes = vec![v];
        s.volume_patch_status = Some(503);
    }

    let err = provider(&base).destroy("nl-ams-1/srv-1").await.expect_err("untaggable");
    assert!(err.is_transient(), "{err}");
    let s = seen.lock().unwrap();
    assert!(s.actions.is_empty() && s.deleted_servers.is_empty(), "{:?}", s.events);
}

/// A failed lookup must not cost the volumes this call already knows about.
#[tokio::test]
async fn a_failed_tag_lookup_still_deletes_the_volumes_it_saw() {
    let (base, seen) = fake_scaleway(json!({ "0": { "id": "vol-root", "volume_type": "sbs_volume" } })).await;
    {
        let mut s = seen.lock().unwrap();
        s.block_volumes = vec![our_volume("vol-root", "srv-1", true)];
        s.volume_list_status = Some(503);
    }

    let err = provider(&base).destroy("nl-ams-1/srv-1").await.expect_err("lookup failed");
    assert!(err.is_transient(), "{err}");
    assert_eq!(seen.lock().unwrap().deleted_volumes, vec!["block:vol-root"]);
}

/// Deletion is asynchronous too: a 2xx is "queued". Only a 404 is "gone".
#[tokio::test]
async fn a_volume_whose_deletion_fails_afterwards_is_not_reported_gone() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_gone = true;
        s.block_volumes = vec![our_volume("vol-left", "srv-1", false)];
        s.volume_delete_leaves = Some("error".into());
    }
    let err = provider(&base).destroy("nl-ams-1/srv-1").await.expect_err("deletion failed");
    assert!(err.to_string().contains("billing"), "{err}");
}

/// A volume already `deleting` is still driven to a 404, not assumed gone.
#[tokio::test]
async fn a_volume_already_deleting_is_still_driven_to_gone() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.server_gone = true;
        let mut v = our_volume("vol-left", "srv-1", false);
        v["status"] = json!("deleting");
        s.block_volumes = vec![v];
    }
    provider(&base).destroy("nl-ams-1/srv-1").await.expect("destroy");
    assert_eq!(seen.lock().unwrap().deleted_volumes, vec!["block:vol-left"]);
}

/// A volume still being created refuses its tag for a moment; that is waited out.
#[tokio::test]
async fn a_volume_still_being_created_is_tagged_once_it_settles() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        s.create_volumes = json!({ "0": { "id": "vol-root", "volume_type": "sbs_volume" } });
        s.volume_patch_in_use_times = 2;
    }
    gpu_provider(&base).create(&transcode_spec()).await.expect("create");
    assert_eq!(seen.lock().unwrap().volume_patches.len(), 3);
}

// ─── creation times, for the orphan sweeper's grace ──────────────────────────

/// The sweeper spares anything younger than its grace — so every handle must say
/// when its instance was created, or it is spared forever.
#[tokio::test]
async fn list_reports_when_each_instance_was_created() {
    let (base, seen) = fake_scaleway(json!({})).await;
    {
        let mut s = seen.lock().unwrap();
        let mut live = our_server("srv-live", "running");
        live["creation_date"] = json!("2026-10-04T10:00:00.123456+00:00");
        s.list_pages = Some(vec![vec![live]]);
        s.server_gone = true;
        let mut stranded = our_volume("vol-x", "srv-gone", false);
        stranded["created_at"] = json!("2026-10-01T08:30:00Z");
        s.block_volumes = vec![stranded];
    }

    let listed = provider(&base).list().await.expect("list");
    let when = |id: &str| {
        listed
            .iter()
            .find(|h| h.provider_id == id)
            .and_then(|h| h.created_at)
            .map(|t| t.to_rfc3339())
    };
    assert_eq!(when("nl-ams-1/srv-live").as_deref(), Some("2026-10-04T10:00:00.123456+00:00"));
    assert_eq!(
        when("nl-ams-1/srv-gone").as_deref(),
        Some("2026-10-01T08:30:00+00:00"),
        "a stranded volume's handle carries the volume's age"
    );
}

#[tokio::test]
async fn create_reports_when_the_instance_was_created() {
    let (base, _seen) = fake_scaleway(json!({})).await;
    let handle = provider(&base).create(&spec()).await.expect("create");
    assert!(handle.created_at.is_some(), "the create response carries creation_date");
}
