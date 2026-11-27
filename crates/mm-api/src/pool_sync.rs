//! Pool-sync loop (relay fleet, Task 3): keeps [`SwitchPool`] in step with
//! `mm_fleet_nodes`, and keeps the relay tree: which relay carries which stream,
//! and which node feeds it.
//!
//! The pure decisions, kept apart from the I/O on purpose:
//!
//! * [`sync_actions`] is the diff between what the database says exists and
//!   what the pool already holds.
//! * [`plan_feeds`] is the relay tree's diff: which feed rows
//!   (`mm_fleet_relay_feeds`) to drop and which to add, each new relay fed by
//!   the closest relay already carrying the stream (`feeder_for`) or by the
//!   stream's entry.
//!
//! [`run_pool_sync`] is the impure shell: it reads `mm_fleet_nodes`, applies
//! `sync_actions` to the pool, takes the census, and applies `plan_feeds` to
//! the relays: every row that stays is seeded on every tick (R2), a removal (sent
//! only where nobody can be watching) removes the source, and a row is written only
//! when a seed created or re-created the source, so rows record where sources
//! really pull from.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use mm_core::config::{FleetMode, MediaPath};
use mm_core::config_handle::ConfigHandle;
use mm_core::fleet::media_path::{Carrier, Feeder, MAX_RELAY_DEPTH, feeder_for, relay_wanted};
use mm_core::fleet::{FleetNode, NodeFlavor, NodeId, NodeState};
use mm_core::switch_client::{CascadeSeedOutcome, SwitchClient, mint_cascade_token, switch_source_id};
use sqlx::PgPool;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::fleet_census::{CensusPass, SwitchCensus};
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
/// dial a node, so it was never taught to read `public_ip` — and the node's
/// zone and region, which choose its feeder in the relay tree, and its destroy deadline,
/// which keeps new viewers off it in its last minutes.
struct FanoutRow {
    node: FleetNode,
    public_ip: Option<String>,
    zone: Option<String>,
    region: Option<String>,
    destroy_deadline: Option<chrono::DateTime<chrono::Utc>>,
}

async fn load_fanout_rows(db: &PgPool) -> Result<Vec<FanoutRow>, String> {
    type Row = (
        String,         // mm_node_id
        String,         // flavor
        String,         // ownership
        String,         // state
        Option<String>, // public_ip (address only; `host()` strips inet's /32)
        Option<i32>,    // viewer_capacity
        i32,            // viewers_current
        String,         // funding
        Option<String>, // provider zone
        Option<String>, // that zone's region
        Option<chrono::DateTime<chrono::Utc>>, // destroy_deadline (rented nodes only)
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT n.mm_node_id, n.flavor, n.ownership, n.state, host(n.public_ip)::text,
                n.viewer_capacity, n.viewers_current, n.funding, n.provider_zone, z.region,
                n.destroy_deadline
           FROM mm_fleet_nodes n
           LEFT JOIN mm_fleet_provider_zones z
                  ON z.provider_id = n.provider_ref AND z.zone = n.provider_zone
          ORDER BY n.mm_node_id",
    )
    .fetch_all(db)
    .await
    .map_err(|e| format!("pool sync: loading mm_fleet_nodes failed: {e}"))?;

    Ok(rows
        .into_iter()
        .filter_map(
            |(
                id,
                flavor,
                ownership,
                state,
                public_ip,
                cap,
                cur,
                funding,
                zone,
                region,
                deadline,
            )| {
                Some(FanoutRow {
                    node: FleetNode {
                        id: NodeId::new(id),
                        flavor: NodeFlavor::parse(&flavor)?,
                        ownership: mm_core::fleet::Ownership::parse(&ownership)?,
                        state: NodeState::parse(&state)?,
                        viewer_capacity: cap.unwrap_or(0).max(0) as u32,
                        viewers_current: cur.max(0) as u32,
                        funding: mm_core::fleet::funding::Funding::parse(&funding)?,
                    },
                    public_ip,
                    zone,
                    region,
                    destroy_deadline: deadline,
                })
            },
        )
        .collect())
}

/// A relay in the pool this tick, as the relay tree sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayPlace {
    pub id: NodeId,
    /// The relay's switch: where it is seeded, and what a relay it feeds dials.
    pub base_url: String,
    pub zone: Option<String>,
    pub region: Option<String>,
    /// Connected sessions on the node right now, relay links included.
    pub viewers: u32,
}

/// One row of `mm_fleet_relay_feeds`: `node_id` carries `stream_id`, pulling from
/// `feeder`. `depth` is an upper bound on its hops from the stream's entry. A `None`
/// feeder at depth 1 pulls from the entry; a `None` feeder at depth `MAX_RELAY_DEPTH` is
/// an adopted leaf: it carries the stream, where it pulls from is unknown, and it never
/// feeds anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedRow {
    pub stream_id: String,
    pub node_id: NodeId,
    pub feeder: Option<NodeId>,
    pub depth: u8,
}

/// What one tick does to the relay tree.
///
/// A row records where a relay's source ACTUALLY pulls from. It is written only when a
/// seed reports that it created the source, or re-created it after the dead-source rule
/// fired, and then with the feeder that seed used. So the plan never re-points a row.
///
/// A removal of a cascade source ends the programme for everyone on that relay
/// (mm-switch treats every `stream-…` id as a programme), so only `remove` ever calls
/// the switch's removal, and only for a row nobody can be watching through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedPlan {
    /// New rows: seeded against their feeder, written only if the seed created the
    /// source.
    pub add: Vec<FeedRow>,
    /// Stored rows that stay as they are and are seeded against their own feeder: those
    /// whose chain reaches the entry, and those below an orphan.
    pub keep: Vec<FeedRow>,
    /// For each ORPHAN (a row that stays, but whose feeder is no longer a relay or has no
    /// stored row, or whose hop to its feeder is not trusted by the depth rule, or whose
    /// chain loops): the row it gets if its source has to be created again. The stored
    /// row is left as it is; the orphan is seeded against this row's feeder, and this
    /// row is written only if that seed created or re-created the source.
    pub fallback: Vec<FeedRow>,
    /// (stream id, node id) dropped for good on a relay: the stream is not live, or it
    /// is not wanted and not held there. The source is removed, then the row. Deepest
    /// rows of each stream first.
    pub remove: Vec<(String, NodeId)>,
    /// (stream id, node id) whose node is no longer a relay: only the row goes. There is
    /// nothing to call, and a draining node serves whoever is still on it.
    pub forget: Vec<(String, NodeId)>,
}

/// Where a stored row that stays stands in its stream's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Its chain of rows reaches the entry.
    Rooted,
    /// Its feeder is no longer a relay or has no stored row; or its recorded depth is not
    /// above its feeder's, so the hop is not trusted (see `standing`); or its chain loops.
    Orphan,
    /// Its chain reaches an orphan.
    BelowOrphan,
}

