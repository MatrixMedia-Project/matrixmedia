//! Pool-sync loop (relay fleet, Task 3): keeps [`SwitchPool`] in step with
//! `mm_fleet_nodes`, and seeds a cascade source on every relay for every live
//! broadcast.
//!
//! Two jobs, kept apart on purpose:
//!
//! * [`sync_actions`] is the pure diff between what the database says exists and
//!   what the pool already holds. It is the only part of this module worth
//!   table-testing, because it is the only part with no I/O to mock.
//! * [`run_pool_sync`] is the impure shell: it reads `mm_fleet_nodes`, calls
//!   `sync_actions`, applies the result to the pool, and — for every relay the
//!   diff just confirmed healthy — seeds one cascade source per live broadcast
//!   so that relay's viewers can cascade from it (R2).
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use mm_core::fleet::{FleetNode, NodeFlavor, NodeId, NodeState};
use mm_core::switch_client::{CascadeSeedOutcome, SwitchClient, mint_cascade_token, switch_source_id};
use sqlx::PgPool;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::fleet_census::SwitchCensus;
use crate::switch_pool::SwitchPool;
use mm_fleet::nodes_db;

/// The port every mm-switch instance listens on, fleet or origin alike. Matches
/// the convention already used wherever a `SwitchClient` is built from a bare
/// address (`startup.rs`'s metering tests, `ad_affinity.rs`).
const SWITCH_PORT: u16 = 7890;

/// How long a minted cascade token is valid for before the relay's own posted
/// offer to the origin would be rejected. 120 s: comfortably longer than the
/// 10 s sync interval — so a relay that takes a few seconds to act on the seed,
/// or a tick that slips, never presents an already-expired token — while short
/// enough that a leaked token is not a standing credential. A fresh token is
/// minted on every seed anyway; the TTL only has to cover the relay's own
/// offer-to-origin round trip.
const CASCADE_TOKEN_TTL: StdDuration = StdDuration::from_secs(120);

/// Per-relay time budget for one tick's seeding. Must sit well under the 10 s
/// tick: an unreachable relay otherwise holds its seeding future for the full
/// HTTP connect/request timeout (×2 on the 401 renewal retry), and with the
/// relays seeded concurrently this budget is what guarantees the TICK, not just
/// each relay, finishes on time. A relay that runs out of budget is skipped and
/// retried next tick — seeding is idempotent (409 = already there).
const SEED_BUDGET_PER_RELAY: StdDuration = StdDuration::from_secs(8);

/// M2: how long a cascade source may sit registered-but-inactive (no track has arrived,
/// or arrived once and the origin connection then died — `source_cascade.go`'s "exhausted
/// retries are intentionally terminal") before this loop deletes it and re-seeds from
/// scratch. Three ticks: long enough that one slow OnTrack right after seeding, or one
/// transient census miss, is not mistaken for a dead source; short enough that a genuinely
/// dead cascade does not sit silently black for minutes.
const INACTIVE_GRACE: StdDuration = StdDuration::from_secs(30);

/// Diff output: what [`run_pool_sync`] must do to the pool this tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncActions {
    /// Rows to upsert into the pool: healthy fan-out relays, nothing else.
    pub upserts: Vec<FleetNode>,
    /// Pool entries with no corresponding healthy fan-out row in the database
    /// any more — gone, draining, torn down, or reflavoured away from fanout.
    pub evictions: Vec<NodeId>,
}

/// Pure diff between the observed database rows and the pool's current
/// membership.
///
/// Only `Fanout` nodes in state `Healthy` are candidates to be in the pool at
/// all: `Origin` is not a pool entry (it is `SwitchPool`'s separate `origin`
/// field), `Transcode` serves no viewers and has nothing to cascade to, and
/// `Edge` serves LL-HLS rather than the WebRTC cascade path this loop seeds.
/// Everything else — present in the pool but not upserted this tick, for any
/// reason — is evicted. An empty database therefore evicts every pool entry:
/// there is nothing left to keep.
pub fn sync_actions(db_nodes: &[FleetNode], pool_nodes: &[NodeId]) -> SyncActions {
    let upserts: Vec<FleetNode> = db_nodes
        .iter()
        .filter(|n| n.flavor == NodeFlavor::Fanout && n.state == NodeState::Healthy)
        .cloned()
        .collect();

    let keep: HashSet<&NodeId> = upserts.iter().map(|n| &n.id).collect();
    let evictions: Vec<NodeId> = pool_nodes
        .iter()
        .filter(|id| !keep.contains(id))
        .cloned()
        .collect();

    SyncActions { upserts, evictions }
}

