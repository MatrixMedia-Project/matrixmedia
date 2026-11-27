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

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use mm_core::config::MediaPath;
use mm_core::error::{ErrorCode, ErrorResponse, MMError};
use mm_core::fleet::NodeId;
use mm_core::fleet::media_path::{ViewerTarget, effective, viewer_target};
use mm_core::switch_client::{SwitchClient, SwitchViewer, is_cascade_session};
use mm_core::types::{StreamId, UserId};
use mm_db::models::Stream;

use crate::client::ViewerGate;
use crate::error::ApiError;
use crate::middleware::AuthUser;
use crate::state::SharedState;
use crate::switch_pool::SwitchPool;

/// Token lifetime for one proxied offer. Short because it is used immediately and
/// once — the offer is forwarded within the same request.
const VIEWER_TOKEN_TTL_SECS: u64 = 60;

/// Is the S1 proxy (FR-346) in effect for every stream? Its flag, or a media path that is
/// not `core` (relays are reached only through the proxy, so a relay path turns it on),
/// AND the switch auth secret: the proxy mints a per-request viewer token, and forwarding
/// an UNBOUND offer would place a viewer on a node with no identity at all. Read from the
/// config the caller holds for this request, so it always agrees with what is running.
///
/// This is the server-wide part. A stream that started in a relay state keeps the proxy
/// after the setting moved: see [`proxy_enabled_for`].
pub fn proxy_enabled(cfg: &mm_core::config::Config) -> bool {
    proxy_enabled_for(cfg, MediaPath::Core)
}

/// The switch auth secret is set: not empty and not only whitespace.
fn secret_set(cfg: &mm_core::config::Config) -> bool {
    !cfg.advertising.switch_auth_secret.trim().is_empty()
}

/// Is the proxy on for a stream whose stored media path state is `stored`? The secret is
/// set AND any of: `fleet.proxy_viewers`; the current setting's effective value is not
/// `core`; the stream's stored state's effective value is not `core`. The last keeps a
/// stream that started as `relay_after_threshold` on the proxy (where its relays are)
/// after the operator moved the setting; under `frozen` or `off` both effective values are
/// `core`, so a new viewer gets the origin.
pub fn proxy_enabled_for(cfg: &mm_core::config::Config, stored: MediaPath) -> bool {
    let mode = cfg.fleet.mode;
    secret_set(cfg)
        && (cfg.fleet.proxy_viewers
            || effective(cfg.fleet.media_path, mode) != MediaPath::Core
            || effective(stored, mode) != MediaPath::Core)
}

/// Is a proxied offer served at all? For the stream (`stored`, its stored state, `None`
/// when there is no such stream) by [`proxy_enabled_for`], or because the viewer is already
/// on a relay: whatever the setting and the mode say, a viewer watching on a relay may
/// re-offer there. The kill switch stops new placements; it does not cut people off.
/// Nothing is served without the secret.
pub fn proxy_serves_offer(
    cfg: &mm_core::config::Config,
    stored: Option<MediaPath>,
    on_a_relay: bool,
) -> bool {
    secret_set(cfg) && (on_a_relay || proxy_enabled_for(cfg, stored.unwrap_or(MediaPath::Core)))
}

/// The `switch_url` a viewer's join hands back for a stream stored in `stored`: the proxy
/// form when the proxy is on for that stream ([`proxy_enabled_for`]), else the direct one.
pub fn viewer_switch_url(
    public_url: &str,
    stream_id: &str,
    stored: MediaPath,
    cfg: &mm_core::config::Config,
) -> String {
    switch_base_url(public_url, stream_id, proxy_enabled_for(cfg, stored))
}

/// The refusal for an offer the proxy does not serve.
fn proxy_off() -> ApiError {
    MMError::api(
        ErrorCode::FeatureDisabled,
        "the viewer proxy is off for this stream: it needs MM_SWITCH_AUTH_SECRET and either \
         fleet.proxy_viewers or a relay media path (fleet.media_path now, or the one the \
         stream started in) with fleet.mode on",
    )
    .into()
}

/// [`proxy_serves_offer`] for stream `id`, reading only what it needs: nothing when the
/// proxy is on server-wide or off for want of the secret, then the viewer's binding, then
/// the stream's stored state (one row read; no membership lookup, so a refusal here says
/// nothing about who may watch).
async fn proxy_serves(
    state: &SharedState,
    cfg: &mm_core::config::Config,
    id: &str,
    viewer: Option<&mm_core::fleet::ViewerId>,
) -> Result<bool, ApiError> {
    if !secret_set(cfg) {
        return Ok(false);
    }
    if proxy_enabled(cfg) {
        return Ok(true);
    }
    if let (Some(pool), Some(viewer)) = (state.switch_pool.as_deref(), viewer)
        && on_a_relay(pool, viewer).await
    {
        return Ok(true);
    }
    let stored = state
        .db
        .get_stream(&StreamId(id.to_string()))
        .await?
        .map(|s| MediaPath::parse(&s.media_path).unwrap_or(MediaPath::Core));
    Ok(proxy_serves_offer(cfg, stored, false))
}

/// Is `viewer` bound to a relay the pool still holds? Bindings exist only for relays: a
/// viewer on the origin is unbound.
pub async fn on_a_relay(pool: &SwitchPool, viewer: &mm_core::fleet::ViewerId) -> bool {
    match pool.node_of_viewer(viewer).await {
        Some(node) => pool.client_for_node(Some(&node)).await.is_some(),
        None => false,
    }
}