/// The relay tree's diff for one tick. Pure.
///
/// * `relays`: the pool's relays this tick.
/// * `wanted_streams`: streams new relays may join (live, stored state not `core`,
///   fleet `on`, [`relay_wanted`]).
/// * `held`: (stream, node) pairs where people may be watching the stream through the
///   node (see [`held_pairs`]).
/// * `existing`: the stored rows.
///
/// Wanted only decides who JOINS; it never cuts anyone off. A stored row stays while its
/// node is still a relay and its stream is wanted or the pair is held, or while a row
/// that stays pulls from it; a row on a node that is no longer a relay is forgotten, any
/// other row is removed. Rows that stay are never changed here (see [`FeedPlan`]).
///
/// Carriers (what [`feeder_for`] may choose from) are only STORED rows that stay, are
/// rooted (their chain reaches the entry through hops the depth rule trusts) and sit
/// below the depth limit. A relay added in this plan carries nothing until its row is
/// written; an orphan and everything below it carry nothing, so nobody joins under a
/// branch whose top lost its feeder. Relays are visited in id order:
/// * a relay with no row joins a wanted stream, fed by [`feeder_for`] over the carriers
///   (depth = the feeder's + 1, or 1 for the entry). At most one such add per stream per
///   plan takes the entry, chosen by `turn` among those whose only choice is the entry
///   (in id order, `turn % count`); the others wait a tick. A relay
///   that rows still pull from is bounded to sit above the shallowest of them.
/// * an orphan gets a fallback: [`feeder_for`] over the carriers that are not below it
///   and sit above its recorded depth, else the entry. So its depth never grows, and the
///   recorded depths below it stay upper bounds of where those relays really are.
pub fn plan_feeds(
    relays: &[RelayPlace],
    wanted_streams: &[String],
    held: &HashSet<(String, NodeId)>,
    existing: &[FeedRow],
    turn: u64,
) -> FeedPlan {
    let mut relays_in_order: Vec<&RelayPlace> = relays.iter().collect();
    relays_in_order.sort_by(|a, b| a.id.cmp(&b.id));
    let relay_by_id: HashMap<&NodeId, &RelayPlace> = relays.iter().map(|r| (&r.id, r)).collect();
    let wanted: HashSet<&str> = wanted_streams.iter().map(String::as_str).collect();
    let streams: BTreeSet<&str> = wanted
        .iter()
        .copied()
        .chain(existing.iter().map(|r| r.stream_id.as_str()))
        .collect();

    let mut plan = FeedPlan::default();
    for stream in streams {
        let is_wanted = wanted.contains(stream);
        let mut rows: Vec<&FeedRow> = existing.iter().filter(|r| r.stream_id == stream).collect();
        rows.sort_by(|a, b| a.node_id.cmp(&b.node_id));

        // A row stays when its node is still a relay and the stream is wanted or the
        // pair is held, or when a row that stays pulls from it: a feeder is held by its
        // rows, not only by what one census saw (a reconnecting relay link drops off its
        // feeder's list for a moment). Leaves empty first: once the last row below a
        // feeder goes, the feeder is judged on its own in this same plan.
        let row_of: HashMap<&NodeId, &FeedRow> = rows.iter().map(|r| (&r.node_id, *r)).collect();
        let mut staying_nodes: HashSet<&NodeId> = rows
            .iter()
            .filter(|r| {
                relay_by_id.contains_key(&r.node_id)
                    && (is_wanted || held.contains(&(stream.to_string(), r.node_id.clone())))
            })
            .map(|r| &r.node_id)
            .collect();
        loop {
            let feeders: Vec<&NodeId> = rows
                .iter()
                .filter(|r| staying_nodes.contains(&r.node_id))
                .filter_map(|r| r.feeder.as_ref())
                .filter_map(|f| row_of.get(f).map(|fr| &fr.node_id))
                .filter(|f| relay_by_id.contains_key(f) && !staying_nodes.contains(f))
                .collect();
            if feeders.is_empty() {
                break;
            }
            staying_nodes.extend(feeders);
        }
        let stays = |row: &FeedRow| staying_nodes.contains(&row.node_id);
        let staying: HashMap<&NodeId, &FeedRow> = rows
            .iter()
            .filter(|r| stays(r))
            .map(|r| (&r.node_id, *r))
            .collect();
        // Walk up the rows that stay: to the entry (rooted), or to a row whose feeder has
        // no row that stays (that row is the orphan). A loop counts as an orphan. A hop is
        // trusted only when the row's recorded depth is above its feeder's: recorded
        // depths are upper bounds of where relays really sit, and a row at or above its
        // feeder's (one that names a relay adopted later at the depth limit) no longer is
        // one, so it is treated as having lost its feeder.
        let standing = |row: &FeedRow| {
            let mut cur = row;
            for _ in 0..=rows.len() {
                match &cur.feeder {
                    None => return Standing::Rooted,
                    Some(up) => match staying.get(up).filter(|next| next.depth < cur.depth) {
                        Some(next) => cur = next,
                        None if std::ptr::eq(cur, row) => return Standing::Orphan,
                        None => return Standing::BelowOrphan,
                    },
                }
            }
            Standing::Orphan
        };
        // The relays whose stored chain passes through `node`.
        let below = |node: &NodeId| -> HashSet<NodeId> {
            rows.iter()
                .filter(|r| {
                    let mut up = r.feeder.as_ref();
                    for _ in 0..=rows.len() {
                        match up {
                            Some(f) if f == node => return true,
                            Some(f) => up = staying.get(f).and_then(|n| n.feeder.as_ref()),
                            None => return false,
                        }
                    }
                    false
                })
                .map(|r| r.node_id.clone())
                .collect()
        };

        let mut carriers: Vec<Carrier> = Vec::new();
        let mut orphans: HashMap<&NodeId, &FeedRow> = HashMap::new();
        let mut remove: Vec<&FeedRow> = Vec::new();
        for row in &rows {
            if !stays(row) {
                if relay_by_id.contains_key(&row.node_id) {
                    remove.push(row);
                } else {
                    plan.forget.push((stream.to_string(), row.node_id.clone()));
                }
                continue;
            }
            match standing(row) {
                Standing::Rooted => {
                    // A row at the depth limit feeds nothing: neither the deepest relays
                    // nor a leaf adopted with an unknown origin (recorded at the limit).
                    // `feeder_for` skips them as well; this keeps them out of every
                    // carrier list, fallbacks and bounded adds included.
                    if row.depth < MAX_RELAY_DEPTH {
                        carriers.push(carrier(relay_by_id[&row.node_id], row.depth));
                    }
                    plan.keep.push((*row).clone());
                }
                Standing::BelowOrphan => plan.keep.push((*row).clone()),
                Standing::Orphan => {
                    orphans.insert(&row.node_id, row);
                }
            }
        }
        remove.sort_by(|a, b| b.depth.cmp(&a.depth).then_with(|| a.node_id.cmp(&b.node_id)));
        plan.remove
            .extend(remove.into_iter().map(|r| (stream.to_string(), r.node_id.clone())));

        // Only stored rows are carriers: a row is a confirmed fact (a seed reported it
        // created the source, pulling from that feeder), a planned row is not, and a
        // relay feeding others from an unconfirmed place can put them deeper than
        // recorded. So a relay added below feeds nobody in this plan; it becomes a
        // carrier on the next tick, once its row is written. To keep relays that join
        // together from each taking a copy from the entry, at most one ORDINARY add per
        // stream per plan may be fed by the entry; any other whose only choice is the
        // entry waits for the next tick, when it can pull from the one that went first.
        // The tree thus grows by one confirmed level per tick. The bounded add (a relay
        // that rows still pull from) and orphan fallbacks are repairs, not limited here; an
        // orphan whose fallback is the entry is limited where its dead source would be
        // replaced, one per stream and tick ([`EntryClaims`]).
        //
        // Which relay takes the entry ROTATES with `turn`: a relay that is in the pool
        // but cannot be seeded would otherwise take the slot on every tick, and every
        // other relay of the stream would wait behind it for good. `turn` comes from the
        // wall clock (see `sync_once`), not a per-process counter, because two server
        // instances that picked different relays would each send one a copy from the
        // entry. With a stored carrier present nobody needs the entry, so the rotation
        // changes nothing.
        let mut adds: Vec<FeedRow> = Vec::new();
        let mut entry_only: Vec<FeedRow> = Vec::new();
        for place in &relays_in_order {
            if let Some(orphan) = orphans.get(&place.id) {
                let under = below(&place.id);
                let candidates: Vec<Carrier> = carriers
                    .iter()
                    .filter(|c| !under.contains(&c.id) && c.depth < orphan.depth)
                    .cloned()
                    .collect();
                let (feeder, depth) = feed_from(place, &candidates);
                plan.fallback.push(FeedRow {
                    stream_id: stream.to_string(),
                    node_id: place.id.clone(),
                    feeder,
                    depth,
                });
            } else if is_wanted && !staying.contains_key(&place.id) {
                // Rows that stay may still pull from this relay (its own row was lost).
                // Then it is bounded like an orphan: at most one above the shallowest of
                // them, so their recorded depths stay upper bounds; else the entry.
                let children = rows
                    .iter()
                    .filter(|r| stays(r) && r.feeder.as_ref() == Some(&place.id))
                    .map(|r| r.depth)
                    .min();
                let candidates: Vec<Carrier> = match children {
                    Some(shallowest) => carriers
                        .iter()
                        .filter(|c| c.depth + 1 < shallowest)
                        .cloned()
                        .collect(),
                    None => carriers.clone(),
                };
                let (feeder, depth) = feed_from(place, &candidates);
                let row = FeedRow {
                    stream_id: stream.to_string(),
                    node_id: place.id.clone(),
                    feeder,
                    depth,
                };
                if row.feeder.is_none() && children.is_none() {
                    entry_only.push(row);
                } else {
                    adds.push(row);
                }
            }
        }
        if !entry_only.is_empty() {
            // Usize fits any count of relays; the remainder is below it.
            let pick = (turn % entry_only.len() as u64) as usize;
            adds.push(entry_only.swap_remove(pick));
            adds.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        }
        plan.add.extend(adds);
    }
    plan
}

/// Which kind of row a seed in the ensure step is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ensure {
    /// A row the plan adds: no stored row describes this relay's source.
    Add,
    /// A stored row (kept).
    Stored,
    /// An orphan's fallback. Written like a stored row; when it is the entry, a dead source
    /// is replaced from it only by the one orphan of the stream that claims the entry this
    /// tick ([`EntryClaims`]).
    Fallback,
}

/// The row to write after a seed, if any. `planned` names the feeder the seed used.
///
/// A row records where a source really pulls from, so it is written when the seed
/// created or re-created the source (a relay fed by the entry is then at depth 1).
///
/// The one exception: an ADD whose seed finds the source already there. The relay holds
/// the stream with no row describing it (a seed cut off before its row was written: a
/// transport error after the switch registered it, the budget dropping the job, a failed
/// database write). Without a row it could never be dropped or removed and its link would
/// keep its feeder looking busy. So it is adopted as a leaf of unknown origin: feeder
/// unknown (`None`, so it is ensured against the entry and re-created from there if it
/// dies), depth `MAX_RELAY_DEPTH`. That is safe: a recorded depth is an upper bound, and
/// at the limit the row is never a carrier, so it can never be anyone's ancestor. An
/// adopted row is recorded only where no row exists ([`adopt_feed_row`]).
fn row_to_write(kind: Ensure, planned: &FeedRow, seeded: Seeded) -> Option<FeedRow> {
    match seeded {
        Seeded::Created | Seeded::Recreated => Some(FeedRow {
            depth: if planned.feeder.is_none() { 1 } else { planned.depth },
            ..planned.clone()
        }),
        Seeded::AlreadyThere(_) if kind == Ensure::Add => Some(FeedRow {
            feeder: None,
            depth: MAX_RELAY_DEPTH,
            ..planned.clone()
        }),
        Seeded::AlreadyThere(_) | Seeded::Failed => None,
    }
}

/// [`feeder_for`] over `carriers`, with the depth the relay would then have.
fn feed_from(place: &RelayPlace, carriers: &[Carrier]) -> (Option<NodeId>, u8) {
    match feeder_for(place.zone.as_deref(), place.region.as_deref(), carriers) {
        Feeder::Entry => (None, 1),
        Feeder::Relay(id) => {
            let up = carriers.iter().find(|c| c.id == id).map_or(0, |c| c.depth);
            (Some(id), up + 1)
        }
    }
}

/// The (stream, node) pairs where people may be watching a live stream through a node:
/// at least one session of it is LISTED there, connected or not (a session still
/// negotiating is listed before it connects; mm-switch drops one on Disconnected, Failed
/// or Closed), relay links included; or the node did not
/// answer the census; or someone on it is on an ad break, which names no programme. The
/// last two hold every live stream the node has a row for, because what they serve is
/// unknown, and guessing low would remove a source people are on.
fn held_pairs(pass: &CensusPass, existing: &[FeedRow]) -> HashSet<(String, NodeId)> {
    let live: HashSet<&str> = pass.live.iter().map(|b| b.broadcast_id.as_str()).collect();
    let mut held: HashSet<(String, NodeId)> = pass
        .listed
        .iter()
        .filter(|(_, broadcast)| live.contains(broadcast.as_str()))
        .map(|(node, broadcast)| (broadcast.clone(), node.clone()))
        .collect();
    held.extend(
        existing
            .iter()
            .filter(|r| {
                live.contains(r.stream_id.as_str())
                    && (pass.unanswered.contains(&r.node_id)
                        || pass.ad_break_nodes.contains(&r.node_id))
            })
            .map(|r| (r.stream_id.clone(), r.node_id.clone())),
    );
    held
}