/// One row of `mm_fleet_nodes`, as this loop needs it: `sync_actions`'s input
/// plus the address that `load_nodes` (`mm_fleet::desired::DesiredStore`)
/// deliberately omits — that method serves the sweepers, which never need to
/// dial a node, so it was never taught to read `public_ip`. This loop is the
/// first caller that does.
async fn load_fanout_rows(db: &PgPool) -> Result<Vec<(FleetNode, Option<String>)>, String> {
    type Row = (
        String,         // mm_node_id
        String,         // flavor
        String,         // ownership
        String,         // state
        Option<String>, // public_ip (INET, text-cast)
        Option<i32>,    // viewer_capacity
        i32,            // viewers_current
        String,         // funding
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT mm_node_id, flavor, ownership, state, public_ip::text,
                viewer_capacity, viewers_current, funding
           FROM mm_fleet_nodes
          ORDER BY mm_node_id",
    )
    .fetch_all(db)
    .await
    .map_err(|e| format!("pool sync: loading mm_fleet_nodes failed: {e}"))?;

    Ok(rows
        .into_iter()
        .filter_map(
            |(id, flavor, ownership, state, public_ip, cap, cur, funding)| {
                Some((
                    FleetNode {
                        id: NodeId::new(id),
                        flavor: NodeFlavor::parse(&flavor)?,
                        ownership: mm_core::fleet::Ownership::parse(&ownership)?,
                        state: NodeState::parse(&state)?,
                        viewer_capacity: cap.unwrap_or(0).max(0) as u32,
                        viewers_current: cur.max(0) as u32,
                        funding: mm_core::fleet::funding::Funding::parse(&funding)?,
                    },
                    public_ip,
                ))
            },
        )
        .collect())
}

/// Per-(relay, broadcast) bookkeeping for M2's liveness grace period: when a cascade
/// source was FIRST observed inactive (no track yet, or dead) with nothing seen healthy
/// since. Cleared the moment the source reports active again.
type InactiveTracker = AsyncMutex<HashMap<(NodeId, String), Instant>>;

