//! Switch-backed [`BroadcastCensus`] — who is watching, and whether there is
//! anything to watch.
//!
//! The trait lives in `mm-fleet`; this implementation lives here because only
//! `SwitchPool` can answer it, and `SwitchPool` lives in mm-api. (A deviation from
//! dev plan §B.2, which had mm-fleet depended on by mm-server alone. The
//! alternative was putting real logic in the wiring crate.)
//!
//! Two answers, with quite different failure consequences:
//!
//! * **`live_broadcasts` must never fail quietly.** An empty list means "nothing
//!   is on air", and the runner releases every node for a broadcast that is no
//!   longer live — so a failed query reported as "no broadcasts" tears the whole
//!   fleet down. It returns `Err`.
//! * **Viewer counts may degrade.** A node that will not answer is counted as
//!   zero, which makes the planner see less demand and grow less. Under-counting
//!   costs quality; over-counting costs money, and `plan()` never shrinks on a low
//!   count, so the safe direction is down.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mm_core::switch_client::{SwitchViewer, is_cascade_session};
use mm_fleet::runner::{BroadcastCensus, LiveBroadcast};
use sqlx::PgPool;
use tokio::task::JoinSet;

use crate::switch_pool::SwitchPool;

/// The source id mm-core registers a stream's programme under. Matches
/// `client.rs`, which hands the same value to clients as `switch_source_id`.
fn programme_source_id(broadcast_id: &str) -> String {
    format!("stream-{broadcast_id}")
}

/// m4: per-node time budget for one census gather, matching `pool_sync.rs`'s
/// `SEED_BUDGET_PER_RELAY` — the two loops share the same "one black-holed node costs its
/// own budget, not the tick" shape. Before this, the census polled every node
/// SEQUENTIALLY with one 60 s timeout shared across all of them (`reqwest`'s default), so
/// a single unreachable relay could stall the whole pass for up to a minute while seeding,
/// right next to it in `sync_once`, was already concurrent with an 8 s budget.
const CENSUS_BUDGET_PER_NODE: Duration = Duration::from_secs(8);

/// Whether a listed session is a person watching: connected, and not a relay node
/// pulling a stream through a cascade session ([`is_cascade_session`]). The switch lists
/// both the same way. Only people count toward a STREAM's audience (counting relays would
/// inflate it and order more relays); a NODE's load counts every connected session it
/// serves, relay links included (see [`Tally`]).
fn is_audience(v: &SwitchViewer) -> bool {
    v.connected && !is_cascade_session(&v.id)
}

/// None (identifies the origin, which has no `mm_fleet_nodes` row) or a fan-out node's id.
type CensusTarget = Option<mm_core::fleet::NodeId>;

/// One census pass's counts, from the switches' viewer lists.
#[derive(Debug, Default)]
struct Tally {
    /// Every connected session each fan-out node serves, relay links included: a feeder
    /// whose only sessions are downstream relays is serving their viewers one hop down,
    /// and must not look empty to drain, lease extension or remaining capacity. The
    /// origin is never a key: it has no `mm_fleet_nodes` row to write back to.
    per_node: HashMap<mm_core::fleet::NodeId, i64>,
    /// People per source ([`is_audience`]), across the origin and every node.
    by_source: HashMap<String, u32>,
    /// (node, broadcast) connected sessions on `stream-<broadcast>` sources, relay links
    /// included, for the relay-hour meter's samples (§8.2 R4): a relay link costs
    /// the node one stream's egress, like a person. The origin is never a key.
    per_node_per_broadcast: HashMap<(mm_core::fleet::NodeId, String), i64>,
    /// Fan-out nodes with at least one person listed on an `ad-…` source, connected or
    /// not. An ad source names the user, not the programme the break interrupted, so
    /// such a person cannot be counted toward a broadcast; the node is marked instead.
    /// Counts nothing.
    ad_break_nodes: HashSet<mm_core::fleet::NodeId>,
    /// (node, broadcast) pairs with at least one session LISTED on a `stream-<broadcast>`
    /// source, connected or not, relay links included: a session still negotiating is
    /// listed before it connects (mm-switch drops one on Disconnected, Failed or Closed).
    /// Counts nothing.
    listed: HashSet<(mm_core::fleet::NodeId, String)>,
}