fn carrier(place: &RelayPlace, depth: u8) -> Carrier {
    Carrier {
        id: place.id.clone(),
        zone: place.zone.clone(),
        region: place.region.clone(),
        viewers: place.viewers,
        depth,
    }
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
///
/// Returns what this call did to the relay's source ([`Seeded`]). Only `Created` and
/// `Recreated` mean the source now pulls from `origin_url`.
#[allow(clippy::too_many_arguments)]
async fn seed_cascade_source(
    dialer: &SwitchClient,
    relay_base_url: &str,
    relay_node_id: &NodeId,
    origin_url: &str,
    secret: &str,
    broadcast_id: &str,
    inactive: &InactiveTracker,
    grace: StdDuration,
    from_entry: Option<&EntryClaims>,
) -> Seeded {
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
                return Seeded::Created;
            }
            Ok(CascadeSeedOutcome::AlreadyExists) => {
                return reconcile_existing_cascade_source(
                    dialer,
                    relay_base_url,
                    relay_node_id,
                    origin_url,
                    &source_id,
                    &token,
                    inactive,
                    grace,
                    from_entry,
                )
                .await;
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
                return Seeded::Failed;
            }
        }
    }
    tracing::error!(
        relay = %relay_node_id,
        broadcast = broadcast_id,
        "cascade seed: relay rejected the server bearer twice in a row — giving up this \
         tick; check MM_SWITCH_AUTH_SECRET on this relay, not the cascade token"
    );
    Seeded::Failed
}

/// What one seed call did to a relay's cascade source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seeded {
    /// The relay had no such source; this call created it, pulling from the URL given.
    Created,
    /// The source was there but dead past the grace period; this call removed it and
    /// created it again, pulling from the URL given.
    Recreated,
    /// The source is there and was left as it is (alive, or not dead for long enough),
    /// still pulling from wherever it was created to pull from; with what the relay's
    /// source list said about it.
    AlreadyThere(Liveness),
    /// The relay could not be made to hold the source this tick.
    Failed,
}

/// What a relay's source list said about a source that was already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// Listed active: the relay has the picture.
    Active,
    /// Not active (no track yet, or dead and not yet past the grace period, or about to
    /// be re-created): no picture.
    NotActive,
    /// The source list could not be read this tick.
    Unread,
}

/// The streams a relay carries after this tick's ensure, for viewer placement
/// ([`SwitchPool::set_carried`]). `ensured` are the streams the relay's job was to ensure,
/// `seeded` what each seed reported (a stream is missing when its job ran out of budget
/// first), `before` what the relay carried after the last tick.
///
/// A stream is carried when its source read active. One just created or re-created has no
/// picture until it reads active (normally on the next tick); one inactive, or whose seed
/// failed, is not carried; one whose state could not be read this tick keeps last tick's
/// answer. A stream the relay was not to ensure (removed, or no longer planned there) is
/// not carried: a feed row alone records where a source pulls from, not a picture.
fn carried_after(
    ensured: &[String],
    seeded: &HashMap<String, Seeded>,
    before: &HashSet<String>,
) -> HashSet<String> {
    ensured
        .iter()
        .filter(|stream| match seeded.get(*stream) {
            Some(Seeded::AlreadyThere(Liveness::Active)) => true,
            Some(Seeded::AlreadyThere(Liveness::Unread)) | None => before.contains(*stream),
            Some(
                Seeded::AlreadyThere(Liveness::NotActive)
                | Seeded::Created
                | Seeded::Recreated
                | Seeded::Failed,
            ) => false,
        })
        .cloned()
        .collect()
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
    grace: StdDuration,
    from_entry: Option<&EntryClaims>,
) -> Seeded {
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
            return Seeded::AlreadyThere(Liveness::Unread);
        }
    };
    let active = sources.iter().any(|s| s.id == source_id && s.active);

    if active {
        inactive.lock().await.remove(&key);
        return Seeded::AlreadyThere(Liveness::Active);
    }

    let now = Instant::now();
    let mut guard = inactive.lock().await;
    let since = *guard.entry(key.clone()).or_insert(now);
    let dead_for = now.duration_since(since);
    drop(guard);

    if dead_for < grace {
        return Seeded::AlreadyThere(Liveness::NotActive);
    }

    // An orphan replaced from the entry makes the main server send it a copy for the rest of
    // the stream. One orphan per stream and tick may: the others are left exactly as they are,
    // and on the next tick take that one, by then a stored first-level relay, as their
    // fallback. Claimed only here, where the source is known to be dead and its removal is
    // the next step.
    if let Some(claims) = from_entry
        && !claims.claim(source_id)
    {
        tracing::info!(
            relay = %relay_node_id,
            source = source_id,
            "cascade seed: another relay of this stream is re-created from the entry this \
             tick; this dead source waits for the next one"
        );
        return Seeded::AlreadyThere(Liveness::NotActive);
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
        return Seeded::AlreadyThere(Liveness::NotActive);
    }
    inactive.lock().await.remove(&key);

    match dialer
        .add_cascade_source(relay_base_url, source_id, origin_url, fresh_token, relay_node_id.as_str())
        .await
    {
        Ok(CascadeSeedOutcome::Created) => Seeded::Recreated,
        // Someone else's seed got there between our removal and this one: whatever it
        // pulls from, it was not this call's doing. Just created, it has no picture yet.
        Ok(CascadeSeedOutcome::AlreadyExists) => Seeded::AlreadyThere(Liveness::NotActive),
        Ok(CascadeSeedOutcome::Unauthorized) => {
            tracing::warn!(
                relay = %relay_node_id,
                source = source_id,
                "cascade seed: re-seeding a deleted dead source was refused as unauthorized"
            );
            Seeded::Failed
        }
        Err(e) => {
            tracing::warn!(
                relay = %relay_node_id,
                source = source_id,
                error = %e,
                "cascade seed: re-seeding a deleted dead source failed"
            );
            Seeded::Failed
        }
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

/// What the pool-sync loop needs besides the pool and the database.
#[derive(Clone)]
pub struct PoolSyncContext {
    /// The live configuration: `fleet.mode` and `fleet.relay_viewer_threshold` are read
    /// from it on every tick, never from a value captured at boot.
    pub live: ConfigHandle,
    /// `MM_SWITCH_AUTH_SECRET`. Without it cascade tokens cannot be minted, so membership
    /// is still synced but the relay tree is left alone.
    pub switch_secret: Option<String>,
    /// The origin's public switch base, which relays fed by the entry dial.
    pub public_switch_base_url: Option<String>,
    /// A relay's switch base URL from its `public_ip`.
    relay_base_url: Arc<dyn Fn(&str) -> String + Send + Sync>,
    /// The sync interval, which numbers the turns of the relay tree's entry slot.
    tick: StdDuration,
    /// How long a relay's source may read dead before it is replaced
    /// ([`INACTIVE_GRACE`]).
    inactive_grace: StdDuration,
}

impl PoolSyncContext {
    pub fn new(
        live: ConfigHandle,
        switch_secret: Option<String>,
        public_switch_base_url: Option<String>,
    ) -> Self {
        Self {
            live,
            switch_secret,
            public_switch_base_url,
            relay_base_url: Arc::new(|ip| format!("http://{ip}:{SWITCH_PORT}")),
            tick: StdDuration::from_secs(10),
            inactive_grace: INACTIVE_GRACE,
        }
    }

    /// Set how long a relay's source may read dead before it is replaced. For tests, which
    /// cannot wait out [`INACTIVE_GRACE`].
    pub fn with_inactive_grace(mut self, grace: StdDuration) -> Self {
        self.inactive_grace = grace;
        self
    }

    /// Set the sync interval the entry slot's turn counts. `run_pool_sync` sets it from
    /// its own interval. Tests set a very long one to keep the turn at 0, so the lowest id
    /// among the relays needing the entry takes it.
    pub fn with_tick(mut self, tick: StdDuration) -> Self {
        self.tick = tick;
        self
    }

    /// The relay tree's turn at `now`: the wall clock divided by the sync interval, so it
    /// advances by one each tick and two server instances agree on it (see
    /// [`plan_feeds`]).
    fn turn(&self, now: SystemTime) -> u64 {
        let since_epoch = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        let tick_ms = self.tick.as_millis().max(1);
        u64::try_from(since_epoch.as_millis() / tick_ms).unwrap_or(u64::MAX)
    }

    /// Replace how a relay's address becomes its switch URL. For tests, whose stand-in
    /// switches cannot all listen on the one port real relays use.
    pub fn with_relay_base_url(mut self, f: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.relay_base_url = Arc::new(f);
        self
    }
}

/// One pass: diff and apply to the pool, take the census, then bring the relay tree
/// in line with it.
pub async fn sync_once(pool: &Arc<SwitchPool>, db: &PgPool, ctx: &PoolSyncContext) {
    let rows = match load_fanout_rows(db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("pool sync: {e}");
            return;
        }
    };
    let db_nodes: Vec<FleetNode> = rows.iter().map(|r| r.node.clone()).collect();
    let pool_node_ids: Vec<NodeId> = pool.nodes().await.into_iter().map(|n| n.id).collect();

    let actions = sync_actions(&db_nodes, &pool_node_ids);

    for id in &actions.evictions {
        pool.evict(id).await;
        tracing::info!(node = %id, "pool sync: evicted");
    }

    // The relays upserted this tick, with their switch URL and place.
    let mut upserted: Vec<(&FanoutRow, String)> = Vec::new();
    for node in &actions.upserts {
        let Some(row) = rows.iter().find(|r| r.node.id == node.id) else {
            continue;
        };
        let Some(ip) = row.public_ip.as_deref() else {
            // A healthy row with no recorded address cannot be dialed; skip it
            // this tick rather than register a client nobody can reach.
            tracing::warn!(node = %node.id, "pool sync: healthy relay has no public_ip yet, skipping");
            continue;
        };
        let base_url = (ctx.relay_base_url)(ip);
        // B2: every server-role route on a relay (cascade seeding, token renewal,
        // switch_viewer, remove_source, …) is `wrapAuth(authSecret, serverOnly, …)` — it
        // demands mm-core's own bearer. A client built via `SwitchClient::new` never sends
        // one and 401s on all of them; `with_auth` is the client every relay call in this
        // loop must use whenever a secret is configured.
        let client: Arc<SwitchClient> = match ctx.switch_secret.as_deref() {
            Some(secret) => Arc::new(SwitchClient::with_auth(&base_url, secret.to_string())),
            None => Arc::new(SwitchClient::new(&base_url)),
        };
        pool.upsert_with_deadline(node, client, row.destroy_deadline)
            .await;
        upserted.push((row, base_url));
    }

    // Census (§8.2 R6 + m4): ONE gather of every node — origin and fan-out alike — feeds
    // both the capacity gauges' `viewers_current` write-back AND the relay tree below.
    // Gathered from the pool we just synced above (so a node just evicted is not polled,
    // and a node just upserted already is).
    let census = SwitchCensus::new(pool.clone(), db.clone());
    let pass = match census.viewers_per_node_and_live_broadcasts().await {
        Ok(pass) => pass,
        Err(e) => {
            tracing::error!("pool sync: census failed: {e}");
            return;
        }
    };
    // A relay that did not answer keeps the count it had: its zero means "unknown", and
    // writing it would make a relay people may be on look empty.
    let viewer_counts: Vec<(NodeId, i64)> = pass
        .per_node
        .iter()
        .filter(|(n, _)| !pass.unanswered.contains(*n))
        .map(|(n, c)| (n.clone(), *c))
        .collect();
    if let Err(e) = nodes_db::update_viewers_current(db, &viewer_counts).await {
        tracing::warn!("pool sync: writing back viewers_current failed: {e}");
    }
    // Viewer placement reads each stream's audience from here: people only, never relay
    // links (the census's per-stream counts).
    pool.set_stream_viewers(
        pass.live
            .iter()
            .map(|b| (b.broadcast_id.clone(), b.viewers))
            .collect(),
    )
    .await;

    let Some(secret) = ctx.switch_secret.as_deref() else {
        return;
    };
    let relays: Vec<RelayPlace> = upserted
        .into_iter()
        .map(|(row, base_url)| RelayPlace {
            id: row.node.id.clone(),
            base_url,
            zone: row.zone.clone(),
            region: row.region.clone(),
            viewers: pass
                .per_node
                .get(&row.node.id)
                .copied()
                .unwrap_or(0)
                .clamp(0, i64::from(u32::MAX)) as u32,
        })
        .collect();
    let entry_url = cascade_origin_url(ctx.public_switch_base_url.as_deref(), pool);
    feed_relays(pool, db, ctx, secret, &relays, &pass, &entry_url).await;
}