/// Mint a cascade token and seed (or confirm) the relay's source for one
/// broadcast.
///
/// On `Unauthorized`: this is now ALWAYS the relay's own `/api/sources/cascade` route
/// rejecting mm-core's server bearer (B2 — `add_cascade_source` attaches it via
/// `apply_auth`), never a complaint about the cascade `token` field. Renewing the cascade
/// token and retrying therefore does nothing useful against a genuinely wrong/rotated
/// server secret; it is kept as a one-shot retry only because a transient clock skew in
/// the minted-token TTL check is cheap to rule out, and logs loudly rather than looping if
/// it happens twice — if that log ever fires in production, look at the SERVER secret
/// (`MM_SWITCH_AUTH_SECRET` on this relay), not the cascade token.
///
/// On `AlreadyExists` (M2 + M3, composed): the relay already holds this broadcast's
/// source from an earlier tick. Two independent things can be true at once:
///   - the source may be dead (origin connection dropped and exhausted its retries —
///     `source_cascade.go` leaves that state TERMINAL on purpose) — `inactive` tracks how
///     long THIS relay+broadcast pair has read inactive across ticks, and past
///     `INACTIVE_GRACE` this deletes and re-seeds from scratch rather than leaving
///     viewers on a source that will never recover on its own.
///   - the source's cascade token may be stale (seeded with a 120 s TTL; this loop ticks
///     every 10 s, so the token is routinely still valid, but a slow/hung tick or a
///     paused relay is not impossible) — a fresh one is pushed via `UpdateToken` every
///     time, win or lose, so the NEXT reconnect (ICE drop, 401) always has a current
///     token instead of the one pinned at seed time.
#[allow(clippy::too_many_arguments)]
async fn seed_cascade_source(
    dialer: &SwitchClient,
    relay_base_url: &str,
    relay_node_id: &NodeId,
    origin_url: &str,
    secret: &str,
    broadcast_id: &str,
    inactive: &InactiveTracker,
) {
    // B1: the claim's "broadcast id" field must be the EXACT string the origin derives
    // from the offer it receives after stripping "cascade-" and "-{relay_node_id}" —
    // that is switch_source_id(broadcast_id) ("stream-{broadcast_id}"), not the raw
    // broadcast id. source_cascade.go's postOffer sends id = "cascade-{id}-{relayNodeID}"
    // where `id` is what THIS loop hands it below — so the claim and the offer must be
    // minted from the same value, which is this one.
    let source_id = switch_source_id(broadcast_id);
    let key = (relay_node_id.clone(), source_id.clone());

    for attempt in 0..2 {
        let token = mint_cascade_token(
            secret,
            &source_id,
            relay_node_id.as_str(),
            CASCADE_TOKEN_TTL,
            SystemTime::now(),
        );
        match dialer
            .add_cascade_source(relay_base_url, &source_id, origin_url, &token, relay_node_id.as_str())
            .await
        {
            Ok(CascadeSeedOutcome::Created) => {
                inactive.lock().await.remove(&key);
                return;
            }
            Ok(CascadeSeedOutcome::AlreadyExists) => {
                reconcile_existing_cascade_source(
                    dialer,
                    relay_base_url,
                    relay_node_id,
                    origin_url,
                    &source_id,
                    &token,
                    inactive,
                )
                .await;
                return;
            }
            Ok(CascadeSeedOutcome::Unauthorized) => {
                tracing::warn!(
                    relay = %relay_node_id,
                    broadcast = broadcast_id,
                    attempt,
                    "cascade seed: relay rejected mm-core's server bearer as unauthorized, retrying once"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    relay = %relay_node_id,
                    broadcast = broadcast_id,
                    error = %e,
                    "cascade seed failed"
                );
                return;
            }
        }
    }
    tracing::error!(
        relay = %relay_node_id,
        broadcast = broadcast_id,
        "cascade seed: relay rejected the server bearer twice in a row — giving up this \
         tick; check MM_SWITCH_AUTH_SECRET on this relay, not the cascade token"
    );
}

/// M2 + M3: called once `add_cascade_source` has confirmed the relay already has this
/// broadcast's source. Pushes a fresh token (M3) unconditionally, then checks the
/// source's liveness (M2): if it has read inactive for longer than `INACTIVE_GRACE`,
/// deletes it and re-seeds fresh rather than leaving viewers on a source that
/// `source_cascade.go` will never bring back on its own.
#[allow(clippy::too_many_arguments)]
async fn reconcile_existing_cascade_source(
    dialer: &SwitchClient,
    relay_base_url: &str,
    relay_node_id: &NodeId,
    origin_url: &str,
    source_id: &str,
    fresh_token: &str,
    inactive: &InactiveTracker,
) {
    let key = (relay_node_id.clone(), source_id.to_string());

    // M3: always push the token we just minted, independent of liveness below — the
    // NEXT reconnect this source attempts (ICE drop, 401) must never use whatever it
    // was constructed with at seed time.
    if let Err(e) = dialer
        .update_cascade_token(relay_base_url, source_id, fresh_token)
        .await
    {
        tracing::warn!(
            relay = %relay_node_id,
            source = source_id,
            error = %e,
            "cascade seed: pushing a renewed token to an already-seeded source failed"
        );
    }

    // M2: ask the relay whether this source is actually alive. list_sources failing is
    // treated as "can't tell this tick" — NOT as inactive, so one census blip does not
    // start (or reset) the grace clock.
    let sources = match dialer.list_sources().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                relay = %relay_node_id,
                source = source_id,
                error = %e,
                "cascade seed: could not check source liveness this tick"
            );
            return;
        }
    };
    let active = sources.iter().any(|s| s.id == source_id && s.active);

    if active {
        inactive.lock().await.remove(&key);
        return;
    }

    let now = Instant::now();
    let mut guard = inactive.lock().await;
    let since = *guard.entry(key.clone()).or_insert(now);
    let dead_for = now.duration_since(since);
    drop(guard);

    if dead_for < INACTIVE_GRACE {
        return;
    }

    tracing::warn!(
        relay = %relay_node_id,
        source = source_id,
        dead_for_secs = dead_for.as_secs(),
        "cascade seed: source inactive past the grace period, deleting and re-seeding"
    );
    pool_sync_dead_cascade_sources_total().inc();
    if let Err(e) = dialer.remove_source(source_id).await {
        tracing::warn!(
            relay = %relay_node_id,
            source = source_id,
            error = %e,
            "cascade seed: removing a dead source before re-seeding failed"
        );
        return;
    }
    inactive.lock().await.remove(&key);

    match dialer
        .add_cascade_source(relay_base_url, source_id, origin_url, fresh_token, relay_node_id.as_str())
        .await
    {
        Ok(_) => {}
        Err(e) => tracing::warn!(
            relay = %relay_node_id,
            source = source_id,
            error = %e,
            "cascade seed: re-seeding a deleted dead source failed"
        ),
    }
}