fn tally(gathered: &[(CensusTarget, Vec<SwitchViewer>)]) -> Tally {
    let mut t = Tally::default();
    for (target, viewers) in gathered {
        if let Some(node_id) = target {
            for v in viewers {
                if let Some(broadcast_id) = v.current_source.strip_prefix("stream-") {
                    t.listed.insert((node_id.clone(), broadcast_id.to_string()));
                } else if v.current_source.starts_with("ad-") && !is_cascade_session(&v.id) {
                    t.ad_break_nodes.insert(node_id.clone());
                }
            }
        }
        let connected: Vec<_> = viewers.iter().filter(|v| v.connected).collect();
        if let Some(node_id) = target {
            t.per_node.insert(node_id.clone(), connected.len() as i64);
        }
        for v in connected {
            if is_audience(v) {
                *t.by_source.entry(v.current_source.clone()).or_insert(0) += 1;
            }
            if let Some(node_id) = target {
                if let Some(broadcast_id) = v.current_source.strip_prefix("stream-") {
                    *t.per_node_per_broadcast
                        .entry((node_id.clone(), broadcast_id.to_string()))
                        .or_insert(0) += 1;
                }
            }
        }
    }
    t
}

/// One census pass, as the pool-sync tick needs it: everything below comes off the same
/// `list_viewers` responses.
#[derive(Debug, Default)]
pub struct CensusPass {
    /// Connected sessions per fan-out node, relay links included (see [`Tally::per_node`]).
    pub per_node: HashMap<mm_core::fleet::NodeId, i64>,
    /// Every active stream with its audience: people only, never relay links.
    pub live: Vec<LiveBroadcast>,
    /// Connected sessions per (node, broadcast), relay links included: the relay-hour
    /// meter's samples (see [`Tally::per_node_per_broadcast`]).
    pub per_node_per_broadcast: HashMap<(mm_core::fleet::NodeId, String), i64>,
    /// Fan-out nodes that did not answer (an error or their time budget ran out). Their
    /// counts above read zero, but nobody knows what they serve.
    pub unanswered: HashSet<mm_core::fleet::NodeId>,
    /// Fan-out nodes with at least one person on an ad break (an `ad-…` source, which
    /// names no programme), connected or not: whichever stream they will return to, it
    /// is on that node.
    pub ad_break_nodes: HashSet<mm_core::fleet::NodeId>,
    /// (node, broadcast) pairs with at least one session listed, connected or not,
    /// relay links included. What holds a relay; it counts nothing.
    pub listed: HashSet<(mm_core::fleet::NodeId, String)>,
}

/// The fan-out nodes a gather was sent to that did not answer: errored, ran out of
/// time, or whose task never came back (panicked or dropped).
fn not_answered(
    spawned: &[mm_core::fleet::NodeId],
    answered: &HashSet<mm_core::fleet::NodeId>,
) -> HashSet<mm_core::fleet::NodeId> {
    spawned
        .iter()
        .filter(|n| !answered.contains(*n))
        .cloned()
        .collect()
}

pub struct SwitchCensus {
    pool: Arc<SwitchPool>,
    db: PgPool,
}

impl SwitchCensus {
    pub fn new(pool: Arc<SwitchPool>, db: PgPool) -> Self {
        Self { pool, db }
    }