/// The relay tree's step of a tick: read the stored rows and the streams' states, plan,
/// and apply the plan to the relays. Any read failing skips the step for this tick, so
/// nothing is dropped on a guess.
async fn feed_relays(
    pool: &Arc<SwitchPool>,
    db: &PgPool,
    ctx: &PoolSyncContext,
    secret: &str,
    relays: &[RelayPlace],
    pass: &CensusPass,
    entry_url: &str,
) {
    let existing = match load_feed_rows(db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("pool sync: {e}");
            return;
        }
    };
    let paths = match load_live_media_paths(db).await {
        Ok(paths) => paths,
        Err(e) => {
            tracing::error!("pool sync: {e}");
            return;
        }
    };

    let cfg = ctx.live.load();
    let wanted: Vec<String> = if cfg.fleet.mode == FleetMode::On {
        pass.live
            .iter()
            .filter(|b| {
                paths.get(&b.broadcast_id).copied().unwrap_or(MediaPath::Core) != MediaPath::Core
                    && relay_wanted(b.viewers, cfg.fleet.relay_viewer_threshold)
            })
            .map(|b| b.broadcast_id.clone())
            .collect()
    } else {
        Vec::new()
    };

    let held = held_pairs(pass, &existing);
    let plan = plan_feeds(relays, &wanted, &held, &existing, ctx.turn(SystemTime::now()));

    let place_of: HashMap<&NodeId, &RelayPlace> = relays.iter().map(|r| (&r.id, r)).collect();
    let origin_of = |feeder: &Option<NodeId>| match feeder {
        None => Some(entry_url.to_string()),
        Some(id) => place_of.get(id).map(|p| p.base_url.clone()),
    };

    for (stream, node) in &plan.forget {
        delete_feed_row(db, stream, node).await;
    }
    let mut work: HashMap<NodeId, NodeFeedWork> = HashMap::new();
    for (stream, node) in &plan.remove {
        work.entry(node.clone()).or_default().remove.push(stream.clone());
    }
    // Every row that stays is ensured: added and kept rows against their feeder, an
    // orphan against its fallback. Each carries the row to write if its seed creates or
    // re-creates the source: the row with the feeder that seed used.
    let adds = plan.add.iter().map(|r| (r, Ensure::Add));
    let stored = plan.keep.iter().map(|r| (r, Ensure::Stored));
    let fallbacks = plan.fallback.iter().map(|r| (r, Ensure::Fallback));
    for (row, kind) in adds.chain(stored).chain(fallbacks) {
        let Some(origin_url) = origin_of(&row.feeder) else {
            continue;
        };
        work.entry(row.node_id.clone())
            .or_default()
            .ensure
            .push((row.clone(), kind, origin_url));
    }

    // What each relay was to ensure, and what each seed reported, for placement below.
    let ensured: HashMap<NodeId, Vec<String>> = work
        .iter()
        .map(|(id, job)| {
            (
                id.clone(),
                job.ensure
                    .iter()
                    .map(|(r, _, _)| r.stream_id.clone())
                    .collect(),
            )
        })
        .collect();
    let seeded: SeedReports = Arc::default();
    let entry_claims = EntryClaims::default();

    let mut relay_jobs = Vec::new();
    for (node_id, job) in work {
        let Some(client) = pool.client_for_node(Some(&node_id)).await else {
            continue;
        };
        let db = db.clone();
        let secret = secret.to_string();
        let relay_id = node_id.clone();
        let reports = seeded.clone();
        let claims = entry_claims.clone();
        let grace = ctx.inactive_grace;
        let fut = async move {
            job.run(&client, &db, &relay_id, &secret, &reports, &claims, grace)
                .await
        };
        relay_jobs.push((node_id, fut));
    }
    for relay_id in seed_relays_within_budget(relay_jobs, SEED_BUDGET_PER_RELAY).await {
        tracing::warn!(
            relay = %relay_id,
            budget_secs = SEED_BUDGET_PER_RELAY.as_secs(),
            "relay feeding ran out of its per-relay budget — skipping the rest of this \
             relay's work; every step is idempotent and the next tick retries"
        );
    }

    // Which streams each relay now has the picture of. A relay the census could not reach
    // keeps what it had: one timeout does not take it away from viewer placement.
    let seeded = std::mem::take(&mut *seeded.lock().unwrap_or_else(|e| e.into_inner()));
    for relay in relays {
        if pass.unanswered.contains(&relay.id) {
            continue;
        }
        let to_ensure = ensured
            .get(&relay.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let reported = seeded.get(&relay.id).cloned().unwrap_or_default();
        let before = pool.carried_by(&relay.id).await;
        pool.set_carried(&relay.id, carried_after(to_ensure, &reported, &before))
            .await;
    }
}

/// The streams one of whose orphans took the entry this tick: at most one orphan per
/// stream and tick has its dead source replaced from the entry. Shared by the tick's relay
/// jobs, which run concurrently.
#[derive(Clone, Default)]
struct EntryClaims(Arc<std::sync::Mutex<HashSet<String>>>);

impl EntryClaims {
    /// Take `stream`'s entry for this tick: `true` for the first caller only.
    fn claim(&self, stream: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(stream.to_string())
    }
}

/// Each relay's seed reports this tick: (relay, stream) → what the seed did. Written as each
/// seed returns, so a relay that runs out of budget still reports the seeds it finished.
type SeedReports = Arc<std::sync::Mutex<HashMap<NodeId, HashMap<String, Seeded>>>>;

/// One relay's share of a tick's [`FeedPlan`]: removals first, then every row that
/// stays is ensured.
///
/// Ensuring seeds the source against the URL the row names (the feeder's, or the
/// entry's): a live source answers 409 and is left as it is, nothing written; one that
/// has read inactive past `INACTIVE_GRACE` is removed and created again from that URL
/// (`reconcile_existing_cascade_source`); a missing one is created. Only the last two
/// write the row, because only then does the source pull from that URL.
#[derive(Default)]
struct NodeFeedWork {
    /// Streams whose cascade source is removed, and their rows with it.
    remove: Vec<String>,
    /// (the row the seed is for, what kind of row it is, the URL it pulls from). What
    /// gets written is [`row_to_write`]'s call.
    ensure: Vec<(FeedRow, Ensure, String)>,
}

impl NodeFeedWork {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        self,
        client: &SwitchClient,
        db: &PgPool,
        relay_id: &NodeId,
        secret: &str,
        reports: &SeedReports,
        entry_claims: &EntryClaims,
        grace: StdDuration,
    ) {
        let relay_base_url = client.base_url().to_string();

        // Only rows nobody can be watching through reach this list (see `FeedPlan`).
        for stream in self.remove {
            let source_id = switch_source_id(&stream);
            match client.remove_source(&source_id).await {
                Ok(()) => {
                    inactive_tracker()
                        .lock()
                        .await
                        .remove(&(relay_id.clone(), source_id));
                    delete_feed_row(db, &stream, relay_id).await;
                }
                // The row stays, so the next tick tries again.
                Err(e) => tracing::warn!(
                    relay = %relay_id,
                    broadcast = %stream,
                    error = %e,
                    "relay feeding: removing a cascade source failed, keeping its row"
                ),
            }
        }
        // Within one relay the streams stay sequential: they share a connection, and a
        // relay slow for one stream is slow for all. A failed seed writes nothing; the
        // next tick tries again.
        for (row, kind, origin_url) in self.ensure {
            let seeded = seed_cascade_source(
                client,
                &relay_base_url,
                relay_id,
                &origin_url,
                secret,
                &row.stream_id,
                inactive_tracker(),
                grace,
                (kind == Ensure::Fallback && row.feeder.is_none()).then_some(entry_claims),
            )
            .await;
            if let Some(written) = row_to_write(kind, &row, seeded) {
                if matches!(seeded, Seeded::AlreadyThere(_)) {
                    adopt_feed_row(db, &written).await;
                } else {
                    write_feed_row(db, &written).await;
                }
            }
            reports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(relay_id.clone())
                .or_default()
                .insert(row.stream_id.clone(), seeded);
        }
    }
}

async fn load_feed_rows(db: &PgPool) -> Result<Vec<FeedRow>, String> {
    let rows: Vec<(String, String, Option<String>, i16)> = sqlx::query_as(
        "SELECT stream_id, node_id, feeder_node_id, depth FROM mm_fleet_relay_feeds",
    )
    .fetch_all(db)
    .await
    .map_err(|e| format!("loading mm_fleet_relay_feeds failed: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|(stream_id, node_id, feeder, depth)| FeedRow {
            stream_id,
            node_id: NodeId::new(node_id),
            feeder: feeder.map(NodeId::new),
            depth: u8::try_from(depth).unwrap_or(u8::MAX),
        })
        .collect())
}

/// The stored state of every active stream. An unreadable value reads as `core`.
async fn load_live_media_paths(db: &PgPool) -> Result<HashMap<String, MediaPath>, String> {
    mm_fleet::pool_autoscale::live_media_paths(db)
        .await
        .map_err(|e| format!("reading the streams' media paths failed: {e}"))
}