/// M2's metric: a dead cascade source noticed and re-seeded. Counted so a flapping
/// relay/origin link is visible without reading logs.
fn pool_sync_dead_cascade_sources_total() -> &'static prometheus::IntCounter {
    use std::sync::OnceLock;
    static COUNTER: OnceLock<prometheus::IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        prometheus::register_int_counter!(
            "mm_pool_sync_dead_cascade_sources_total",
            "Cascade sources found inactive past the grace period and re-seeded"
        )
        .expect("metric registered once")
    })
}

/// M2's cross-tick state: one process-wide tracker, not per-call, because the grace
/// period it enforces (`INACTIVE_GRACE`) only means anything measured across ticks.
fn inactive_tracker() -> &'static InactiveTracker {
    use std::sync::OnceLock;
    static TRACKER: OnceLock<InactiveTracker> = OnceLock::new();
    TRACKER.get_or_init(|| AsyncMutex::new(HashMap::new()))
}

/// One pass: diff, apply to the pool, then seed every healthy relay with every
/// live broadcast that has viewers.
async fn sync_once(pool: &Arc<SwitchPool>, db: &PgPool, switch_secret: Option<&str>) {
    let rows = match load_fanout_rows(db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("pool sync: {e}");
            return;
        }
    };
    let db_nodes: Vec<FleetNode> = rows.iter().map(|(n, _)| n.clone()).collect();
    let pool_node_ids: Vec<NodeId> = pool.nodes().await.into_iter().map(|n| n.id).collect();

    let actions = sync_actions(&db_nodes, &pool_node_ids);

    for id in &actions.evictions {
        pool.evict(id).await;
        tracing::info!(node = %id, "pool sync: evicted");
    }

    for node in &actions.upserts {
        let Some(ip) = rows
            .iter()
            .find(|(n, _)| &n.id == &node.id)
            .and_then(|(_, ip)| ip.clone())
        else {
            // A healthy row with no recorded address cannot be dialed; skip it
            // this tick rather than register a client nobody can reach.
            tracing::warn!(node = %node.id, "pool sync: healthy relay has no public_ip yet, skipping");
            continue;
        };
        let base_url = format!("http://{ip}:{SWITCH_PORT}");
        // B2: every server-role route on a relay (cascade seeding, token renewal,
        // switch_viewer, remove_source, …) is `wrapAuth(authSecret, serverOnly, …)` — it
        // demands mm-core's own bearer. A client built via `SwitchClient::new` never sends
        // one and 401s on all of them; `with_auth` is the client every relay call in this
        // loop must use whenever a secret is configured.
        let client: Arc<SwitchClient> = match switch_secret {
            Some(secret) => Arc::new(SwitchClient::with_auth(&base_url, secret.to_string())),
            None => Arc::new(SwitchClient::new(&base_url)),
        };
        pool.upsert(node, client).await;
    }

    // Census (§8.2 R6 + m4): ONE gather of every node — origin and fan-out alike — feeds
    // both the capacity gauges' `viewers_current` write-back AND cascade seeding's live-
    // broadcast list below. Before this they were two separate full gathers in the same
    // tick, each polling every node for the same underlying `list_viewers` data. Gathered
    // from the pool we just synced above (so a node just evicted is not polled, and a node
    // just upserted already is).
    let census = SwitchCensus::new(pool.clone(), db.clone());
    let (viewer_counts_by_node, live) = match census.viewers_per_node_and_live_broadcasts().await
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!("pool sync: census failed: {e}");
            return;
        }
    };
    let viewer_counts: Vec<(NodeId, i64)> = viewer_counts_by_node.into_iter().collect();
    if let Err(e) = nodes_db::update_viewers_current(db, &viewer_counts).await {
        tracing::warn!("pool sync: writing back viewers_current failed: {e}");
    }

    // Seed cascade sources: the origin is always reachable even with no fleet, so
    // viewer-counting for seeding reuses the census gathered above.
    let Some(secret) = switch_secret else {
        return;
    };
    let live_with_viewers: Vec<&str> = live
        .iter()
        .filter(|b| b.viewers > 0)
        .map(|b| b.broadcast_id.as_str())
        .collect();
    if live_with_viewers.is_empty() {
        return;
    }

    let origin_url = pool.origin().base_url().to_string();
    let broadcasts: Vec<String> = live_with_viewers.iter().map(|s| s.to_string()).collect();

    let mut relays = Vec::new();
    for node in &actions.upserts {
        // The relay's own client: its connection pool already points at the
        // relay. (The origin's client would also work — `add_cascade_source`
        // takes the target URL explicitly — but would open a second pool of
        // connections to every relay for no reason.)
        let Some(client) = pool.client_for_node(Some(&node.id)).await else {
            continue;
        };
        let relay_id = node.id.clone();
        let origin_url = origin_url.clone();
        let secret = secret.to_string();
        let broadcasts = broadcasts.clone();
        let fut = async move {
            let relay_base_url = client.base_url().to_string();
            // Within one relay the broadcasts stay sequential: they share a
            // connection, and a relay slow for one broadcast is slow for all.
            for broadcast_id in &broadcasts {
                seed_cascade_source(
                    &client,
                    &relay_base_url,
                    &relay_id,
                    &origin_url,
                    &secret,
                    broadcast_id,
                    inactive_tracker(),
                )
                .await;
            }
        };
        relays.push((node.id.clone(), fut));
    }
    for relay_id in seed_relays_within_budget(relays, SEED_BUDGET_PER_RELAY).await {
        tracing::warn!(
            relay = %relay_id,
            budget_secs = SEED_BUDGET_PER_RELAY.as_secs(),
            "cascade seeding ran out of its per-relay budget — skipping the rest of \
             this relay's broadcasts; seeding is idempotent and the next tick retries"
        );
    }
}

