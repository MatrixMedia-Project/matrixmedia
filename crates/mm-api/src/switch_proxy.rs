//! The S1 viewer proxy (FR-346, FR-350).
//!
//! mm-core exposes the mm-switch viewer API beneath a prefix of its own and
//! forwards each call to the node holding that viewer. What this buys is the
//! whole reason the fleet can ship at all: **the apps already in both stores
//! reach a multi-node fleet with no client update**, because they use whatever
//! `switch_url` the join response gives them and concatenate the same sub-paths
//! (`{switch_url}/api/viewers/offer`). Verified: no shipped SDK hardcodes the
//! prefix — only the dashboard's dev lab does, as an editable default.
//!
//! ## Three things this fixes rather than merely moves
//!
//! **Identity.** On the direct path the client presents a `viewer` token minted at
//! join time and the switch enforces `body.id == sub` (FR-347). Here the caller is
//! authenticated as a **Matrix user** by mm-core's own extractor, and the viewer id
//! is derived server-side from that — the client cannot name a viewer id at all.
//! The body's `id` is **ignored**, not merely checked.
//!
//! **The token the proxy presents.** mm-core mints a **per-request `viewer`-role**
//! token whose `sub` is the derived id (FR-347b). It must NOT reuse its
//! `server`-role token: the switch exempts `server` from the binding, precisely
//! because that token names mm-core rather than a resource, so reusing it would
//! take the exemption and reopen the hole.
//!
//! **The audience list.** `GET /api/viewers` on the direct path was open to the
//! internet and leaked which Matrix users were watching which stream (FR-349b).
//! Here it is a Matrix-authenticated, per-stream count: no ids leave the server.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use mm_core::error::{ErrorCode, ErrorResponse, MMError};
use mm_core::types::StreamId;

use crate::error::ApiError;
use crate::middleware::AuthUser;
use crate::state::SharedState;

/// Token lifetime for one proxied offer. Short because it is used immediately and
/// once — the offer is forwarded within the same request.
const VIEWER_TOKEN_TTL_SECS: u64 = 60;

/// Is the S1 proxy (FR-346) in effect? Its flag AND the switch auth secret: the
/// proxy mints a per-request viewer token, and forwarding an UNBOUND offer would
/// place a viewer on a node with no identity at all. Read from the config the
/// caller holds for this request, so it always agrees with what is running.
pub fn proxy_enabled(cfg: &mm_core::config::Config) -> bool {
    cfg.fleet.proxy_viewers && cfg.advertising.switch_auth_secret_opt().is_some()
}

