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
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration as StdDuration, SystemTime};

use mm_core::fleet::{FleetNode, NodeFlavor, NodeId, NodeState};
use mm_core::switch_client::{CascadeSeedOutcome, SwitchClient, mint_cascade_token, switch_source_id};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::fleet_census::SwitchCensus;
use crate::switch_pool::SwitchPool;
use mm_fleet::runner::BroadcastCensus;

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

/// Mint a cascade token and seed (or confirm) the relay's source for one
/// broadcast. On `Unauthorized` — the relay rejected our token — mints a fresh
/// one and tries exactly once more; this is the renewal path, and a relay that
/// rejects twice in a row logs rather than loops.
async fn seed_cascade_source(
    dialer: &SwitchClient,
    relay_base_url: &str,
    relay_node_id: &NodeId,
    origin_url: &str,
    secret: &str,
    broadcast_id: &str,
) {
    let source_id = switch_source_id(broadcast_id);
    for attempt in 0..2 {
        let token = mint_cascade_token(
            secret,
            broadcast_id,
            relay_node_id.as_str(),
            CASCADE_TOKEN_TTL,
            SystemTime::now(),
        );
        match dialer
            .add_cascade_source(relay_base_url, &source_id, origin_url, &token)
            .await
        {
            Ok(CascadeSeedOutcome::Created | CascadeSeedOutcome::AlreadyExists) => return,
            Ok(CascadeSeedOutcome::Unauthorized) => {
                tracing::warn!(
                    relay = %relay_node_id,
                    broadcast = broadcast_id,
                    attempt,
                    "cascade seed: relay rejected the token as unauthorized, renewing"
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
        "cascade seed: relay rejected the token as unauthorized twice in a row — giving up this tick"
    );
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
        let client = Arc::new(SwitchClient::new(&base_url));
        pool.upsert(node, client).await;
    }

    // Seed cascade sources: the origin is always reachable even with no fleet,
    // so viewer-counting for seeding reuses the same census the fleet planner
    // reads from.
    let Some(secret) = switch_secret else {
        return;
    };
    let census = SwitchCensus::new(pool.clone(), db.clone());
    let live = match census.live_broadcasts().await {
        Ok(live) => live,
        Err(e) => {
            tracing::warn!("pool sync: census failed, skipping cascade seeding this tick: {e}");
            return;
        }
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
        if let Ok(Some(relay_id)) = res {
            timed_out.push(relay_id);
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