/// Forward a viewer's offer to where [`place_viewer`] put it, and return the node's SDP
/// answer (the inner `answer` object).
///
/// The viewer is bound before forwarding, so an ad switch racing this request resolves to
/// the same node (FR-405) instead of falling back to the origin; a viewer on the origin is
/// unbound (binding a synthetic origin id would make a real node indistinguishable from
/// none).
///
/// A join never fails because a relay failed. When the relay refuses, cannot be reached,
/// times out or answers with no usable SDP, the viewer's binding is removed and the same
/// offer goes to the origin, once: a viewer whose relay has gone bad belongs on the main
/// server, and a binding left in place would send every retry (the viewer id is
/// deterministic) back to the same relay. If the origin fails too, its error is returned
/// and the binding stays removed.
pub async fn forward_offer(
    pool: &SwitchPool,
    stream: &StreamId,
    viewer: &mm_core::fleet::ViewerId,
    placed: (Option<NodeId>, Arc<SwitchClient>),
    token: &str,
    offer: &serde_json::Value,
    source_id: &str,
) -> Result<serde_json::Value, MMError> {
    let (node_id, client) = placed;
    let Some(node) = node_id else {
        pool.unbind_viewer(viewer).await;
        return offer_to_node(&client, None, viewer, token, offer, source_id).await;
    };
    pool.bind_viewer(viewer.clone(), node.clone()).await;
    match offer_to_node(&client, Some(&node), viewer, token, offer, source_id).await {
        Ok(answer) => Ok(answer),
        Err(e) => {
            pool.unbind_viewer(viewer).await;
            tracing::warn!(
                stream = %stream.0,
                node = %node,
                viewer = %viewer,
                error = %e,
                "proxied viewer offer: the relay failed, sending the viewer to the origin"
            );
            offer_to_node(&pool.origin(), None, viewer, token, offer, source_id).await
        }
    }
}

/// One offer to one switch, its answer checked for a usable SDP. `node` is `None` for the
/// origin; it only names the node in the log.
async fn offer_to_node(
    client: &SwitchClient,
    node: Option<&NodeId>,
    viewer: &mm_core::fleet::ViewerId,
    token: &str,
    offer: &serde_json::Value,
    source_id: &str,
) -> Result<serde_json::Value, MMError> {
    let forwarded = client
        .viewer_offer(viewer.as_str(), token, offer, Some(source_id))
        .await
        .map_err(|e| {
            // The binding rejecting us is our bug, not the caller's, and it must not be
            // reported as a client error.
            tracing::error!(viewer = %viewer, node = ?node, error = %e, "proxied viewer offer failed");
            MMError::Sfu(format!("switch node rejected the offer: {e}"))
        })?;
    proxy_answer_of(&forwarded).map_err(|e| {
        tracing::error!(
            viewer = %viewer,
            node = ?node,
            error = %e,
            "proxied viewer offer returned an unusable answer"
        );
        MMError::Sfu(format!("switch node returned an unusable answer: {e}"))
    })
}