/// What `switch_url` the join response should hand a client.
///
/// FR-346: with the proxy on, that is mm-core itself, and the client's own
/// concatenation (`{switch_url}/api/viewers/offer`) lands on the proxy routes —
/// which is what lets a multi-node fleet serve the apps already in both stores
/// with no client update.
///
/// The proxy form carries the stream id because a viewer id is per-stream and
/// mm-core must know which broadcast to place the viewer on. The direct form does
/// not, because on that path the client already holds a per-stream token.
pub fn switch_base_url(public_url: &str, stream_id: &str, proxy_viewers: bool) -> String {
    if proxy_viewers {
        format!("{public_url}/_mm/fleet/v1/streams/{stream_id}")
    } else {
        format!("{public_url}/_mm/switch")
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ProxyOfferRequest {
    /// The viewer's SDP offer, forwarded verbatim.
    pub offer: serde_json::Value,
    /// Which source to attach to immediately. Optional, as on the direct path.
    #[serde(default)]
    pub source_id: Option<String>,
    /// Accepted and **ignored**. Present only so a client that sends the field it
    /// sends on the direct path is not rejected; the viewer id is derived from the
    /// authenticated Matrix user, never taken from the body.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProxyOfferResponse {
    /// The viewer id mm-core assigned. Echoed so a client can keep using it.
    pub id: String,
    /// The node's SDP answer, forwarded verbatim.
    pub answer: serde_json::Value,
}

/// POST /_mm/fleet/v1/streams/{id}/api/viewers/offer
///
/// The stream id is in the path rather than inferred, because a viewer id is
/// per-stream and mm-core must know which broadcast to place the viewer on.
#[utoipa::path(
    post,
    path = "/streams/{id}/api/viewers/offer",
    tag = "switch-proxy",
    params(("id" = String, Path, description = "Stream id")),
    request_body = ProxyOfferRequest,
    responses(
        (status = 200, description = "The node's SDP answer", body = ProxyOfferResponse),
        (status = 401, description = "Unauthenticated", body = ErrorResponse),
        (status = 404, description = "Stream not found, or the caller is neither its host nor in its room", body = ErrorResponse),
        (status = 501, description = "Switch or auth secret not configured", body = ErrorResponse),
        (status = 503, description = "The node rejected or could not answer the offer", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn proxy_viewer_offer(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<ProxyOfferRequest>,
) -> Result<Json<ProxyOfferResponse>, ApiError> {
    let stream_id = StreamId(id.clone());
    // Live streams are members-only however they are watched: the same gate
    // as `/join`, so the proxy is not a way around it. (This is membership
    // only; `/join`'s tier and content gates are not applied here yet.)
    let stream = crate::client::visible_stream_or_404(&state, &id, &auth.user_id).await?;
    if stream.status == "ended" {
        return Err(MMError::api(ErrorCode::StreamEnded, "stream has ended").into());
    }

    let cfg = state.config();
    let (Some(pool), Some(secret)) = (
        state.switch_pool.clone(),
        cfg.advertising.switch_auth_secret_opt().map(str::to_owned),
    ) else {
        // Without a secret the proxy cannot mint a bound token, and forwarding
        // unbound would be worse than refusing: it would place a viewer on a node
        // with no identity at all.
        return Err(MMError::api(
            ErrorCode::FeatureDisabled,
            "switch proxy requires MM_SWITCH_URL and MM_SWITCH_AUTH_SECRET",
        )
        .into());
    };

    // Derived, never taken from the body. `req.id` is accepted and dropped.
    let viewer_id = mm_core::switch_client::switch_viewer_id(&stream.id, &auth.user_id.0);
    let source_id = req
        .source_id
        .unwrap_or_else(|| mm_core::switch_client::switch_source_id(&stream.id));

    let viewer = mm_core::fleet::ViewerId::new(&viewer_id);

    // STICKY. A viewer id is deterministic, so a reconnect or a renegotiation
    // arrives with the same one — and sending it to whichever node is least loaded
    // *now* would register the same viewer on a second node while the first keeps
    // its stale entry until that node is destroyed. Worse, the ad binding would
    // move while the viewer's existing PeerConnection did not.
    //
    // So: reuse the node this viewer is already on, and pick a new one only when
    // they have none. Deliberately moving a viewer between nodes is
    // make-before-break (FR-322), an explicit mechanism, not a side effect of
    // re-offering.
    let existing = match pool.node_of_viewer(&viewer).await {
        Some(node) => pool
            .client_for_node(Some(&node))
            .await
            .map(|client| (Some(node), client)),
        None => None,
    };

    let (node_id, client) = match existing {
        Some(pair) => pair,
        None => match pool.assign_fanout_node(&stream_id).await {
            Some((node_id, client)) => (Some(node_id), client),
            None => (None, pool.origin()),
        },
    };

    // Bind before forwarding, so an ad switch racing this request resolves to the
    // same node (FR-405) instead of falling back to the origin.
    match node_id.clone() {
        Some(node) => pool.bind_viewer(viewer.clone(), node).await,
        // Unbound means "on the origin", which `route_viewer` already reads
        // correctly; binding a synthetic origin id would make a real node
        // indistinguishable from none.
        None => pool.unbind_viewer(&viewer).await,
    }

    // FR-347b: a per-request VIEWER-role token whose sub is the derived id. Not
    // the server token — that one is exempt from the binding.
    let token = mm_core::switch_auth::generate_switch_token(
        &secret,
        "viewer",
        &viewer_id,
        VIEWER_TOKEN_TTL_SECS,
    );

    let answer = client
        .viewer_offer(&viewer_id, &token, &req.offer, Some(&source_id))
        .await
        .map_err(|e| {
            // The binding rejecting us is our bug, not the caller's, and it must
            // not be reported as a client error.
            tracing::error!(
                viewer = %viewer_id,
                node = ?node_id,
                error = %e,
                "proxied viewer offer failed"
            );
            MMError::Sfu(format!("switch node rejected the offer: {e}"))
        })?;

    Ok(Json(ProxyOfferResponse {
        id: viewer_id,
        answer,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ViewerCountResponse {
    pub stream_id: String,
    /// Connected viewers on this stream, summed across the origin and every
    /// fan-out node. **A count, not a list** — the ids embed Matrix user ids, and
    /// publishing them is what FR-349b closed.
    pub viewers: u32,
}

/// GET /_mm/fleet/v1/streams/{id}/api/viewers
///
/// Replaces the direct path's open `GET /api/viewers`, which returned every
/// viewer id on the node to anyone who could reach it.
///
/// Members-only like the stream itself: a caller who neither hosts it nor has
/// joined its room gets what an unknown stream gets, a count of 0.
#[utoipa::path(
    get,
    path = "/streams/{id}/api/viewers",
    tag = "switch-proxy",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Connected viewer count", body = ViewerCountResponse),
        (status = 401, description = "Unauthenticated", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn proxy_viewer_count(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<ViewerCountResponse>, ApiError> {
    let none = |id: String| Json(ViewerCountResponse { stream_id: id, viewers: 0 });
    let Some(pool) = state.switch_pool.clone() else {
        return Ok(none(id));
    };
    let visible = match state.db.get_stream(&StreamId(id.clone())).await? {
        Some(stream) => {
            let cfg = state.config();
            crate::client::stream_visible_to(
                state.db.as_ref(),
                mm_core::http::shared(),
                &cfg.matrix.homeserver_url,
                &cfg.matrix.synapse_admin_token,
                &stream,
                &auth.user_id,
            )
            .await?
        }
        None => false,
    };
    if !visible {
        return Ok(none(id));
    }

    let want = mm_core::switch_client::switch_source_id(&id);
    let mut clients = vec![pool.origin()];
    for node in pool.nodes().await {
        if let Some(c) = pool.client_for_node(Some(&node.id)).await {
            clients.push(c);
        }
    }

    let mut viewers: u32 = 0;
    for client in clients {
        match client.list_viewers().await {
            Ok(list) => {
                viewers += list
                    .iter()
                    .filter(|v| v.connected)
                    // Mid ad-break still counts, as the shipped client counts it.
                    .filter(|v| v.current_source == want || v.current_source.starts_with("ad-"))
                    .count() as u32;
            }
            Err(e) => tracing::warn!(
                node = client.base_url(),
                error = %e,
                "viewer count: a node did not answer, counting it as zero"
            ),
        }
    }

    Ok(Json(ViewerCountResponse {
        stream_id: id,
        viewers,
    }))
}

fn api_router() -> utoipa_axum::router::OpenApiRouter<SharedState> {
    use utoipa_axum::router::OpenApiRouter;
    use utoipa_axum::routes;

    OpenApiRouter::new()
        .routes(routes!(proxy_viewer_offer))
        .routes(routes!(proxy_viewer_count))
}

pub fn routes(state: SharedState) -> axum::Router {
    api_router().with_state(state).into()
}

/// The generated paths, so `openapi.rs` can nest them and its test can assert
/// that the mounted prefix and the annotated path agree. Getting that wrong is
/// silent: the route exists somewhere the clients do not look.
pub fn openapi_fragment() -> utoipa::openapi::OpenApi {
    api_router().split_for_parts().1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The proxy derives the viewer id through mm-core's one definition, and that
    /// id is what the shipped SDKs were handed at join. Pinned here as well as in
    /// switch_client.rs because the proxy is where a divergence would be invisible:
    /// the switch simply does not know the viewer, ad switches no-op, and
    /// impressions are billed for ads nobody saw.
    #[test]
    fn the_proxy_uses_the_viewer_id_the_clients_were_given() {
        assert_eq!(
            mm_core::switch_client::switch_viewer_id("abc123", "@alice:example.org"),
            "viewer-abc123--alice-example.org"
        );
        assert_eq!(
            mm_core::switch_client::switch_viewer_id("s", "@a!b:c"),
            "viewer-s--a-b-c",
            "the replace set is [':', '@', '!']"
        );
    }

    /// FR-346 needs a secret: the proxy mints a per-request viewer token, and an
    /// UNBOUND offer would place a viewer on a node with no identity. So the flag
    /// alone does not enable it — and the rule is applied where the join reads the
    /// config, since startup holds only a copy of it.
    #[test]
    fn the_proxy_is_enabled_only_with_its_flag_and_a_secret() {
        let mut cfg = mm_core::config::Config::default();
        cfg.fleet.proxy_viewers = true;
        cfg.advertising.switch_auth_secret = String::new();
        assert!(!proxy_enabled(&cfg), "no secret: the proxy must stay off");

        cfg.advertising.switch_auth_secret = "s3cret".into();
        assert!(proxy_enabled(&cfg));

        cfg.fleet.proxy_viewers = false;
        assert!(!proxy_enabled(&cfg), "the flag is off by default and stays authoritative");
    }

    /// The path the shipped clients will actually build. They append
    /// `/api/viewers/offer` to whatever they are given, so the proxy form must
    /// leave the route intact under mm-core's own mount.
    #[test]
    fn the_client_concatenation_lands_on_the_proxy_route() {
        let base = switch_base_url("https://mm.example", "s1", true);
        assert_eq!(base, "https://mm.example/_mm/fleet/v1/streams/s1");
        assert_eq!(
            format!("{base}/api/viewers/offer"),
            "https://mm.example/_mm/fleet/v1/streams/s1/api/viewers/offer",
            "this is the URL a shipped SDK builds; it must match the mounted route"
        );
    }

    /// Default off. This is the code path every viewer join traverses, on a service
    /// with live users in two app stores, so the release must not change it.
    #[test]
    fn the_direct_path_is_what_an_unconfigured_server_still_returns() {
        assert_eq!(
            switch_base_url("https://mm.example", "s1", false),
            "https://mm.example/_mm/switch",
            "with the proxy off, clients must get exactly the URL they get today"
        );
    }

    #[test]
    fn a_token_ttl_short_enough_to_be_single_use() {
        // The token is minted and used inside one request. A long TTL would turn a
        // logged or cached header into a reusable credential for that viewer id.
        assert!(VIEWER_TOKEN_TTL_SECS <= 300);
    }
}