    /// m4: one concurrent pass over the origin + every fan-out node's `list_viewers`,
    /// each under its own [`CENSUS_BUDGET_PER_NODE`] — the single gather both
    /// [`Self::viewers_by_source`] and [`Self::viewers_per_node`] used to each do on their
    /// own, polling every node a SECOND time for the exact same data within the same
    /// `sync_once` tick. A node that times out or errors reports an empty viewer list
    /// (under-counting is the safe direction — see both callers' doc comments); such a
    /// fan-out node is also named in the returned set, for a caller to which "zero" and
    /// "unknown" are not the same thing.
    async fn gather(
        &self,
    ) -> (
        Vec<(CensusTarget, Vec<SwitchViewer>)>,
        HashSet<mm_core::fleet::NodeId>,
    ) {
        let mut targets: Vec<(CensusTarget, Arc<mm_core::switch_client::SwitchClient>)> =
            vec![(None, self.pool.origin())];
        for node in self.pool.nodes().await {
            if let Some(c) = self.pool.client_for_node(Some(&node.id)).await {
                targets.push((Some(node.id), c));
            }
        }

        let spawned: Vec<mm_core::fleet::NodeId> =
            targets.iter().filter_map(|(t, _)| t.clone()).collect();
        let mut tasks = JoinSet::new();
        for (target, client) in targets {
            tasks.spawn(async move {
                let base_url = client.base_url().to_string();
                let viewers = match tokio::time::timeout(
                    CENSUS_BUDGET_PER_NODE,
                    client.list_viewers(),
                )
                .await
                {
                    Ok(Ok(v)) => Some(v),
                    Ok(Err(e)) => {
                        tracing::warn!(
                            node = base_url,
                            error = %e,
                            "viewer census: a node did not answer, counting it as zero — \
                             the planner will see less demand and grow less, which is the \
                             safe direction to be wrong in"
                        );
                        None
                    }
                    Err(_) => {
                        tracing::warn!(
                            node = base_url,
                            budget_secs = CENSUS_BUDGET_PER_NODE.as_secs(),
                            "viewer census: a node ran out of its per-node budget, counting \
                             it as zero"
                        );
                        None
                    }
                };
                (target, viewers)
            });
        }

        let mut out = Vec::new();
        let mut answered = HashSet::new();
        while let Some(res) = tasks.join_next().await {
            match res {
                Ok((target, Some(viewers))) => {
                    if let Some(node_id) = &target {
                        answered.insert(node_id.clone());
                    }
                    out.push((target, viewers));
                }
                Ok((target, None)) => out.push((target, Vec::new())),
                // m5: contained (one node's gather, not the tick), but previously silent.
                // Its node never reaches `answered`, so it is reported unanswered.
                Err(e) => tracing::error!(
                    error = %e,
                    "viewer census: a node's gather task panicked or was cancelled"
                ),
            }
        }
        (out, not_answered(&spawned, &answered))
    }