/// Where a new offer of `viewer` on `stream` goes: `Some(node)` for a relay, `None` for
/// the origin.
///
/// A viewer who already has a node keeps it (see the STICKY note in
/// [`proxy_viewer_offer`]) while that node still carries the stream. A binding to a relay
/// that no longer carries it (the viewer dropped, and the sync then removed the stream's
/// source there) is removed and the viewer is placed fresh: that relay would answer the
/// offer with no picture. Otherwise the stream's stored state (`stream_path`, fixed when
/// it started) decides through [`viewer_target`], with the live `fleet.mode` and
/// `fleet.relay_viewer_threshold` and the stream's audience at the last census: `Relay`
/// takes a relay that carries the stream ([`SwitchPool::assign_fanout_node`]) or falls
/// back to the origin; `Core` is the origin. So under state `core` relays are never used
/// for placement, even with `fleet.proxy_viewers` on and relays leased.
pub async fn place_viewer(
    pool: &SwitchPool,
    cfg: &mm_core::config::Config,
    stream: &StreamId,
    stream_path: MediaPath,
    viewer: &mm_core::fleet::ViewerId,
) -> (Option<NodeId>, Arc<SwitchClient>) {
    if let Some(node) = pool.node_of_viewer(viewer).await {
        if pool.carried_by(&node).await.contains(&stream.0)
            && let Some(client) = pool.client_for_node(Some(&node)).await
        {
            return (Some(node), client);
        }
        pool.unbind_viewer(viewer).await;
    }
    let target = viewer_target(
        stream_path,
        cfg.fleet.mode,
        pool.viewers_of(stream).await,
        cfg.fleet.relay_viewer_threshold,
    );
    match target {
        ViewerTarget::Relay => match pool.assign_fanout_node(stream).await {
            Some((node, client)) => (Some(node), client),
            None => (None, pool.origin()),
        },
        ViewerTarget::Core => (None, pool.origin()),
    }
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
    /// Anonymous share-link viewers only: the grant id from their watch link.
    /// Present exactly when no member JWT is sent; ignored otherwise.
    #[serde(default)]
    pub grant_id: Option<String>,
    /// Anonymous share-link viewers only: the switch viewer token their
    /// `public-join` minted. Carried in the body (not the `Authorization`
    /// header) because these callers hold no header credential, and routing
    /// on its presence keeps the member extractor untouched.
    #[serde(default)]
    pub viewer_token: Option<String>,
    /// Anonymous share-link viewers on a priced link only: the bearer access
    /// token from their settled Lightning paywall claim, as at join.
    #[serde(default)]
    pub access_token: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProxyOfferResponse {
    /// The viewer id mm-core assigned. Echoed so a client can keep using it.
    pub id: String,
    /// The node's SDP answer, forwarded verbatim.
    pub answer: serde_json::Value,
}

/// The node replies `{"id": ..., "answer": {"type": "answer", "sdp": ...}}`
/// (mm-switch `handleViewerOffer`), and `viewer_offer` returns that whole
/// body. The proxy response carries its own `id`, so the answer forwarded to
/// clients must be the node's inner `answer` object — forwarding the whole
/// body nests it, and every shipped SDK (`answer["type"]` / `answer["sdp"]`)
/// then reads nulls. A body with no `answer` field is forwarded as-is.
fn unwrap_node_answer(body: serde_json::Value) -> serde_json::Value {
    match body {
        serde_json::Value::Object(mut map) => match map.remove("answer") {
            Some(answer) => answer,
            None => serde_json::Value::Object(map),
        },
        other => other,
    }
}

/// Whether to serve a viewer offer for stream `id` at all, decided before any
/// node is touched. Factored over its dependencies like
/// [`crate::client::authorize_viewer`] so tests can drive it. In order:
///
/// 1. The proxy serves this offer (`proxy_on`, decided by the caller from
///    [`proxy_serves_offer`]: for the stream, or because the viewer is already on a
///    relay). Off, it is the route's 501, before any membership lookup, so it says
///    nothing about who may watch the stream. The route is mounted whatever the flag says: the settings live in
///    the database and can change while mm-core runs, so the flag is read per
///    request rather than when the router is built.
/// 2. `/join`'s viewer gate, the same function: membership, ended, the content
///    and tier gates, capacity. Minting a viewer token is handing out the
///    stream, so the proxy must refuse whoever `/join` refuses, with the same
///    status, or it is a way around the paywall.
/// 3. A `source_id` in the body, if any, is this stream's own. The gate admits
///    the caller to this stream only; another stream's source would carry that
///    stream's media past its gate. (The switch pins it too, FR-347f; this does
///    not depend on the deployed switch having that pin.)
pub async fn admit_offer(
    gate: &ViewerGate<'_>,
    proxy_on: bool,
    id: &str,
    caller: &UserId,
    requested_source: Option<&str>,
) -> Result<Stream, ApiError> {
    if !proxy_on {
        return Err(proxy_off());
    }
    let stream = crate::client::authorize_viewer(gate, id, caller).await?;
    if let Some(requested) = requested_source
        && requested != mm_core::switch_client::switch_source_id(&stream.id)
    {
        return Err(MMError::api(
            ErrorCode::InvalidRequest,
            "source_id must be this stream's source",
        )
        .into());
    }
    Ok(stream)
}

/// The member seat a verified switch viewer token names, if any: the
/// participant (from the member `/join` that minted the token) whose
/// derived viewer id is exactly the token subject. Guest seats
/// (`guest:{grant}:…`) are excluded — those are admitted by the
/// anonymous branch, never here.
pub fn member_seat_user_id(
    participant_user_ids: &[String],
    stream_id: &str,
    token_subject: &str,
) -> Option<String> {
    participant_user_ids
        .iter()
        .find(|user_id| {
            !user_id.starts_with("guest:")
                && mm_core::switch_client::switch_viewer_id(stream_id, user_id) == token_subject
        })
        .cloned()
}

/// Admit a member who presents the switch viewer token their `/join`
/// minted (in `Authorization`, the only credential shipped member
/// clients have for the switch path). The token is HMAC-verified, its
/// subject must name a live member seat on this stream, and the seat's
/// user then passes the ordinary member gate ([`admit_offer`]), so a
/// token alone — without the join that seated its owner — admits no
/// one. Returns `Ok(None)` when the header token is not a member-seat
/// token at all (e.g. a guest token), leaving the caller to try the
/// anonymous branch.
pub async fn authorize_member_token_offer(
    gate: &ViewerGate<'_>,
    id: &str,
    bearer_token: &str,
    secret: &str,
    requested_source: Option<&str>,
) -> Result<Option<(Stream, String)>, ApiError> {
    let invalid = |msg: &str| -> ApiError { MMError::api(ErrorCode::InvalidToken, msg).into() };
    let claims = match mm_core::switch_auth::verify_switch_token(secret, bearer_token, "viewer") {
        Ok(claims) => claims,
        Err(_) => return Ok(None), // not a switch token: not ours to judge
    };
    if !claims
        .sub
        .starts_with(&mm_core::switch_client::switch_viewer_prefix(id))
    {
        return Err(invalid("viewer token is for a different stream"));
    }
    let participants = gate
        .db
        .list_participants(&StreamId(id.to_string()))
        .await?;
    let user_ids: Vec<String> = participants.iter().map(|p| p.user_id.clone()).collect();
    let Some(seat_user) = member_seat_user_id(&user_ids, id, &claims.sub) else {
        return Ok(None); // guest seat or no seat: anonymous branch decides
    };
    let stream = admit_offer(
        gate,
        true,
        id,
        &UserId(seat_user.clone()),
        requested_source,
    )
    .await?;
    Ok(Some((
        stream,
        mm_core::switch_client::switch_viewer_id(id, &seat_user),
    )))
}

/// POST /_mm/fleet/v1/streams/{id}/api/viewers/offer
///
/// Admitted by [`admit_offer`] for members (exactly as `POST
/// /streams/{id}/join` would refuse the caller), and by
/// [`crate::client::authorize_anonymous_offer`] for anonymous share-link
/// viewers: no member JWT, but the body carries the `grant_id` from their
/// watch link and the `viewer_token` their `public-join` minted (plus
/// `access_token` on a priced link). Either credential refuses exactly as
/// its join would, so the proxy is never a way around the paywall.
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
        (status = 400, description = "source_id names a source other than this stream's", body = ErrorResponse),
        (status = 401, description = "No usable credential: neither a member JWT nor a body viewer token matching a live seat for grant_id", body = ErrorResponse),
        (status = 402, description = "Stream is tier-gated and the caller is not entitled", body = ErrorResponse),
        (status = 403, description = "The caller's tier lacks can_join_live, or is below the stream's content gate", body = ErrorResponse),
        (status = 404, description = "Stream not found or not public; the grant is unknown, revoked, or expired; or the member caller is neither the host nor in the room", body = ErrorResponse),
        (status = 409, description = "Every seat is taken and the caller holds none (members only; a guest's seat was taken at join)", body = ErrorResponse),
        (status = 410, description = "Stream already ended", body = ErrorResponse),
        (status = 501, description = "The proxy is off for this stream (it needs the switch auth secret and fleet.proxy_viewers or a relay media path, now or the one the stream started in), or the switch or its auth secret is not configured", body = ErrorResponse),
        (status = 503, description = "The node rejected or could not answer the offer", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn proxy_viewer_offer(
    auth: Option<AuthUser>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<ProxyOfferRequest>,
) -> Result<Json<ProxyOfferResponse>, ApiError> {
    let cfg = state.config();
    let gate = ViewerGate::from_state(&state, &cfg);
    // Members first, exactly as before: a valid JWT takes the member gate and
    // derives the viewer id from the Matrix user. Without one the caller must
    // be an anonymous share-link viewer, whose credential rides in the body.
    // These requests carry no `Authorization` header, so the `None` case costs
    // no Matrix-bearer fallback round-trip inside the extractor.
    let member_jwt = auth.is_some();
    let (stream, viewer_id) = match auth {
        Some(auth) => {
            // Decided for this stream and this viewer before any membership lookup.
            let viewer = mm_core::fleet::ViewerId::new(mm_core::switch_client::switch_viewer_id(
                &id,
                &auth.user_id.0,
            ));
            let stream = admit_offer(
                &gate,
                proxy_serves(&state, &cfg, &id, Some(&viewer)).await?,
                &id,
                &auth.user_id,
                req.source_id.as_deref(),
            )
            .await?;
            // Derived, never taken from the body. `req.id` is accepted and dropped.
            let viewer_id =
                mm_core::switch_client::switch_viewer_id(&stream.id, &auth.user_id.0);
            (stream, viewer_id)
        }
        None => {
            // Without the secret nothing here can work; refuse before any lookup. Whether
            // the proxy serves this stream and viewer is decided once the viewer is known.
            if !secret_set(&cfg) {
                return Err(proxy_off());
            }
            // Shipped member clients hold no MM JWT for the switch path:
            // their /join handed them a switch viewer token, presented
            // in Authorization. Admit it as a member-seat credential
            // before falling to the anonymous branch.
            let bearer = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| {
                    value
                        .strip_prefix("Bearer ")
                        .or_else(|| value.strip_prefix("bearer "))
                });
            if let (Some(token), Some(secret)) = (
                bearer,
                cfg.advertising.switch_auth_secret_opt().map(str::to_owned),
            ) {
                if let Some(pair) = authorize_member_token_offer(
                    &gate,
                    &id,
                    token,
                    &secret,
                    req.source_id.as_deref(),
                )
                .await?
                {
                    pair
                } else if req.grant_id.is_none() {
                    // A credential was presented and it is neither a
                    // member-seat token nor an anonymous grant offer.
                    return Err(MMError::api(
                        ErrorCode::InvalidToken,
                        "viewer token matches no current member seat for this stream",
                    )
                    .into());
                } else {
                    // Guest token (anonymous share-link viewer): its
                    // seat is a guest seat, so authorize anonymously.
                    let (grant_id, viewer_token) =
                        match (req.grant_id.as_deref(), req.viewer_token.as_deref()) {
                            (Some(grant_id), Some(viewer_token)) => (grant_id, viewer_token),
                            _ => {
                                return Err(MMError::api(
                                    ErrorCode::InvalidToken,
                                    "anonymous offers need the watch link's grant_id and the viewer_token their public-join minted",
                                )
                                .into());
                            }
                        };
                    let Some(secret) = cfg
                        .advertising
                        .switch_auth_secret_opt()
                        .map(str::to_owned)
                    else {
                        return Err(MMError::api(
                            ErrorCode::FeatureDisabled,
                            "switch proxy requires MM_SWITCH_URL and MM_SWITCH_AUTH_SECRET",
                        )
                        .into());
                    };
                    crate::client::authorize_anonymous_offer(
                        &gate,
                        &id,
                        grant_id,
                        req.access_token.as_deref(),
                        viewer_token,
                        &secret,
                        req.source_id.as_deref(),
                    )
                    .await?
                }
            } else {
            let (grant_id, viewer_token) =
                match (req.grant_id.as_deref(), req.viewer_token.as_deref()) {
                    (Some(grant_id), Some(viewer_token)) => (grant_id, viewer_token),
                    _ => {
                        return Err(MMError::api(
                            ErrorCode::InvalidToken,
                            "anonymous offers need the watch link's grant_id and the viewer_token their public-join minted",
                        )
                        .into());
                    }
                };
            let Some(secret) = cfg.advertising.switch_auth_secret_opt().map(str::to_owned)
            else {
                return Err(MMError::api(
                    ErrorCode::FeatureDisabled,
                    "switch proxy requires MM_SWITCH_URL and MM_SWITCH_AUTH_SECRET",
                )
                .into());
            };
            crate::client::authorize_anonymous_offer(
                &gate,
                &id,
                grant_id,
                req.access_token.as_deref(),
                viewer_token,
                &secret,
                req.source_id.as_deref(),
            )
            .await?
            }
        }
    };
    let stream_id = StreamId(stream.id.clone());

    // A viewer admitted by a switch viewer token or a share link: whether the proxy serves
    // this stream and viewer, now that both are known (members were decided before their
    // gate).
    if !member_jwt {
        let viewer = mm_core::fleet::ViewerId::new(&viewer_id);
        let bound = match state.switch_pool.as_deref() {
            Some(pool) => on_a_relay(pool, &viewer).await,
            None => false,
        };
        let stored = MediaPath::parse(&stream.media_path).unwrap_or(MediaPath::Core);
        if !proxy_serves_offer(&cfg, Some(stored), bound) {
            return Err(proxy_off());
        }
    }

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

    // The admit step refused any other source, so this is the body's too.
    let source_id = mm_core::switch_client::switch_source_id(&stream.id);

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
    //
    // A viewer with no node is placed by the stream's stored media path state.
    let stream_path = MediaPath::parse(&stream.media_path).unwrap_or(MediaPath::Core);
    let (node_id, client) = place_viewer(&pool, &cfg, &stream_id, stream_path, &viewer).await;

    // FR-347b: a per-request VIEWER-role token whose sub is the derived id. Not
    // the server token — that one is exempt from the binding.
    let token = mm_core::switch_auth::generate_switch_token(
        &secret,
        "viewer",
        &viewer_id,
        VIEWER_TOKEN_TTL_SECS,
    );

    // Binds (or unbinds) the viewer, forwards, and moves the viewer to the origin if a
    // relay fails.
    let answer = forward_offer(
        &pool,
        &stream_id,
        &viewer,
        (node_id, client),
        &token,
        &req.offer,
        &source_id,
    )
    .await?;

    Ok(Json(ProxyOfferResponse {
        id: viewer_id,
        answer: unwrap_node_answer(answer),
    }))
}

/// The SDP answer inside a switch `/api/viewers/offer` response: the node
/// answers `{"id", "answer": {"type", "sdp"}}`, and the proxy's contract
/// hands the client the inner answer verbatim — the same shape the direct
/// path returns. Forwarding the whole envelope instead double-wraps it
/// (`answer.answer`), and every client reads `answer.sdp`: proxied viewers
/// never got media, silently falling back or failing.
fn proxy_answer_of(forwarded: &serde_json::Value) -> Result<serde_json::Value, String> {
    let answer = forwarded
        .get("answer")
        .ok_or_else(|| "switch response has no answer field".to_string())?;
    match answer.get("sdp").and_then(|s| s.as_str()) {
        Some(sdp) if !sdp.is_empty() => Ok(answer.clone()),
        _ => Err("switch answer has no SDP".to_string()),
    }
}

#[cfg(test)]
mod proxy_answer_tests {
    use super::proxy_answer_of;
    use serde_json::json;

    #[test]
    fn unwraps_the_switch_envelope_verbatim() {
        // Exactly what the node sends: the client must receive the inner
        // answer, with its type and SDP, not the envelope.
        let forwarded = json!({
            "id": "viewer-s-1-guest-g-1",
            "answer": {"type": "answer", "sdp": "v=0\r\n"},
        });
        assert_eq!(
            proxy_answer_of(&forwarded).expect("valid answer unwraps"),
            json!({"type": "answer", "sdp": "v=0\r\n"}),
        );
    }

    #[test]
    fn rejects_an_answer_without_sdp() {
        for forwarded in [
            json!({"id": "viewer-s-1"}),
            json!({"id": "viewer-s-1", "answer": {"type": "answer"}}),
            json!({"id": "viewer-s-1", "answer": {"type": "answer", "sdp": ""}}),
        ] {
            assert!(
                proxy_answer_of(&forwarded).is_err(),
                "unusable answer must not reach the client: {forwarded}"
            );
        }
    }
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
            Ok(list) => viewers += stream_viewer_count(&list, &want),
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

/// People watching `source_id` in one switch's viewer list: connected, on the stream or
/// mid ad-break (the shipped client counts those too), and not a relay node pulling the
/// stream through a cascade session — the census counts the same way.
fn stream_viewer_count(viewers: &[SwitchViewer], source_id: &str) -> u32 {
    viewers
        .iter()
        .filter(|v| v.connected)
        .filter(|v| !is_cascade_session(&v.id))
        .filter(|v| v.current_source == source_id || v.current_source.starts_with("ad-"))
        .count() as u32
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

    /// The node body is `{"id", "answer": {type, sdp}}`; clients must receive
    /// the inner answer, not the whole body (nested `answer.type` reads null
    /// and WebRTC rejects it: Fluffy Web "Join failed: ... 'null' ... is not
    /// a valid value for enumeration RTCSdpType").
    #[test]
    fn proxied_answer_is_the_nodes_inner_answer() {
        let body = serde_json::json!({
            "id": "viewer-s1--a-b",
            "answer": {"type": "answer", "sdp": "v=0\r\n"}
        });
        assert_eq!(
            unwrap_node_answer(body),
            serde_json::json!({"type": "answer", "sdp": "v=0\r\n"})
        );
        // No `answer` field: forward verbatim.
        let flat = serde_json::json!({"type": "answer", "sdp": "v=0"});
        assert_eq!(unwrap_node_answer(flat.clone()), flat);
    }

    #[test]
    fn the_proxy_is_on_whenever_the_state_is_not_core() {
        use mm_core::config::FleetMode;
        let mut cfg = mm_core::config::Config::default();
        cfg.fleet.proxy_viewers = false;
        cfg.fleet.media_path = MediaPath::RelayAfterThreshold;
        cfg.fleet.mode = FleetMode::On;
        cfg.advertising.switch_auth_secret = "s3cret".into();
        assert!(
            proxy_enabled(&cfg),
            "a relay path needs the proxy: relays are reached through it"
        );

        cfg.fleet.mode = FleetMode::Frozen;
        assert!(
            !proxy_enabled(&cfg),
            "frozen makes the path core, and the flag is off"
        );

        cfg.fleet.mode = FleetMode::On;
        cfg.advertising.switch_auth_secret = String::new();
        assert!(!proxy_enabled(&cfg), "no secret: the proxy must stay off");
        cfg.advertising.switch_auth_secret = "  \n".into();
        assert!(!proxy_enabled(&cfg), "a whitespace-only secret is not set");
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
    fn member_seat_is_found_by_derived_viewer_id() {
        let users = vec![
            "@demo_viewer:steegler.com".to_string(),
            "@other:steegler.com".to_string(),
        ];
        let sub = mm_core::switch_client::switch_viewer_id("s1", "@demo_viewer:steegler.com");
        assert_eq!(
            member_seat_user_id(&users, "s1", &sub),
            Some("@demo_viewer:steegler.com".to_string())
        );
        // A token for another stream names no seat here.
        let foreign = mm_core::switch_client::switch_viewer_id("s2", "@demo_viewer:steegler.com");
        assert_eq!(member_seat_user_id(&users, "s1", &foreign), None);
        // No seat at all.
        assert_eq!(member_seat_user_id(&[], "s1", &sub), None);
    }

    #[test]
    fn guest_seats_are_never_member_seats() {
        // Share-link guests derive viewer ids exactly like members; the
        // anonymous branch owns them, so the member path must skip them
        // even when the subject matches.
        let guest = "guest:grant-1:join-1".to_string();
        let sub = mm_core::switch_client::switch_viewer_id("s1", &guest);
        assert_eq!(member_seat_user_id(&[guest], "s1", &sub), None);
    }

    #[test]
    fn a_token_ttl_short_enough_to_be_single_use() {
        // The token is minted and used inside one request. A long TTL would turn a
        // logged or cached header into a reusable credential for that viewer id.
        assert!(VIEWER_TOKEN_TTL_SECS <= 300);
    }

    fn relay(id: &str) -> mm_core::fleet::FleetNode {
        mm_core::fleet::FleetNode {
            id: NodeId::new(id),
            flavor: mm_core::fleet::NodeFlavor::Fanout,
            ownership: mm_core::fleet::Ownership::Leased,
            state: mm_core::fleet::NodeState::Healthy,
            viewer_capacity: 250,
            viewers_current: 0,
            funding: mm_core::fleet::funding::Funding::Operator,
        }
    }

    /// A pool whose relay `r1` carries stream `s1`, which 500 people watch.
    async fn pool_with_a_carrier() -> SwitchPool {
        let pool = SwitchPool::new(Arc::new(SwitchClient::new("http://origin.invalid:7890")));
        pool.upsert(
            &relay("r1"),
            Arc::new(SwitchClient::new("http://r1.invalid:7890")),
        )
        .await;
        pool.set_carried(
            &NodeId::new("r1"),
            std::collections::HashSet::from(["s1".to_string()]),
        )
        .await;
        pool.set_stream_viewers(std::collections::HashMap::from([("s1".to_string(), 500)]))
            .await;
        pool
    }

    fn fleet_on(threshold: u32) -> mm_core::config::Config {
        let mut cfg = mm_core::config::Config::default();
        cfg.fleet.mode = mm_core::config::FleetMode::On;
        cfg.fleet.relay_viewer_threshold = threshold;
        cfg.advertising.switch_auth_secret = "s3cret".into();
        cfg
    }

    async fn placed(
        pool: &SwitchPool,
        cfg: &mm_core::config::Config,
        stream: &str,
        path: MediaPath,
        viewer: &str,
    ) -> Option<String> {
        place_viewer(
            pool,
            cfg,
            &StreamId(stream.into()),
            path,
            &mm_core::fleet::ViewerId::new(viewer),
        )
        .await
        .0
        .map(|n| n.as_str().to_string())
    }

    /// Design C14: under state `core` relays are not used for placement, even with
    /// `fleet.proxy_viewers` on, relays leased and carrying the stream, and the audience
    /// far past the threshold.
    #[tokio::test]
    async fn state_core_means_the_core_only() {
        let pool = pool_with_a_carrier().await;
        let mut cfg = fleet_on(50);
        cfg.fleet.proxy_viewers = true;
        cfg.fleet.media_path = MediaPath::RelayAfterThreshold; // the setting today
        assert!(proxy_enabled(&cfg));
        assert_eq!(placed(&pool, &cfg, "s1", MediaPath::Core, "v1").await, None);
    }

    #[tokio::test]
    async fn a_relayed_stream_past_its_threshold_sends_new_viewers_to_a_carrier() {
        let pool = pool_with_a_carrier().await;
        let path = MediaPath::RelayAfterThreshold;

        assert_eq!(
            placed(&pool, &fleet_on(50), "s1", path, "v1")
                .await
                .as_deref(),
            Some("r1")
        );
        // A stored relay_entry is served as relay_after_threshold.
        assert_eq!(
            placed(&pool, &fleet_on(50), "s1", MediaPath::RelayEntry, "v2")
                .await
                .as_deref(),
            Some("r1")
        );
        // Below the threshold: the core.
        assert_eq!(placed(&pool, &fleet_on(501), "s1", path, "v3").await, None);
        // The fleet not on: the core, whatever the stored state.
        let mut frozen = fleet_on(50);
        frozen.fleet.mode = mm_core::config::FleetMode::Frozen;
        assert_eq!(placed(&pool, &frozen, "s1", path, "v4").await, None);
        // Nobody carries s2, though it is past its threshold: the core.
        pool.set_stream_viewers(std::collections::HashMap::from([
            ("s1".to_string(), 500),
            ("s2".to_string(), 500),
        ]))
        .await;
        assert_eq!(placed(&pool, &fleet_on(50), "s2", path, "v5").await, None);
        // A stream the census did not list has an unknown audience: the core.
        assert_eq!(placed(&pool, &fleet_on(50), "s3", path, "v6").await, None);
    }

    /// Nobody already watching is moved: a viewer bound to a relay keeps it, even when
    /// the stream would now place a new viewer on the core.
    #[tokio::test]
    async fn a_bound_viewer_keeps_its_node() {
        let pool = pool_with_a_carrier().await;
        pool.bind_viewer(mm_core::fleet::ViewerId::new("v1"), NodeId::new("r1"))
            .await;
        assert_eq!(
            placed(&pool, &fleet_on(50), "s1", MediaPath::Core, "v1")
                .await
                .as_deref(),
            Some("r1")
        );
    }

    /// A binding is honoured only while its relay carries the stream. A viewer whose relay
    /// dropped the stream (the viewer's own disconnect let the sync remove it) re-offers
    /// with the same id: sending them back there would answer with no picture, so the
    /// binding goes and the viewer is placed fresh, here on the origin.
    #[tokio::test]
    async fn a_bound_viewer_whose_relay_no_longer_carries_the_stream_is_placed_fresh() {
        let pool = pool_with_a_carrier().await;
        let viewer = mm_core::fleet::ViewerId::new("v1");
        pool.bind_viewer(viewer.clone(), NodeId::new("r1")).await;
        pool.set_carried(&NodeId::new("r1"), std::collections::HashSet::new())
            .await;
        let path = MediaPath::RelayAfterThreshold;
        assert_eq!(
            placed(&pool, &fleet_on(50), "s1", path, "v1").await,
            None,
            "nobody carries s1 any more: the origin"
        );
        assert_eq!(
            pool.node_of_viewer(&viewer).await,
            None,
            "the stale binding is gone"
        );
    }

    /// A bound viewer whose relay carries another stream but not this one is placed fresh
    /// on a relay that does carry it.
    #[tokio::test]
    async fn a_bound_viewer_is_moved_off_a_relay_that_does_not_carry_its_stream() {
        let pool = pool_with_a_carrier().await;
        pool.upsert(
            &relay("r2"),
            Arc::new(SwitchClient::new("http://r2.invalid:7890")),
        )
        .await;
        pool.set_carried(
            &NodeId::new("r2"),
            std::collections::HashSet::from(["s9".to_string()]),
        )
        .await;
        let viewer = mm_core::fleet::ViewerId::new("v1");
        pool.bind_viewer(viewer.clone(), NodeId::new("r2")).await;
        let path = MediaPath::RelayAfterThreshold;
        assert_eq!(
            placed(&pool, &fleet_on(50), "s1", path, "v1")
                .await
                .as_deref(),
            Some("r1")
        );
    }

    // ── The proxy per stream, and a failing relay ──────────────────────────

    #[test]
    fn the_proxy_stays_on_for_a_stream_that_started_in_the_relay_state() {
        let mut cfg = fleet_on(50);
        cfg.fleet.proxy_viewers = false;
        cfg.fleet.media_path = MediaPath::Core; // moved after the stream started
        assert!(proxy_enabled_for(&cfg, MediaPath::RelayAfterThreshold));
        assert!(!proxy_enabled_for(&cfg, MediaPath::Core));
        cfg.advertising.switch_auth_secret = String::new();
        assert!(!proxy_enabled_for(&cfg, MediaPath::RelayAfterThreshold));
    }

    /// The join hands out the proxy URL for a relay-state stream after the setting moved,
    /// and the direct origin URL for a core stream.
    #[test]
    fn the_joins_switch_url_follows_the_streams_state() {
        let mut cfg = fleet_on(50);
        cfg.fleet.proxy_viewers = false;
        cfg.fleet.media_path = MediaPath::Core;
        assert_eq!(
            viewer_switch_url(
                "https://mm.example",
                "s1",
                MediaPath::RelayAfterThreshold,
                &cfg
            ),
            "https://mm.example/_mm/fleet/v1/streams/s1"
        );
        assert_eq!(
            viewer_switch_url("https://mm.example", "s1", MediaPath::Core, &cfg),
            "https://mm.example/_mm/switch"
        );
    }

    /// The kill switch stops new placements; it does not cut off a viewer already on a
    /// relay: the re-offer is admitted and goes to the bound relay.
    #[tokio::test]
    async fn a_viewer_already_on_a_relay_is_served_whatever_the_setting() {
        let pool = pool_with_a_carrier().await;
        pool.bind_viewer(mm_core::fleet::ViewerId::new("v1"), NodeId::new("r1"))
            .await;
        let mut cfg = fleet_on(50);
        cfg.fleet.proxy_viewers = false;
        cfg.fleet.media_path = MediaPath::Core;
        cfg.fleet.mode = mm_core::config::FleetMode::Frozen;
        let stored = MediaPath::RelayAfterThreshold;

        let viewer = mm_core::fleet::ViewerId::new("v1");
        assert!(proxy_serves_offer(
            &cfg,
            Some(stored),
            on_a_relay(&pool, &viewer).await
        ));
        assert_eq!(
            placed(&pool, &cfg, "s1", stored, "v1").await.as_deref(),
            Some("r1")
        );
        // Without a secret nothing is served, not even a bound viewer.
        cfg.advertising.switch_auth_secret = String::new();
        assert!(!proxy_serves_offer(&cfg, Some(stored), true));
    }

    #[tokio::test]
    async fn a_new_viewer_under_frozen_goes_to_the_origin() {
        let pool = pool_with_a_carrier().await;
        let mut cfg = fleet_on(50);
        cfg.fleet.mode = mm_core::config::FleetMode::Frozen;
        let viewer = mm_core::fleet::ViewerId::new("v-new");
        assert!(!on_a_relay(&pool, &viewer).await);
        assert_eq!(
            placed(&pool, &cfg, "s1", MediaPath::RelayAfterThreshold, "v-new").await,
            None
        );
    }

    /// A stand-in switch answering `POST /api/viewers/offer`: with an SDP naming `name`
    /// when `ok`, else with a 503.
    async fn offer_switch(name: &'static str, ok: bool) -> Arc<SwitchClient> {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/api/viewers/offer",
            post(move || async move {
                if ok {
                    Json(serde_json::json!({
                        "id": "v",
                        "answer": { "type": "answer", "sdp": format!("v=0 {name}") }
                    }))
                    .into_response()
                } else {
                    (StatusCode::SERVICE_UNAVAILABLE, "down").into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Arc::new(SwitchClient::new(&url))
    }

    /// A pool whose origin and relay `r1` are stand-ins.
    async fn pool_of(origin_ok: bool, relay_ok: bool) -> (SwitchPool, Arc<SwitchClient>) {
        let pool = SwitchPool::new(offer_switch("origin", origin_ok).await);
        let relay_client = offer_switch("relay", relay_ok).await;
        pool.upsert(&relay("r1"), relay_client.clone()).await;
        (pool, relay_client)
    }

    async fn offer_to(
        pool: &SwitchPool,
        node: Option<&str>,
        client: Arc<SwitchClient>,
    ) -> Result<serde_json::Value, MMError> {
        forward_offer(
            pool,
            &StreamId("s1".into()),
            &mm_core::fleet::ViewerId::new("v1"),
            (node.map(NodeId::new), client),
            "token",
            &serde_json::json!({ "type": "offer", "sdp": "v=0" }),
            "stream-s1",
        )
        .await
    }

    #[tokio::test]
    async fn a_relay_that_fails_sends_the_viewer_to_the_origin() {
        let (pool, relay_client) = pool_of(true, false).await;
        let answer = offer_to(&pool, Some("r1"), relay_client)
            .await
            .expect("the origin answers");
        assert_eq!(answer["sdp"], "v=0 origin");
        assert_eq!(
            pool.node_of_viewer(&mm_core::fleet::ViewerId::new("v1"))
                .await,
            None,
            "the viewer is on the origin now: no binding"
        );
    }

    #[tokio::test]
    async fn a_bound_viewer_whose_relay_fails_moves_to_the_origin() {
        let (pool, relay_client) = pool_of(true, false).await;
        let viewer = mm_core::fleet::ViewerId::new("v1");
        pool.bind_viewer(viewer.clone(), NodeId::new("r1")).await;
        pool.set_carried(
            &NodeId::new("r1"),
            std::collections::HashSet::from(["s1".to_string()]),
        )
        .await;
        // Placed by its binding, as the handler does.
        let placed_on = place_viewer(
            &pool,
            &fleet_on(50),
            &StreamId("s1".into()),
            MediaPath::Core,
            &viewer,
        )
        .await;
        assert_eq!(placed_on.0, Some(NodeId::new("r1")));
        let _ = relay_client;
        let answer = forward_offer(
            &pool,
            &StreamId("s1".into()),
            &viewer,
            placed_on,
            "token",
            &serde_json::json!({ "type": "offer", "sdp": "v=0" }),
            "stream-s1",
        )
        .await
        .expect("the origin answers");
        assert_eq!(answer["sdp"], "v=0 origin");
        assert_eq!(pool.node_of_viewer(&viewer).await, None);
    }

    #[tokio::test]
    async fn when_the_origin_fails_too_the_error_is_the_origins() {
        let (pool, relay_client) = pool_of(false, false).await;
        let err = offer_to(&pool, Some("r1"), relay_client)
            .await
            .expect_err("both fail");
        let text = err.to_string();
        assert!(text.contains("503"), "{text}");
        assert_eq!(
            pool.node_of_viewer(&mm_core::fleet::ViewerId::new("v1"))
                .await,
            None
        );

        // An offer placed on the origin is sent once: its failure is returned as is.
        let (pool, _) = pool_of(false, true).await;
        let origin = pool.origin();
        assert!(offer_to(&pool, None, origin).await.is_err());
    }

    /// A relay that answers with no usable SDP has failed too.
    #[tokio::test]
    async fn a_relay_with_an_unusable_answer_sends_the_viewer_to_the_origin() {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/api/viewers/offer",
            post(|| async {
                Json(serde_json::json!({ "id": "v", "answer": { "type": "answer" } }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let pool = SwitchPool::new(offer_switch("origin", true).await);
        let bad = Arc::new(SwitchClient::new(&url));
        pool.upsert(&relay("r1"), bad.clone()).await;
        let answer = offer_to(&pool, Some("r1"), bad)
            .await
            .expect("the origin answers");
        assert_eq!(answer["sdp"], "v=0 origin");
    }

    /// The count the apps show must agree with the census: a relay pulling the
    /// stream (`cascade-…`) is not a viewer, while a real viewer, one mid ad-break,
    /// is. Disconnected viewers and other streams' viewers are not counted either.
    #[test]
    fn a_relay_pulling_the_stream_is_not_in_the_viewer_count() {
        let viewer = |id: &str, source: &str, connected: bool| SwitchViewer {
            id: id.to_string(),
            current_source: source.to_string(),
            connected,
        };
        let list = vec![
            viewer("viewer-b1--alice-example.org", "stream-b1", true),
            viewer("cascade-stream-b1-lease-1-0", "stream-b1", true),
            viewer("viewer-b1--bob-example.org", "ad-bob", true),
            viewer("viewer-b1--carol-example.org", "stream-b1", false),
            viewer("viewer-b2--dave-example.org", "stream-b2", true),
        ];
        assert_eq!(stream_viewer_count(&list, "stream-b1"), 2);
    }
}
