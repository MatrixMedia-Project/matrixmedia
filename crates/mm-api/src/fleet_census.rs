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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mm_core::switch_client::SwitchViewer;
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

/// None (identifies the origin, which has no `mm_fleet_nodes` row) or a fan-out node's id.
type CensusTarget = Option<mm_core::fleet::NodeId>;

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
    /// (under-counting is the safe direction — see both callers' doc comments).
    async fn gather(&self) -> Vec<(CensusTarget, Vec<SwitchViewer>)> {
        let mut targets: Vec<(CensusTarget, Arc<mm_core::switch_client::SwitchClient>)> =
            vec![(None, self.pool.origin())];
        for node in self.pool.nodes().await {
            if let Some(c) = self.pool.client_for_node(Some(&node.id)).await {
                targets.push((Some(node.id), c));
            }
        }

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
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        tracing::warn!(
                            node = base_url,
                            error = %e,
                            "viewer census: a node did not answer, counting it as zero — \
                             the planner will see less demand and grow less, which is the \
                             safe direction to be wrong in"
                        );
                        Vec::new()
                    }
                    Err(_) => {
                        tracing::warn!(
                            node = base_url,
                            budget_secs = CENSUS_BUDGET_PER_NODE.as_secs(),
                            "viewer census: a node ran out of its per-node budget, counting \
                             it as zero"
                        );
                        Vec::new()
                    }
                };
                (target, viewers)
            });
        }

        let mut out = Vec::new();
        while let Some(res) = tasks.join_next().await {
            match res {
                Ok(pair) => out.push(pair),
                // m5: contained (one node's gather, not the tick), but previously silent.
                Err(e) => tracing::error!(
                    error = %e,
                    "viewer census: a node's gather task panicked or was cancelled"
                ),
            }
        }
        out
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
        for (_, viewers) in self.gather().await {
            for v in viewers.iter().filter(|v| v.connected) {
                // An ad viewer is attributed to nothing here; the caller
                // adds them per broadcast, because only it knows which
                // programme the ad interrupted.
                *counts.entry(v.current_source.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Live viewers per fan-out node, right now (§8.2 R6): one total per relay,
    /// across every source it carries, not attributed to a broadcast. The
    /// origin is never included — it has no `mm_fleet_nodes` row to write back
    /// to. A relay that does not answer is counted as zero, the same
    /// under-count-is-safe posture as [`Self::viewers_by_source`]: the planner
    /// (and now the capacity gauge) would rather see a node emptier than it is
    /// than fuller.
    pub async fn viewers_per_node(&self) -> HashMap<mm_core::fleet::NodeId, i64> {
        self.gather()
            .await
            .into_iter()
            .filter_map(|(target, viewers)| {
                let node_id = target?;
                let n = viewers.iter().filter(|v| v.connected).count() as i64;
                Some((node_id, n))
            })
            .collect()
    }

    /// m4: `pool_sync.rs`'s `sync_once` used to call [`Self::viewers_per_node`] and (via
    /// `live_broadcasts`) [`Self::viewers_by_source`] separately — two full gathers of
    /// every node, back to back, in the SAME tick, for data that comes off the exact same
    /// `list_viewers` responses. This does one [`Self::gather`] and derives both answers
    /// from it, for the one caller (`sync_once`) that needs both.
    ///
    /// `live_broadcasts` on the [`BroadcastCensus`] trait is left untouched (and still
    /// does its own gather) for its OTHER callers, who do not also want `viewers_per_node`
    /// in the same breath.
    pub async fn viewers_per_node_and_live_broadcasts(
        &self,
    ) -> Result<(HashMap<mm_core::fleet::NodeId, i64>, Vec<LiveBroadcast>), String> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM mm_streams WHERE status = 'active'")
                .fetch_all(&self.db)
                .await
                .map_err(|e| format!("listing active streams failed: {e}"))?;

        let gathered = self.gather().await;

        let mut per_node = HashMap::new();
        let mut by_source: HashMap<String, u32> = HashMap::new();
        // (node, broadcast) viewer counts, for the relay-hour meter's samples
        // (Task 9, §8.2 R4): the origin has no `mm_fleet_nodes` row to bill, so it
        // is never a key here, same exclusion as `per_node` above.
        let mut per_node_per_broadcast: HashMap<(mm_core::fleet::NodeId, String), i64> =
            HashMap::new();
        for (target, viewers) in &gathered {
            let connected: Vec<_> = viewers.iter().filter(|v| v.connected).collect();
            if let Some(node_id) = target {
                per_node.insert(node_id.clone(), connected.len() as i64);
            }
            for v in &connected {
                *by_source.entry(v.current_source.clone()).or_insert(0) += 1;
                if let Some(node_id) = target {
                    if let Some(broadcast_id) = v.current_source.strip_prefix("stream-") {
                        *per_node_per_broadcast
                            .entry((node_id.clone(), broadcast_id.to_string()))
                            .or_insert(0) += 1;
                    }
                }
            }
        }
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

        Ok((per_node, live))
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

    #[test]
    fn the_programme_source_id_matches_what_clients_are_given() {
        // client.rs hands `switch_source_id = format!("stream-{}", stream.id)` to
        // both the publisher and the viewer. If these two ever disagree,
        // `programme_is_live` answers false for every live broadcast and the fleet
        // silently never grows.
        assert_eq!(programme_source_id("abc123"), "stream-abc123");
    }
}