/// Record where a relay's source now pulls from, after a seed created or re-created it.
/// Overwrites a stored row for the same relay: the seed that just ran is the truth. Two
/// server instances seeding the same relay at once cannot both create the source (the
/// second gets 409, and at most adopts where no row exists).
async fn write_feed_row(db: &PgPool, row: &FeedRow) {
    if let Err(e) = sqlx::query(
        "INSERT INTO mm_fleet_relay_feeds (stream_id, node_id, feeder_node_id, depth)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (stream_id, node_id)
         DO UPDATE SET feeder_node_id = EXCLUDED.feeder_node_id, depth = EXCLUDED.depth",
    )
    .bind(&row.stream_id)
    .bind(row.node_id.as_str())
    .bind(row.feeder.as_ref().map(NodeId::as_str))
    .bind(i16::from(row.depth))
    .execute(db)
    .await
    {
        tracing::warn!(
            relay = %row.node_id,
            broadcast = %row.stream_id,
            error = %e,
            "relay feeding: recording a feed failed"
        );
    }
}

/// Record an adopted leaf (see [`row_to_write`]), only where no row exists. A row that
/// is there was written by a seed that created the source (another server instance's,
/// between this tick's read of the rows and its seed), so it names where the source
/// really pulls from and must not be replaced by "origin unknown".
async fn adopt_feed_row(db: &PgPool, row: &FeedRow) {
    if let Err(e) = sqlx::query(
        "INSERT INTO mm_fleet_relay_feeds (stream_id, node_id, feeder_node_id, depth)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (stream_id, node_id) DO NOTHING",
    )
    .bind(&row.stream_id)
    .bind(row.node_id.as_str())
    .bind(row.feeder.as_ref().map(NodeId::as_str))
    .bind(i16::from(row.depth))
    .execute(db)
    .await
    {
        tracing::warn!(
            relay = %row.node_id,
            broadcast = %row.stream_id,
            error = %e,
            "relay feeding: recording an adopted feed failed"
        );
    }
}

async fn delete_feed_row(db: &PgPool, stream_id: &str, node_id: &NodeId) {
    if let Err(e) =
        sqlx::query("DELETE FROM mm_fleet_relay_feeds WHERE stream_id = $1 AND node_id = $2")
            .bind(stream_id)
            .bind(node_id.as_str())
            .execute(db)
            .await
    {
        tracing::warn!(
            relay = %node_id,
            broadcast = %stream_id,
            error = %e,
            "relay feeding: deleting a feed row failed"
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
/// `SwitchPool` in step with `mm_fleet_nodes` and the relay tree in step with
/// the census.
///
/// `ctx.switch_secret` is `None` when `MM_SWITCH_AUTH_SECRET` is unset; cascade
/// tokens cannot be minted without it, so this loop still syncs node
/// membership but leaves the relay tree alone — same posture as the rest of the
/// switch integration when the secret is absent.
pub async fn run_pool_sync(
    pool: Arc<SwitchPool>,
    db: PgPool,
    ctx: PoolSyncContext,
    interval: StdDuration,
    cancel: CancellationToken,
) {
    // The turn counts ticks of THIS interval.
    let ctx = PoolSyncContext { tick: interval, ..ctx };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                sync_once(&pool, &db, &ctx).await;
                mm_core::metrics_global::heartbeat("pool_sync");
            }
        }
    }
}