/// Run one seeding future per relay CONCURRENTLY, each under its own `budget`.
/// Returns the ids of relays that ran out of budget.
///
/// The isolation property this pins: one unreachable relay costs its own budget
/// and nothing else — not the other relays' seeding (they run in parallel, as
/// their own tasks) and not the tick (the whole fan-out finishes within one
/// budget plus scheduling noise, however many relays there are). Generic over
/// the future so the property is testable without sockets.
async fn seed_relays_within_budget<Fut>(
    relays: Vec<(NodeId, Fut)>,
    budget: StdDuration,
) -> Vec<NodeId>
where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut tasks = tokio::task::JoinSet::new();
    for (relay_id, fut) in relays {
        tasks.spawn(async move {
            match tokio::time::timeout(budget, fut).await {
                Ok(()) => None,
                Err(_) => Some(relay_id),
            }
        });
    }
    let mut timed_out = Vec::new();
    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(Some(relay_id)) => timed_out.push(relay_id),
            Ok(None) => {}
            // m5: previously discarded silently. A panic inside one relay's seeding future
            // is contained (JoinSet, not the tick's own task) but was otherwise invisible —
            // logged here so a bug in the seeding path shows up instead of just quietly
            // never seeding that relay.
            Err(e) => tracing::error!(
                error = %e,
                "pool sync: a relay's seeding task panicked or was cancelled"
            ),
        }
    }
    timed_out
}

