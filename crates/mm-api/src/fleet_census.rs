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

use async_trait::async_trait;
use mm_fleet::runner::{BroadcastCensus, LiveBroadcast};
use sqlx::PgPool;

use crate::switch_pool::SwitchPool;

/// The source id mm-core registers a stream's programme under. Matches
/// `client.rs`, which hands the same value to clients as `switch_source_id`.
fn programme_source_id(broadcast_id: &str) -> String {
    format!("stream-{broadcast_id}")
}

pub struct SwitchCensus {
    pool: Arc<SwitchPool>,
    db: PgPool,
}

impl SwitchCensus {
    pub fn new(pool: Arc<SwitchPool>, db: PgPool) -> Self {
        Self { pool, db }
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
        let mut clients = vec![self.pool.origin()];
        for node in self.pool.nodes().await {
            if let Some(c) = self.pool.client_for_node(Some(&node.id)).await {
                clients.push(c);
            }
        }

        for client in clients {
            match client.list_viewers().await {
                Ok(viewers) => {
                    for v in viewers.iter().filter(|v| v.connected) {
                        // An ad viewer is attributed to nothing here; the caller
                        // adds them per broadcast, because only it knows which
                        // programme the ad interrupted.
                        *counts.entry(v.current_source.clone()).or_insert(0) += 1;
                    }
                }
                Err(e) => tracing::warn!(
                    node = client.base_url(),
                    error = %e,
                    "viewer census: a node did not answer, counting it as zero — the \
                     planner will see less demand and grow less, which is the safe \
                     direction to be wrong in"
                ),
            }
        }
        counts
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