    /// Viewer counts per programme source, gathered across the origin and every
    /// fan-out node.
    ///
    /// A viewer whose `current_source` starts with `ad-` is counted toward the
    /// broadcast they are watching: they are mid ad-break, not gone. The shipped
    /// Flutter client already counts them the same way
    /// (`webrtc_platform_web.dart`), so doing otherwise here would make mm-core
    /// and the client disagree about the audience during every break.
    async fn viewers_by_source(&self) -> HashMap<String, u32> {
        let mut counts: HashMap<String, u32> = HashMap::new();
        for (_, viewers) in self.gather().await.0 {
            for v in viewers.iter().filter(|v| is_audience(v)) {
                // An ad viewer is attributed to nothing here; the caller
                // adds them per broadcast, because only it knows which
                // programme the ad interrupted.
                *counts.entry(v.current_source.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Connected sessions per fan-out node, right now (§8.2 R6): one total per relay,
    /// across every source it carries, not attributed to a broadcast, and including
    /// relays pulling from it (see [`Tally::per_node`]). The origin is never
    /// included — it has no `mm_fleet_nodes` row to write back to. A relay that does
    /// not answer is counted as zero, the same under-count-is-safe posture as
    /// [`Self::viewers_by_source`]: the planner (and now the capacity gauge) would
    /// rather see a node emptier than it is than fuller.
    pub async fn viewers_per_node(&self) -> HashMap<mm_core::fleet::NodeId, i64> {
        tally(&self.gather().await.0).per_node
    }

    /// m4: `pool_sync.rs`'s `sync_once` used to call [`Self::viewers_per_node`] and (via
    /// `live_broadcasts`) [`Self::viewers_by_source`] separately — two full gathers of
    /// every node, back to back, in the SAME tick, for data that comes off the exact same
    /// `list_viewers` responses. This does one [`Self::gather`] and derives both answers
    /// from it, for the one caller (`sync_once`) that needs both. It also hands that caller
    /// the per-(node, broadcast) counts and the nodes that did not answer, which the relay
    /// tree needs to know where people are still watching.
    ///
    /// `live_broadcasts` on the [`BroadcastCensus`] trait is left untouched (and still
    /// does its own gather) for its OTHER callers, who do not also want `viewers_per_node`
    /// in the same breath.
    pub async fn viewers_per_node_and_live_broadcasts(&self) -> Result<CensusPass, String> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM mm_streams WHERE status = 'active'")
                .fetch_all(&self.db)
                .await
                .map_err(|e| format!("listing active streams failed: {e}"))?;

        let (gathered, unanswered) = self.gather().await;
        let Tally {
            per_node,
            by_source,
            per_node_per_broadcast,
            ad_break_nodes,
            listed,
        } = tally(&gathered);
        self.insert_relay_usage_samples(&per_node_per_broadcast, Utc::now())
            .await;

        let ad_viewers: u32 = by_source
            .iter()
            .filter(|(src, _)| src.starts_with("ad-"))
            .map(|(_, n)| *n)
            .sum();
        let single_live = ids.len() == 1;

        let live = ids
            .into_iter()
            .map(|id| {
                let mut viewers = by_source
                    .get(&programme_source_id(&id))
                    .copied()
                    .unwrap_or(0);
                if single_live {
                    viewers += ad_viewers;
                }
                LiveBroadcast {
                    broadcast_id: id,
                    viewers,
                }
            })
            .collect();

        Ok(CensusPass {
            per_node,
            live,
            per_node_per_broadcast,
            unanswered,
            ad_break_nodes,
            listed,
        })
    }

    /// Write this tick's per-(relay, broadcast) viewer counts into
    /// `mm_relay_usage_samples` (V052) — the relay-hour meter's input (Task 9,
    /// §8.2 R4). Best-effort: a write failure here must not fail the census, which
    /// the capacity gauges and cascade seeding both depend on this same tick for;
    /// a missed sample just makes that one tick's apportionment slightly thinner,
    /// not wrong.
    async fn insert_relay_usage_samples(
        &self,
        counts: &HashMap<(mm_core::fleet::NodeId, String), i64>,
        sampled_at: DateTime<Utc>,
    ) {
        for ((node_id, broadcast_id), viewers) in counts {
            if let Err(e) = sqlx::query(
                "INSERT INTO mm_relay_usage_samples (mm_node_id, broadcast_id, sampled_at, viewers)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (mm_node_id, broadcast_id, sampled_at) DO NOTHING",
            )
            .bind(node_id.as_str())
            .bind(broadcast_id)
            .bind(sampled_at)
            .bind(*viewers as i32)
            .execute(&self.db)
            .await
            {
                tracing::warn!(
                    node = %node_id,
                    broadcast = %broadcast_id,
                    error = %e,
                    "relay-hour meter: writing a viewer sample failed"
                );
            }
        }
    }
}

#[async_trait]
impl BroadcastCensus for SwitchCensus {
    async fn live_broadcasts(&self) -> Result<Vec<LiveBroadcast>, String> {
        // Err, never an empty Vec, on failure: an empty list is the instruction to
        // release every node.
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM mm_streams WHERE status = 'active'")
                .fetch_all(&self.db)
                .await
                .map_err(|e| format!("listing active streams failed: {e}"))?;

        let counts = self.viewers_by_source().await;

        // Ad viewers are attributed here rather than in the gather, because only
        // this loop knows which programme an `ad-…` source interrupted — the ad
        // source id embeds the user, not the stream, so it is shared out across
        // live broadcasts only when there is exactly one.
        let ad_viewers: u32 = counts
            .iter()
            .filter(|(src, _)| src.starts_with("ad-"))
            .map(|(_, n)| *n)
            .sum();
        let single_live = ids.len() == 1;

        Ok(ids
            .into_iter()
            .map(|id| {
                let mut viewers = counts
                    .get(&programme_source_id(&id))
                    .copied()
                    .unwrap_or(0);
                if single_live {
                    viewers += ad_viewers;
                }
                LiveBroadcast {
                    broadcast_id: id,
                    viewers,
                }
            })
            .collect())
    }

    async fn programme_is_live(&self, broadcast_id: &str) -> Result<bool, String> {
        // The origin holds ingest, so it is the only switch that can answer.
        let want = programme_source_id(broadcast_id);
        let sources = self.pool.origin().list_sources().await?;
        Ok(sources.iter().any(|s| s.id == want && s.active))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_core::fleet::NodeId;

    #[test]
    fn the_programme_source_id_matches_what_clients_are_given() {
        // client.rs hands `switch_source_id = format!("stream-{}", stream.id)` to
        // both the publisher and the viewer. If these two ever disagree,
        // `programme_is_live` answers false for every live broadcast and the fleet
        // silently never grows.
        assert_eq!(programme_source_id("abc123"), "stream-abc123");
    }

    fn session(id: &str, source: &str, connected: bool) -> SwitchViewer {
        SwitchViewer {
            id: id.to_string(),
            current_source: source.to_string(),
            connected,
        }
    }

    fn sample(t: &Tally, node: &str, broadcast: &str) -> Option<i64> {
        t.per_node_per_broadcast
            .get(&(NodeId::new(node), broadcast.to_string()))
            .copied()
    }

    /// A relay pulling a stream through a cascade session costs the node it pulls from
    /// one stream's egress, exactly like a person, so the node's load and its relay-hour
    /// samples count it. The stream's audience does not: the relay is not a viewer.
    #[test]
    fn a_relay_link_counts_toward_the_node_and_the_meter_but_not_the_stream() {
        let t = tally(&[(
            Some(NodeId::new("relay-a")),
            vec![
                session("viewer-b1--alice-example.org", "stream-b1", true),
                session("cascade-stream-b1-lease-1-0", "stream-b1", true),
                session("viewer-b1--carol-example.org", "stream-b1", false),
            ],
        )]);
        assert_eq!(t.per_node.get(&NodeId::new("relay-a")).copied(), Some(2));
        assert_eq!(sample(&t, "relay-a", "b1"), Some(2));
        assert_eq!(t.by_source.get("stream-b1").copied(), Some(1));
    }

    /// The case that keeps a feeder alive: its only session is a downstream relay that
    /// serves people one hop down. It must not read as empty to the drain and
    /// lease-extension check, and it still carries a share of its own hour.
    #[test]
    fn a_node_that_only_feeds_relays_is_not_empty() {
        let t = tally(&[(
            Some(NodeId::new("relay-a")),
            vec![session("cascade-stream-b1-lease-1-0", "stream-b1", true)],
        )]);
        assert_eq!(t.per_node.get(&NodeId::new("relay-a")).copied(), Some(1));
        assert_eq!(sample(&t, "relay-a", "b1"), Some(1));
        assert_eq!(t.by_source.get("stream-b1").copied(), None);
    }

    #[test]
    fn a_disconnected_session_counts_nowhere() {
        let t = tally(&[
            (
                None,
                vec![session("viewer-b1--bob-example.org", "stream-b1", false)],
            ),
            (
                Some(NodeId::new("relay-a")),
                vec![session("cascade-stream-b1-lease-1-0", "stream-b1", false)],
            ),
        ]);
        assert_eq!(t.per_node.get(&NodeId::new("relay-a")).copied(), Some(0));
        assert_eq!(sample(&t, "relay-a", "b1"), None);
        assert!(t.by_source.is_empty());
    }

    /// A person on an ad break watches an `ad-…` source that names no programme, so the
    /// node they are on is marked instead, connected or not (a session that is only
    /// negotiating is listed before it connects). A relay link on an ad source
    /// marks nothing, and the origin is never a node.
    #[test]
    fn a_person_on_an_ad_break_marks_their_node() {
        let t = tally(&[
            (
                Some(NodeId::new("relay-a")),
                vec![session("viewer-b1--alice-example.org", "ad-alice-1", true)],
            ),
            (
                Some(NodeId::new("relay-b")),
                vec![session("viewer-b1--bob-example.org", "ad-bob-1", false)],
            ),
            (
                Some(NodeId::new("relay-c")),
                vec![session("cascade-ad-x-lease-1-0", "ad-x", true)],
            ),
            (
                None,
                vec![session("viewer-b1--carol-example.org", "ad-carol-1", true)],
            ),
        ]);
        assert_eq!(
            t.ad_break_nodes,
            HashSet::from([NodeId::new("relay-a"), NodeId::new("relay-b")])
        );
        assert_eq!(sample(&t, "relay-a", "b1"), None, "the counts are unchanged");
    }

    /// Every session listed on a node marks its (node, broadcast) pair, connected or not;
    /// the counts still take connected sessions only.
    #[test]
    fn every_listed_session_marks_its_pair_but_only_connected_ones_count() {
        let t = tally(&[
            (
                Some(NodeId::new("relay-a")),
                vec![
                    session("viewer-b1--alice-example.org", "stream-b1", false),
                    session("cascade-stream-b2-lease-1-0", "stream-b2", true),
                ],
            ),
            (None, vec![session("viewer-b3--bob-example.org", "stream-b3", false)]),
        ]);
        assert_eq!(
            t.listed,
            HashSet::from([
                (NodeId::new("relay-a"), "b1".to_string()),
                (NodeId::new("relay-a"), "b2".to_string()),
            ])
        );
        assert_eq!(sample(&t, "relay-a", "b1"), None);
        assert_eq!(sample(&t, "relay-a", "b2"), Some(1));
        assert_eq!(t.per_node.get(&NodeId::new("relay-a")).copied(), Some(1));
    }

    /// A gather task that panicked or was dropped never reports back; its node is
    /// unanswered, like one that errored or ran out of time.
    #[test]
    fn a_node_whose_gather_task_did_not_return_is_unanswered() {
        let spawned = [NodeId::new("relay-a"), NodeId::new("relay-b"), NodeId::new("relay-c")];
        let answered = HashSet::from([NodeId::new("relay-a")]);
        assert_eq!(
            not_answered(&spawned, &answered),
            HashSet::from([NodeId::new("relay-b"), NodeId::new("relay-c")])
        );
    }

    /// The origin is billed to nobody and written back nowhere.
    #[test]
    fn the_origin_is_never_a_node_or_a_meter_key() {
        let t = tally(&[(
            None,
            vec![
                session("viewer-b1--alice-example.org", "stream-b1", true),
                session("cascade-stream-b1-lease-1-0", "stream-b1", true),
            ],
        )]);
        assert!(t.per_node.is_empty());
        assert!(t.per_node_per_broadcast.is_empty());
        assert_eq!(t.by_source.get("stream-b1").copied(), Some(1));
    }
}