/// The origin URL a fan-out relay dials for cascade subscription: the public
/// switch base when the deployment has one. Relays live outside the docker
/// network and cannot resolve its internal hostnames — seeding the pool's
/// internal origin URL makes every cascade subscription fail DNS. Without a
/// public base the internal URL is kept (today's behavior for single-host
/// and dev setups).
fn cascade_origin_url(public_switch_base_url: Option<&str>, pool: &SwitchPool) -> String {
    if let Some(public) = public_switch_base_url.filter(|s| !s.is_empty()) {
        return public.to_string();
    }
    pool.origin().base_url().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_core::fleet::Ownership;
    use mm_core::fleet::media_path::MAX_RELAY_DEPTH;

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
    fn cascade_origin_url_prefers_the_public_switch_base() {
        use mm_core::switch_client::SwitchClient;
        use std::sync::Arc;
        // Relays live outside the docker network: the pool's internal origin
        // URL does not resolve there, so the public base wins whenever the
        // deployment has one.
        let pool = SwitchPool::new(Arc::new(SwitchClient::new("http://mm-switch:7890")));
        assert_eq!(
            cascade_origin_url(Some("https://mm.example/_mm/switch"), &pool),
            "https://mm.example/_mm/switch"
        );
        assert_eq!(
            cascade_origin_url(None, &pool),
            "http://mm-switch:7890",
            "no public base keeps today's single-host behavior"
        );
        assert_eq!(
            cascade_origin_url(Some(""), &pool),
            "http://mm-switch:7890",
            "an empty public base is the same as none"
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

    // ── The relay tree (plan_feeds) ─────────────────────────────────────────

    const S: &str = "s1";

    fn relay(id: &str, zone: &str, viewers: u32) -> RelayPlace {
        RelayPlace {
            id: NodeId::new(id),
            base_url: format!("http://{id}:7890"),
            zone: Some(zone.to_string()),
            region: Some("eu".to_string()),
            viewers,
        }
    }

    fn feed(node: &str, feeder: Option<&str>, depth: u8) -> FeedRow {
        FeedRow {
            stream_id: S.to_string(),
            node_id: NodeId::new(node),
            feeder: feeder.map(NodeId::new),
            depth,
        }
    }

    fn wanted() -> Vec<String> {
        vec![S.to_string()]
    }

    fn held(nodes: &[&str]) -> HashSet<(String, NodeId)> {
        nodes
            .iter()
            .map(|n| (S.to_string(), NodeId::new(*n)))
            .collect()
    }

    fn nodes(pairs: &[(String, NodeId)]) -> Vec<&str> {
        let mut out: Vec<&str> = pairs.iter().map(|(_, n)| n.as_str()).collect();
        out.sort();
        out
    }

    fn kept(plan: &FeedPlan) -> Vec<&str> {
        let mut out: Vec<&str> = plan.keep.iter().map(|r| r.node_id.as_str()).collect();
        out.sort();
        out
    }

    /// Apply a plan the way the shell does when every seed of a NEW row creates its
    /// source, every stored row's source is alive (nothing written), and the sources of
    /// `reseeded` are re-created from their fallback.
    fn apply(existing: &[FeedRow], plan: &FeedPlan, reseeded: &[&str]) -> Vec<FeedRow> {
        let gone: HashSet<(&str, &str)> = plan
            .remove
            .iter()
            .chain(&plan.forget)
            .map(|(s, n)| (s.as_str(), n.as_str()))
            .collect();
        let mut rows: Vec<FeedRow> = existing
            .iter()
            .filter(|r| !gone.contains(&(r.stream_id.as_str(), r.node_id.as_str())))
            .map(|r| {
                plan.fallback
                    .iter()
                    .find(|f| f.stream_id == r.stream_id && f.node_id == r.node_id)
                    .filter(|f| reseeded.contains(&f.node_id.as_str()))
                    .cloned()
                    .unwrap_or_else(|| r.clone())
            })
            .collect();
        rows.extend(plan.add.iter().cloned());
        rows.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        rows
    }

    /// Every chain of rows ends at the entry or at a feeder with no row, never back at
    /// itself, and no recorded depth is above the limit.
    fn assert_tree(rows: &[FeedRow], step: &str) {
        let row_of: HashMap<&NodeId, &FeedRow> = rows.iter().map(|r| (&r.node_id, r)).collect();
        for row in rows {
            assert!(row.depth <= MAX_RELAY_DEPTH, "{step}: {row:?}");
            let mut seen = vec![&row.node_id];
            let mut up = row.feeder.as_ref();
            while let Some(f) = up {
                assert!(!seen.contains(&f), "{step}: {} is in a loop: {rows:?}", row.node_id);
                seen.push(f);
                up = row_of.get(f).and_then(|r| r.feeder.as_ref());
            }
        }
    }

    #[test]
    fn the_first_relay_is_fed_by_the_entry() {
        let plan = plan_feeds(&[relay("r1", "ams", 0)], &wanted(), &held(&[]), &[], 0);
        assert_eq!(
            plan,
            FeedPlan {
                add: vec![feed("r1", None, 1)],
                ..FeedPlan::default()
            }
        );
    }

    #[test]
    fn a_second_relay_is_fed_by_the_closest_relay_not_the_entry() {
        let relays = [relay("r1", "ams", 0), relay("r2", "ams", 0)];
        let existing = [feed("r1", None, 1)];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(
            plan,
            FeedPlan {
                add: vec![feed("r2", Some("r1"), 2)],
                keep: vec![feed("r1", None, 1)],
                ..FeedPlan::default()
            },
            "r1's row stays as it is"
        );
    }

    #[test]
    fn a_relay_whose_feeder_left_keeps_its_row() {
        let relays = [relay("r2", "ams", 0)];
        let existing = [feed("r1", None, 1), feed("r2", Some("r1"), 2)];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert!(plan.keep.is_empty(), "r2 is not kept as a normal row: {:?}", plan.keep);
        assert_eq!(
            plan.fallback,
            vec![feed("r2", None, 1)],
            "r2's row stays as stored; the entry is its fallback"
        );
        assert!(plan.remove.is_empty(), "{:?}", plan.remove);
        assert_eq!(nodes(&plan.forget), vec!["r1"]);
        assert!(plan.add.is_empty());
        assert_eq!(
            apply(&existing, &plan, &[]),
            vec![feed("r2", Some("r1"), 2)],
            "while r2's source lives, its row still names r1"
        );
    }

    #[test]
    fn a_lost_feeder_reattaches_its_relays() {
        // r1 is gone; r2 hung off it and r3 off r2. Only r2 lost its feeder; r3 still
        // pulls from r2. A row changes only when its source is re-created.
        let relays = [relay("r2", "ams", 0), relay("r3", "ams", 0)];
        let existing = [
            feed("r1", None, 1),
            feed("r2", Some("r1"), 2),
            feed("r3", Some("r2"), 3),
        ];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(plan.fallback, vec![feed("r2", None, 1)]);
        assert_eq!(plan.keep, vec![feed("r3", Some("r2"), 3)]);
        assert!(plan.remove.is_empty() && plan.add.is_empty());
        assert_eq!(nodes(&plan.forget), vec!["r1"]);

        // Once r2's source is re-created from the entry, r2's row says so; r3's recorded
        // depth (3) stays an upper bound of where it really is (2).
        let after = apply(&existing, &plan, &["r2"]);
        assert_eq!(after, vec![feed("r2", None, 1), feed("r3", Some("r2"), 3)]);
        let next = plan_feeds(&relays, &wanted(), &held(&[]), &after, 0);
        assert!(next.fallback.is_empty() && next.remove.is_empty() && next.forget.is_empty());
        assert_eq!(kept(&next), vec!["r2", "r3"]);
    }

    #[test]
    fn an_orphan_is_never_offered_a_feeder_below_it() {
        // X is gone. A hung off X, and B off A: B still pulls from A.
        let relays = [relay("a", "ams", 0), relay("b", "ams", 0)];
        let existing = [
            feed("x", None, 1),
            feed("a", Some("x"), 2),
            feed("b", Some("a"), 3),
        ];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(plan.fallback, vec![feed("a", None, 1)], "never b: b pulls from a");
        assert_eq!(plan.keep, vec![feed("b", Some("a"), 3)]);
        assert!(plan.remove.is_empty() && plan.add.is_empty());
    }

    #[test]
    fn an_orphans_fallback_is_no_deeper_than_its_row() {
        // o is recorded at depth 2 under the lost x. c1 (depth 1) is busy, c2 (depth 2) is
        // idle and would win on load, but would put o at depth 3.
        let relays = [
            relay("c1", "ams", 50),
            relay("c2", "ams", 0),
            relay("o", "ams", 0),
        ];
        let existing = [
            feed("c1", None, 1),
            feed("c2", Some("c1"), 2),
            feed("x", None, 1),
            feed("o", Some("x"), 2),
        ];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(plan.fallback, vec![feed("o", Some("c1"), 2)]);
        assert!(plan.fallback.iter().all(|f| f.depth <= 2));
    }

    #[test]
    fn a_new_relay_does_not_join_under_an_orphan() {
        let relays = [
            relay("a", "ams", 0),
            relay("b", "ams", 0),
            relay("n", "ams", 0),
        ];
        let existing = [
            feed("x", None, 1),
            feed("a", Some("x"), 2),
            feed("b", Some("a"), 3),
        ];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(plan.add, vec![feed("n", None, 1)], "neither a nor b may feed n");
    }

    /// Review finding 1, replayed. Rows A←X, B←A (B sorts before A); X drains; later Y
    /// (a candidate fallback) leaves; then X is destroyed and A's source is re-created.
    /// A row changes only where a step says a source was re-created.
    #[test]
    fn the_reviewers_sequence_cannot_form_a_loop() {
        let all = |ids: &[&str]| -> Vec<RelayPlace> { ids.iter().map(|i| relay(i, "ams", 0)).collect() };
        let mut rows = vec![
            feed("A", Some("X"), 2),
            feed("B", Some("A"), 3),
            feed("C", None, 1),
            feed("X", None, 1),
            feed("Y", None, 1),
        ];
        assert_tree(&rows, "start");

        // 1. X starts draining: A has lost its feeder, but its source still pulls from X.
        let relays = all(&["A", "B", "C", "Y"]);
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 0);
        assert_eq!(plan.fallback.len(), 1, "{plan:?}");
        assert_eq!(plan.fallback[0].node_id, NodeId::new("A"));
        assert_ne!(plan.fallback[0].feeder, Some(NodeId::new("B")));
        rows = apply(&rows, &plan, &[]);
        assert_tree(&rows, "X drains");
        assert_eq!(rows.iter().find(|r| r.node_id.as_str() == "A").unwrap().feeder, Some(NodeId::new("X")));

        // 2. Y leaves too. Nothing has been re-created: rows only lose Y.
        let relays = all(&["A", "B", "C"]);
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 0);
        assert_ne!(plan.fallback[0].feeder, Some(NodeId::new("B")));
        rows = apply(&rows, &plan, &[]);
        assert_tree(&rows, "Y leaves");

        // 3. X is destroyed: A's source dies and is re-created from its fallback.
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 0);
        let fallback = plan.fallback[0].clone();
        assert_ne!(fallback.feeder, Some(NodeId::new("B")), "B pulls from A");
        rows = apply(&rows, &plan, &["A"]);
        assert_tree(&rows, "A re-created");

        // 4. The tree is whole again, with every recorded depth an upper bound.
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 0);
        assert!(plan.fallback.is_empty() && plan.remove.is_empty(), "{plan:?}");
        rows = apply(&rows, &plan, &[]);
        assert_tree(&rows, "settled");
        assert_eq!(kept(&plan), vec!["A", "B", "C"]);
    }

    #[test]
    fn depth_never_exceeds_three() {
        // Relays join one per tick, the busier the less chosen, so each new relay prefers
        // the deepest stored carrier it may use: that walks the tree to its limit.
        let all = [
            relay("r1", "ams", 50),
            relay("r2", "ams", 40),
            relay("r3", "ams", 30),
            relay("r4", "ams", 20),
            relay("r5", "ams", 10),
        ];
        let mut rows: Vec<FeedRow> = Vec::new();
        let mut added = Vec::new();
        for n in 1..=all.len() {
            let plan = plan_feeds(&all[..n], &wanted(), &held(&[]), &rows, 0);
            assert!(plan.fallback.is_empty() && plan.remove.is_empty() && plan.forget.is_empty());
            added.extend(plan.add.iter().cloned());
            rows = apply(&rows, &plan, &[]);
        }
        assert_eq!(
            added,
            vec![
                feed("r1", None, 1),
                feed("r2", Some("r1"), 2),
                // A stored carrier at depth 2 may feed a new relay: the new one is at 3.
                feed("r3", Some("r2"), 3),
                // r3 is the least busy carrier but sits at depth 3, so it feeds nobody.
                feed("r4", Some("r2"), 3),
                feed("r5", Some("r2"), 3),
            ]
        );
        assert_tree(&rows, "grown");
        let row_of: HashMap<&NodeId, &FeedRow> = rows.iter().map(|r| (&r.node_id, r)).collect();
        for row in &rows {
            // Walk up to the entry: the hops match the recorded depth.
            let mut hops = 1;
            let mut up = row.feeder.clone();
            while let Some(f) = up {
                hops += 1;
                up = row_of[&f].feeder.clone();
            }
            assert_eq!(hops, row.depth, "{row:?}");
        }
    }

    #[test]
    fn relays_that_join_together_take_one_copy_from_the_entry() {
        let relays = [relay("a", "ams", 0), relay("b", "ams", 0), relay("c", "ams", 0)];
        let first = plan_feeds(&relays, &wanted(), &held(&[]), &[], 0);
        assert_eq!(first.add, vec![feed("a", None, 1)], "only the lowest id, from the entry");
        let rows = apply(&[], &first, &[]);
        let second = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 0);
        assert_eq!(second.add, vec![feed("b", Some("a"), 2), feed("c", Some("a"), 2)]);
        let from_entry = first
            .add
            .iter()
            .chain(&second.add)
            .filter(|r| r.feeder.is_none())
            .count();
        assert_eq!(from_entry, 1, "the main server sends one copy, not one per relay");
    }

    #[test]
    fn the_entry_slot_rotates_with_the_turn() {
        // Three new relays, no stored carrier: one of them takes the entry per plan, and
        // which one moves with the turn.
        let relays = [relay("a", "ams", 0), relay("b", "ams", 0), relay("c", "ams", 0)];
        for (turn, first) in [(0, "a"), (1, "b"), (2, "c"), (3, "a")] {
            let plan = plan_feeds(&relays, &wanted(), &held(&[]), &[], turn);
            assert_eq!(plan.add, vec![feed(first, None, 1)], "turn {turn}");
        }
    }

    /// The turn is the wall clock in sync intervals: one step per tick, the same in any
    /// server instance looking at the same clock.
    #[test]
    fn the_turn_advances_once_per_tick_from_the_wall_clock() {
        let ctx = || {
            PoolSyncContext::new(ConfigHandle::new(mm_core::config::Config::default()), None, None)
                .with_tick(StdDuration::from_secs(10))
        };
        let at = |secs: u64| SystemTime::UNIX_EPOCH + StdDuration::from_secs(secs);
        assert_eq!(ctx().turn(at(1_000)), 100);
        assert_eq!(ctx().turn(at(1_009)), 100);
        assert_eq!(ctx().turn(at(1_010)), 101);
        assert_eq!(ctx().turn(at(1_010)), ctx().turn(at(1_010)), "two instances agree");
    }

    /// The stall rotation fixes: a is in the pool, but its seed fails on every tick.
    #[test]
    fn a_relay_that_cannot_be_seeded_does_not_hold_the_entry() {
        let relays = [relay("a", "ams", 0), relay("b", "ams", 0), relay("c", "ams", 0)];
        // Tick 0: a takes the entry; its seed fails, so no row is written.
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &[], 0);
        assert_eq!(plan.add, vec![feed("a", None, 1)]);
        let rows: Vec<FeedRow> = Vec::new();
        // Tick 1: the slot has moved on: b takes the entry, is seeded, and is stored.
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 1);
        assert_eq!(plan.add, vec![feed("b", None, 1)], "exactly one relay, and not a");
        let rows = apply(&rows, &plan, &[]);
        // Tick 2: a stored carrier exists: the rest pull from b, whatever the turn.
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 2);
        assert_eq!(plan.add, vec![feed("a", Some("b"), 2), feed("c", Some("b"), 2)]);
    }

    #[test]
    fn a_relay_added_in_this_plan_feeds_nobody_in_this_plan() {
        // c is stored and busy. a joins under c; b would prefer the idle a, but a's source
        // is not confirmed yet.
        let relays = [relay("a", "ams", 0), relay("b", "ams", 0), relay("c", "ams", 50)];
        let existing = [feed("c", None, 1)];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(plan.add, vec![feed("a", Some("c"), 2), feed("b", Some("c"), 2)]);
    }

    /// Re-review finding: X was added as X←Q (3) and its seed created the source, but the
    /// row write was lost. On the next tick X is planned again and a new relay Y arrives.
    #[test]
    fn the_lost_row_sequence_cannot_reach_depth_four() {
        // P2 is the least busy stored carrier, so X is re-planned under it; X itself (idle)
        // would be the most attractive feeder for Y, were it offered.
        let relays = [
            relay("P1", "ams", 50),
            relay("P2", "ams", 5),
            relay("Q", "ams", 50),
            relay("X", "ams", 0),
            relay("Y", "ams", 0),
        ];
        let stored = vec![feed("P1", None, 1), feed("P2", None, 1), feed("Q", Some("P1"), 2)];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &stored, 0);
        let x = NodeId::new("X");
        assert!(
            plan.add.iter().all(|r| r.feeder.as_ref() != Some(&x)),
            "Y is never planned under X in the tick X is re-planned: {:?}",
            plan.add
        );
        let y = plan.add.iter().find(|r| r.node_id.as_str() == "Y").expect("Y added").clone();
        assert!(y.depth <= 2, "Y hangs off a stored carrier at depth 1: {y:?}");

        // X's seed finds its source (pulling from Q, depth 3): adopted. Y's seed creates.
        let mut rows = stored.clone();
        rows.push(feed("X", None, MAX_RELAY_DEPTH));
        rows.push(y);
        assert_tree(&rows, "after the tick");
        // From now on X feeds nobody.
        let more = [&relays[..], &[relay("Z", "ams", 0)]].concat();
        let plan = plan_feeds(&more, &wanted(), &held(&[]), &rows, 0);
        assert!(plan.add.iter().all(|r| r.feeder.as_ref() != Some(&x)), "{:?}", plan.add);
        assert!(plan.fallback.iter().all(|r| r.feeder.as_ref() != Some(&x)));
    }

    #[test]
    fn a_stream_no_longer_wanted_loses_all_its_feeds() {
        let relays = [relay("r1", "ams", 0), relay("r2", "ams", 0)];
        let existing = [feed("r1", None, 1), feed("r2", Some("r1"), 2)];
        let plan = plan_feeds(&relays, &[], &held(&[]), &existing, 0);
        assert_eq!(nodes(&plan.remove), vec!["r1", "r2"], "nobody is connected: removed");
        assert_eq!(plan.remove[0].1.as_str(), "r2", "the deepest goes first");
        assert!(plan.add.is_empty() && plan.keep.is_empty() && plan.fallback.is_empty());
        assert!(plan.forget.is_empty());
    }

    #[test]
    fn a_stream_under_the_line_keeps_the_relays_people_are_on() {
        let relays = [relay("r1", "ams", 3), relay("r2", "ams", 0)];
        let existing = [feed("r1", None, 1), feed("r2", Some("r1"), 2)];
        let plan = plan_feeds(&relays, &[], &held(&["r1"]), &existing, 0);
        assert_eq!(nodes(&plan.remove), vec!["r2"]);
        assert_eq!(kept(&plan), vec!["r1"]);
        assert!(plan.add.is_empty() && plan.fallback.is_empty() && plan.forget.is_empty());
    }

    #[test]
    fn a_feeder_is_kept_while_its_downstream_is_held() {
        let relays = [relay("r1", "ams", 1), relay("r2", "ams", 4)];
        let existing = [feed("r1", None, 1), feed("r2", Some("r1"), 2)];
        let plan = plan_feeds(&relays, &[], &held(&["r1", "r2"]), &existing, 0);
        assert_eq!(
            plan,
            FeedPlan {
                keep: existing.to_vec(),
                ..FeedPlan::default()
            }
        );
    }

    #[test]
    fn no_relay_joins_a_stream_that_is_not_wanted() {
        let relays = [relay("r1", "ams", 2), relay("r2", "ams", 0)];
        let existing = [feed("r1", None, 1)];
        let plan = plan_feeds(&relays, &[], &held(&["r1"]), &existing, 0);
        assert_eq!(
            plan,
            FeedPlan {
                keep: existing.to_vec(),
                ..FeedPlan::default()
            }
        );
    }

    #[test]
    fn a_held_relay_that_lost_its_feeder_is_fed_again() {
        let relays = [relay("r2", "ams", 5)];
        let existing = [feed("r1", None, 1), feed("r2", Some("r1"), 2)];
        let plan = plan_feeds(&relays, &[], &held(&["r2"]), &existing, 0);
        assert_eq!(plan.fallback, vec![feed("r2", None, 1)]);
        assert!(plan.remove.is_empty(), "people are on r2: no removal");
        assert_eq!(nodes(&plan.forget), vec!["r1"]);
        assert!(plan.add.is_empty() && plan.keep.is_empty());
        assert_eq!(apply(&existing, &plan, &["r2"]), vec![feed("r2", None, 1)]);
    }

    #[test]
    fn a_feeder_is_kept_while_a_kept_row_pulls_from_it() {
        // Not wanted. B has viewers; A has none of its own, and B's link is missing from
        // A's list at this census. A is still held by B's row.
        let relays = [relay("a", "ams", 0), relay("b", "ams", 2)];
        let existing = [feed("a", None, 1), feed("b", Some("a"), 2)];
        let plan = plan_feeds(&relays, &[], &held(&["b"]), &existing, 0);
        assert!(plan.remove.is_empty(), "{:?}", plan.remove);
        assert_eq!(kept(&plan), vec!["a", "b"]);
        assert!(plan.fallback.is_empty() && plan.add.is_empty() && plan.forget.is_empty());
    }

    #[test]
    fn when_the_last_row_below_a_feeder_goes_the_feeder_is_judged_on_its_own() {
        let relays = [relay("a", "ams", 0), relay("b", "ams", 0)];
        let existing = [feed("a", None, 1), feed("b", Some("a"), 2)];
        let plan = plan_feeds(&relays, &[], &held(&[]), &existing, 0);
        assert_eq!(nodes(&plan.remove), vec!["a", "b"]);
        assert!(plan.keep.is_empty() && plan.fallback.is_empty());
    }

    /// Re-review finding: rows C1←E, A←E, B←A. The stream drops under the line, B is held,
    /// and A's census misses B's link. Then the stream is wanted again.
    #[test]
    fn the_re_reviewers_sequence_keeps_depth_within_three() {
        let relays = [relay("A", "ams", 0), relay("B", "ams", 3), relay("C1", "ams", 0)];
        let mut rows = vec![
            feed("A", None, 1),
            feed("B", Some("A"), 2),
            feed("C1", None, 1),
        ];

        // 1. Under the line, only B held: A is not removed while B's row pulls from it.
        let plan = plan_feeds(&relays, &[], &held(&["B"]), &rows, 0);
        assert!(
            !plan.remove.iter().any(|(_, n)| n.as_str() == "A"),
            "A feeds a kept row: {:?}",
            plan.remove
        );
        rows = apply(&rows, &plan, &[]);
        assert!(rows.iter().any(|r| r.node_id.as_str() == "A"));

        // 2. Force the add path: A's row is gone (by hand), the stream is wanted again,
        // and a new relay N arrives.
        rows.retain(|r| r.node_id.as_str() != "A");
        let relays = [
            relay("A", "ams", 0),
            relay("B", "ams", 3),
            relay("C1", "ams", 0),
            relay("N", "ams", 0),
        ];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 0);
        let add_a = plan.add.iter().find(|r| r.node_id.as_str() == "A").expect("A added");
        assert!(add_a.depth <= 1, "B (recorded 2) pulls from A: {add_a:?}");
        rows = apply(&rows, &plan, &[]);
        assert_tree(&rows, "A re-added");

        // 3. Settle: physical depth is the recorded one here (every seed created its
        // source), and nothing sits deeper than 3, N included.
        for _ in 0..3 {
            let plan = plan_feeds(&relays, &wanted(), &held(&[]), &rows, 0);
            rows = apply(&rows, &plan, &[]);
            assert_tree(&rows, "settle");
        }
        let row_of: HashMap<&str, &FeedRow> = rows.iter().map(|r| (r.node_id.as_str(), r)).collect();
        for row in &rows {
            let mut hops = 1;
            let mut up = row.feeder.as_ref();
            while let Some(f) = up {
                hops += 1;
                up = row_of[f.as_str()].feeder.as_ref();
            }
            assert!(hops <= usize::from(MAX_RELAY_DEPTH), "{row:?} sits at depth {hops}");
            assert!(usize::from(row.depth) >= hops, "{row:?}: recorded depth below the real {hops}");
        }
    }

    #[test]
    fn an_add_under_existing_children_is_bounded() {
        // B (recorded 2) still pulls from A, which has no row. A depth-1 carrier would put
        // A at depth 2 and B at 3 behind a recorded 2.
        let relays = [relay("a", "ams", 0), relay("b", "ams", 0), relay("c", "ams", 0)];
        let existing = [feed("b", Some("a"), 2), feed("c", None, 1)];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(plan.add, vec![feed("a", None, 1)], "the entry, not c");
    }

    #[test]
    fn a_source_found_with_no_row_is_adopted_as_a_leaf() {
        let planned = feed("x", Some("c"), 2);
        // An add whose seed finds the source already there: adopted as a leaf of unknown
        // origin. Created or re-created: the row as planned.
        assert_eq!(
            row_to_write(
                Ensure::Add,
                &planned,
                Seeded::AlreadyThere(Liveness::Active)
            ),
            Some(feed("x", None, MAX_RELAY_DEPTH))
        );
        // Whatever the source list said: an add that finds its source is adopted.
        for liveness in [Liveness::NotActive, Liveness::Unread] {
            assert_eq!(
                row_to_write(Ensure::Add, &planned, Seeded::AlreadyThere(liveness)),
                Some(feed("x", None, MAX_RELAY_DEPTH))
            );
            assert_eq!(
                row_to_write(Ensure::Stored, &planned, Seeded::AlreadyThere(liveness)),
                None
            );
        }
        assert_eq!(row_to_write(Ensure::Add, &planned, Seeded::Created), Some(planned.clone()));
        assert_eq!(row_to_write(Ensure::Add, &planned, Seeded::Recreated), Some(planned.clone()));
        assert_eq!(row_to_write(Ensure::Add, &planned, Seeded::Failed), None);
        // A stored row or a fallback that finds its source alive writes nothing.
        assert_eq!(row_to_write(Ensure::Stored, &planned, Seeded::AlreadyThere(Liveness::Active)), None);
        assert_eq!(row_to_write(Ensure::Stored, &planned, Seeded::Created), Some(planned.clone()));
        // A leaf re-created from the entry is at depth 1.
        let leaf = feed("x", None, MAX_RELAY_DEPTH);
        assert_eq!(
            row_to_write(Ensure::Stored, &leaf, Seeded::Recreated),
            Some(feed("x", None, 1))
        );

        // The adopted leaf is offered to nobody: not as the fallback of the orphan m
        // (recorded 3 under the lost y; visited first, so x is its only relay), and not to
        // a new relay n.
        let relays = [relay("m", "ams", 0), relay("n", "ams", 0), relay("x", "ams", 0)];
        let existing = [leaf.clone(), feed("m", Some("y"), 3)];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(plan.fallback, vec![feed("m", None, 1)]);
        assert_eq!(plan.add, vec![feed("n", None, 1)]);
        assert_eq!(plan.keep, vec![leaf.clone()]);
        let x = NodeId::new("x");
        assert!(
            plan.add.iter().chain(&plan.fallback).all(|r| r.feeder.as_ref() != Some(&x)),
            "{plan:?}"
        );
    }

    #[test]
    fn a_row_whose_recorded_depth_is_not_below_its_feeders_carries_nothing() {
        // c still names x, which was adopted later at depth 3: c's recorded 2 is no longer
        // an upper bound, so c is treated as an orphan, and nothing joins under it.
        let relays = [relay("c", "ams", 0), relay("n", "ams", 0), relay("x", "ams", 0)];
        let existing = [feed("c", Some("x"), 2), feed("x", None, MAX_RELAY_DEPTH)];
        let plan = plan_feeds(&relays, &wanted(), &held(&[]), &existing, 0);
        assert_eq!(plan.fallback, vec![feed("c", None, 1)]);
        assert_eq!(plan.add, vec![feed("n", None, 1)], "neither c nor x feeds n");
        assert!(plan.remove.is_empty());
    }

    // ── Who holds a relay (held_pairs) ──────────────────────────────────────

    fn live(ids: &[&str]) -> Vec<mm_fleet::runner::LiveBroadcast> {
        ids.iter()
            .map(|id| mm_fleet::runner::LiveBroadcast {
                broadcast_id: id.to_string(),
                viewers: 0,
            })
            .collect()
    }

    fn row_on(stream: &str, node: &str) -> FeedRow {
        FeedRow {
            stream_id: stream.to_string(),
            node_id: NodeId::new(node),
            feeder: None,
            depth: 1,
        }
    }

    fn pairs(held: &HashSet<(String, NodeId)>) -> Vec<(&str, &str)> {
        let mut out: Vec<(&str, &str)> =
            held.iter().map(|(s, n)| (s.as_str(), n.as_str())).collect();
        out.sort();
        out
    }

    #[test]
    fn a_listed_session_of_a_live_stream_holds_its_node() {
        let pass = CensusPass {
            live: live(&["s1"]),
            listed: HashSet::from([
                (NodeId::new("r1"), "s1".to_string()),
                (NodeId::new("r2"), "ended".to_string()),
            ]),
            ..CensusPass::default()
        };
        assert_eq!(pairs(&held_pairs(&pass, &[])), vec![("s1", "r1")]);
    }

    #[test]
    fn a_listed_session_that_is_not_connected_holds_its_pair() {
        // Still negotiating: listed, not connected. It counts nowhere, but holds.
        let pass = CensusPass {
            live: live(&["s1"]),
            listed: HashSet::from([(NodeId::new("r1"), "s1".to_string())]),
            per_node_per_broadcast: HashMap::new(),
            ..CensusPass::default()
        };
        assert_eq!(pairs(&held_pairs(&pass, &[])), vec![("s1", "r1")]);
    }

    #[test]
    fn an_ad_break_holds_every_live_stream_its_node_carries() {
        let pass = CensusPass {
            live: live(&["s1", "s2"]),
            ad_break_nodes: HashSet::from([NodeId::new("r1")]),
            ..CensusPass::default()
        };
        let rows = [row_on("s1", "r1"), row_on("s2", "r1"), row_on("ended", "r1"), row_on("s1", "r2")];
        assert_eq!(
            pairs(&held_pairs(&pass, &rows)),
            vec![("s1", "r1"), ("s2", "r1")],
            "r1's live rows, not an ended stream's, not another node's"
        );
    }

    #[test]
    fn a_node_that_did_not_answer_holds_every_live_stream_it_carries() {
        let pass = CensusPass {
            live: live(&["s1"]),
            unanswered: HashSet::from([NodeId::new("r2")]),
            ..CensusPass::default()
        };
        let rows = [row_on("s1", "r2"), row_on("ended", "r2"), row_on("s1", "r1")];
        assert_eq!(pairs(&held_pairs(&pass, &rows)), vec![("s1", "r2")]);
    }

    // ── What the seed reports ───────────────────────────────────────────────

    /// A stand-in relay for the seed: holds `held` sources (listed as `active`), answers
    /// 409 for one it holds, and records the `origin_url` of every seed.
    async fn seed_relay(held: &[&str], active: bool) -> (String, Arc<AsyncMutex<Vec<String>>>) {
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::routing::{delete, get, post};
        use axum::{Json, Router};
        use serde_json::{Value, json};

        #[derive(Clone)]
        struct Relay {
            held: Arc<AsyncMutex<HashSet<String>>>,
            origins: Arc<AsyncMutex<Vec<String>>>,
            active: bool,
        }
        let relay = Relay {
            held: Arc::new(AsyncMutex::new(held.iter().map(|s| s.to_string()).collect())),
            origins: Arc::default(),
            active,
        };
        let origins = relay.origins.clone();
        let app = Router::new()
            .route(
                "/api/sources",
                get(|State(r): State<Relay>| async move {
                    let list: Vec<Value> = r
                        .held
                        .lock()
                        .await
                        .iter()
                        .map(|id| json!({ "id": id, "type": "cascade", "active": r.active }))
                        .collect();
                    Json(json!({ "sources": list }))
                }),
            )
            .route(
                "/api/sources/cascade",
                post(|State(r): State<Relay>, Json(body): Json<Value>| async move {
                    r.origins
                        .lock()
                        .await
                        .push(body["origin_url"].as_str().unwrap_or_default().to_string());
                    let id = body["id"].as_str().unwrap_or_default().to_string();
                    if r.held.lock().await.insert(id) {
                        StatusCode::CREATED
                    } else {
                        StatusCode::CONFLICT
                    }
                }),
            )
            .route(
                "/api/sources/{id}/cascade-token",
                post(|| async { Json(json!({ "ok": "true" })) }),
            )
            .route(
                "/api/sources/{id}",
                delete(|State(r): State<Relay>, Path(id): Path<String>| async move {
                    r.held.lock().await.remove(&id);
                    Json(json!({ "ok": "true" }))
                }),
            )
            .with_state(relay);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), origins)
    }

    #[tokio::test]
    async fn the_seed_reports_created_already_there_and_recreated() {
        let source = switch_source_id("b1");
        let relay_id = NodeId::new("r1");
        let tracker: InactiveTracker = AsyncMutex::new(HashMap::new());

        // Nothing there: created.
        let (url, _) = seed_relay(&[], true).await;
        let client = SwitchClient::with_auth(&url, "s".to_string());
        let got = seed_cascade_source(
            &client,
            &url,
            &relay_id,
            "http://feeder",
            "s",
            "b1",
            &tracker,
            INACTIVE_GRACE,
            None,
        )
        .await;
        assert_eq!(got, Seeded::Created);

        // There and alive: already there, nothing replaced.
        let got = seed_cascade_source(
            &client,
            &url,
            &relay_id,
            "http://fallback",
            "s",
            "b1",
            &tracker,
            INACTIVE_GRACE,
            None,
        )
        .await;
        assert_eq!(got, Seeded::AlreadyThere(Liveness::Active));

        // There, inactive, but inside the grace period: already there, not active.
        let (url, origins) = seed_relay(&[&source], false).await;
        let client = SwitchClient::with_auth(&url, "s".to_string());
        let got = seed_cascade_source(
            &client,
            &url,
            &relay_id,
            "http://fallback",
            "s",
            "b1",
            &tracker,
            INACTIVE_GRACE,
            None,
        )
        .await;
        assert_eq!(got, Seeded::AlreadyThere(Liveness::NotActive));

        // Inactive past the grace period: removed and re-created from the URL passed in.
        tracker.lock().await.insert(
            (relay_id.clone(), source.clone()),
            Instant::now() - INACTIVE_GRACE - StdDuration::from_secs(1),
        );
        let got = seed_cascade_source(
            &client,
            &url,
            &relay_id,
            "http://fallback",
            "s",
            "b1",
            &tracker,
            INACTIVE_GRACE,
            None,
        )
        .await;
        assert_eq!(got, Seeded::Recreated);
        assert_eq!(
            origins.lock().await.last().map(String::as_str),
            Some("http://fallback"),
            "the replacement pulls from the URL the caller passed: the row's fallback"
        );
    }

    /// The seed's source list cannot be read: already there, liveness unread.
    #[tokio::test]
    async fn a_source_list_that_cannot_be_read_reports_unread() {
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use serde_json::json;

        let app = Router::new()
            .route(
                "/api/sources",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .route(
                "/api/sources/cascade",
                post(|| async { StatusCode::CONFLICT }),
            )
            .route(
                "/api/sources/{id}/cascade-token",
                post(|| async { Json(json!({ "ok": "true" })) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let client = SwitchClient::with_auth(&url, "s".to_string());
        let tracker: InactiveTracker = AsyncMutex::new(HashMap::new());
        let got = seed_cascade_source(
            &client,
            &url,
            &NodeId::new("r1"),
            "http://f",
            "s",
            "b1",
            &tracker,
            INACTIVE_GRACE,
            None,
        )
        .await;
        assert_eq!(got, Seeded::AlreadyThere(Liveness::Unread));
    }

    // ── What a relay carries for placement (carried_after) ──────────────────

    #[test]
    fn a_relay_carries_a_stream_only_once_its_source_reads_active() {
        let streams =
            |names: &[&str]| -> Vec<String> { names.iter().map(|s| s.to_string()).collect() };
        let set =
            |names: &[&str]| -> HashSet<String> { names.iter().map(|s| s.to_string()).collect() };
        let ensured = streams(&[
            "active",
            "created",
            "recreated",
            "inactive",
            "failed",
            "unread",
            "unreached",
        ]);
        let seeded = HashMap::from([
            ("active".to_string(), Seeded::AlreadyThere(Liveness::Active)),
            ("created".to_string(), Seeded::Created),
            ("recreated".to_string(), Seeded::Recreated),
            (
                "inactive".to_string(),
                Seeded::AlreadyThere(Liveness::NotActive),
            ),
            ("failed".to_string(), Seeded::Failed),
            ("unread".to_string(), Seeded::AlreadyThere(Liveness::Unread)),
            // "unreached": the relay's budget ran out before its seed.
        ]);

        // Nothing carried before: only the active source counts.
        assert_eq!(
            carried_after(&ensured, &seeded, &set(&[])),
            set(&["active"])
        );

        // Everything carried before: unread and unreached keep it, the rest lose it, and a
        // stream the relay was not to ensure this tick (its row removed) is dropped.
        let before = set(&[
            "active",
            "created",
            "recreated",
            "inactive",
            "failed",
            "unread",
            "unreached",
            "removed",
        ]);
        assert_eq!(
            carried_after(&ensured, &seeded, &before),
            set(&["active", "unread", "unreached"])
        );

        // A relay with nothing to ensure carries nothing.
        assert!(carried_after(&[], &HashMap::new(), &before).is_empty());
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