/// The pool-sync loop (relay fleet Task 3). Runs every `interval`, keeping
/// `SwitchPool` in step with `mm_fleet_nodes` and seeding cascade sources on
/// every healthy relay for every live, watched broadcast.
///
/// `switch_secret` is `None` when `MM_SWITCH_AUTH_SECRET` is unset; cascade
/// tokens cannot be minted without it, so this loop still syncs node
/// membership but skips seeding — same posture as the rest of the switch
/// integration when the secret is absent.
pub async fn run_pool_sync(
    pool: Arc<SwitchPool>,
    db: PgPool,
    switch_secret: Option<String>,
    interval: StdDuration,
    cancel: CancellationToken,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                sync_once(&pool, &db, switch_secret.as_deref()).await;
                mm_core::metrics_global::heartbeat("pool_sync");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_core::fleet::Ownership;

    fn node(id: &str, flavor: NodeFlavor, state: NodeState) -> FleetNode {
        FleetNode {
            id: NodeId::new(id),
            flavor,
            ownership: Ownership::Rented,
            state,
            viewer_capacity: 250,
            viewers_current: 0,
            funding: mm_core::fleet::funding::Funding::Wallet,
        }
    }

    /// B1, pinned at the exact seam the review traced the bug to: `seed_cascade_source`
    /// must mint the cascade token for `switch_source_id(broadcast_id)` ("stream-{id}"),
    /// NOT the raw broadcast id. `source_cascade.go`'s offer id is
    /// "cascade-{source_id}-{relay_node_id}"; the origin strips "cascade-" and
    /// "-{relay_node_id}" to recover `source_id` and compares it against the claim's
    /// broadcast field. Minting for the raw id (the bug this review found) makes that
    /// compare fail on every production-shaped id — this test fails the moment that
    /// regresses, without needing a live HTTP round trip.
    #[test]
    fn seed_mints_the_token_for_switch_source_id_not_the_raw_broadcast_id() {
        let broadcast_id = "3f2a1c9e-0000-4000-8000-00000000000a";
        let source_id = switch_source_id(broadcast_id);
        assert_eq!(source_id, format!("stream-{broadcast_id}"));

        let secret = "s";
        let relay = "relay-1";
        let token = mint_cascade_token(secret, &source_id, relay, CASCADE_TOKEN_TTL, SystemTime::now());
        let claim = token.split('.').next().unwrap();
        assert!(
            claim.starts_with(&format!("cascade:{source_id}:{relay}:")),
            "claim {claim:?} was not minted for switch_source_id(broadcast_id)"
        );
    }

    #[test]
    fn a_healthy_fanout_row_is_upserted() {
        let db = vec![node("r1", NodeFlavor::Fanout, NodeState::Healthy)];
        let actions = sync_actions(&db, &[]);
        assert_eq!(actions.upserts, db);
        assert!(actions.evictions.is_empty());
    }

    #[test]
    fn gone_and_draining_rows_are_absent_from_the_upsert_and_evict_the_pool_entry() {
        for state in [NodeState::Gone, NodeState::Draining] {
            let db = vec![node("r1", NodeFlavor::Fanout, state)];
            let pool_nodes = vec![NodeId::new("r1")];
            let actions = sync_actions(&db, &pool_nodes);
            assert!(
                actions.upserts.is_empty(),
                "{state:?} must not be upserted"
            );
            assert_eq!(actions.evictions, vec![NodeId::new("r1")]);
        }
    }

    #[test]
    fn a_booting_or_requested_row_is_not_upserted_but_is_not_evicted_if_absent() {
        // Not yet healthy, and not in the pool either: nothing to do, in either
        // direction — there is no stale pool entry to clean up.
        let db = vec![node("r1", NodeFlavor::Fanout, NodeState::Booting)];
        let actions = sync_actions(&db, &[]);
        assert!(actions.upserts.is_empty());
        assert!(actions.evictions.is_empty());
    }

    #[test]
    fn transcode_flavor_is_ignored_even_when_healthy() {
        let db = vec![node("t1", NodeFlavor::Transcode, NodeState::Healthy)];
        let actions = sync_actions(&db, &[]);
        assert!(
            actions.upserts.is_empty(),
            "a transcoder serves no WebRTC viewers and must never be fanout-pooled"
        );
    }

    #[test]
    fn a_pool_entry_for_a_transcoder_that_appeared_in_the_pool_is_evicted() {
        let db = vec![node("t1", NodeFlavor::Transcode, NodeState::Healthy)];
        let pool_nodes = vec![NodeId::new("t1")];
        let actions = sync_actions(&db, &pool_nodes);
        assert_eq!(actions.evictions, vec![NodeId::new("t1")]);
    }

    #[test]
    fn edge_flavor_is_ignored_even_when_healthy() {
        let db = vec![node("e1", NodeFlavor::Edge, NodeState::Healthy)];
        let actions = sync_actions(&db, &[]);
        assert!(actions.upserts.is_empty());
    }

    #[test]
    fn an_empty_database_evicts_every_pool_entry() {
        let pool_nodes = vec![NodeId::new("r1"), NodeId::new("r2")];
        let actions = sync_actions(&[], &pool_nodes);
        assert!(actions.upserts.is_empty());
        let mut got = actions.evictions.clone();
        got.sort();
        assert_eq!(got, vec![NodeId::new("r1"), NodeId::new("r2")]);
    }

    #[test]
    fn a_healthy_row_already_in_the_pool_is_upserted_again_not_evicted() {
        // Re-registering an existing id is how the pool picks up a refreshed
        // viewer count; it must never be read as "this node is gone".
        let db = vec![node("r1", NodeFlavor::Fanout, NodeState::Healthy)];
        let pool_nodes = vec![NodeId::new("r1")];
        let actions = sync_actions(&db, &pool_nodes);
        assert_eq!(actions.upserts, db);
        assert!(actions.evictions.is_empty());
    }

    /// THE ISOLATION PROPERTY of the seeding fan-out: a relay that never answers
    /// costs its own budget only. The healthy relay finishes, the hung one is
    /// reported timed out, and the whole fan-out returns at the budget — not at
    /// the HTTP stack's much longer connect/request timeouts, and not stacked
    /// per relay.
    #[tokio::test(start_paused = true)]
    async fn a_hung_relay_times_out_alone_and_does_not_stall_the_others() {
        let started = tokio::time::Instant::now();
        let healthy = async {
            tokio::time::sleep(StdDuration::from_secs(1)).await;
        };
        let hung = async {
            tokio::time::sleep(StdDuration::from_secs(3600)).await;
        };
        let timed_out = seed_relays_within_budget(
            vec![(NodeId::new("healthy"), Box::pin(healthy) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>),
                 (NodeId::new("hung"), Box::pin(hung))],
            StdDuration::from_secs(8),
        )
        .await;

        assert_eq!(timed_out, vec![NodeId::new("hung")]);
        assert!(
            started.elapsed() <= StdDuration::from_secs(9),
            "the fan-out must return at the budget, not wait out the hung relay \
             (elapsed {:?})",
            started.elapsed()
        );
    }

    /// Relays are seeded in parallel, not one after another: two relays that
    /// each take 6 s finish together in ~6 s. Sequentially they would take 12 s
    /// — longer than the 10 s tick, which is exactly the stall being prevented.
    #[tokio::test(start_paused = true)]
    async fn relays_are_seeded_concurrently_not_sequentially() {
        let started = tokio::time::Instant::now();
        let slow = |_: u32| async {
            tokio::time::sleep(StdDuration::from_secs(6)).await;
        };
        let timed_out = seed_relays_within_budget(
            vec![(NodeId::new("r1"), slow(1)), (NodeId::new("r2"), slow(2))],
            StdDuration::from_secs(8),
        )
        .await;

        assert!(timed_out.is_empty(), "6 s is inside an 8 s budget");
        assert!(
            started.elapsed() < StdDuration::from_secs(12),
            "two 6 s relays must overlap, not stack (elapsed {:?})",
            started.elapsed()
        );
    }

    #[test]
    fn a_mixed_batch_upserts_the_healthy_fanout_and_evicts_everything_else() {
        let db = vec![
            node("healthy", NodeFlavor::Fanout, NodeState::Healthy),
            node("draining", NodeFlavor::Fanout, NodeState::Draining),
            node("tx", NodeFlavor::Transcode, NodeState::Healthy),
        ];
        let pool_nodes = vec![
            NodeId::new("healthy"),
            NodeId::new("draining"),
            NodeId::new("tx"),
            NodeId::new("stale-gone-from-db"),
        ];
        let actions = sync_actions(&db, &pool_nodes);
        assert_eq!(
            actions.upserts,
            vec![node("healthy", NodeFlavor::Fanout, NodeState::Healthy)]
        );
        let mut evicted = actions.evictions.clone();
        evicted.sort();
        let mut expected = vec![
            NodeId::new("draining"),
            NodeId::new("tx"),
            NodeId::new("stale-gone-from-db"),
        ];
        expected.sort();
        assert_eq!(evicted, expected);
    }
}
