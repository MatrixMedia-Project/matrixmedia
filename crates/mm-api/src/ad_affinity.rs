//! Node affinity for in-stream ad switching (WS-A Task 5, FR-405).
//!
//! An ad break is three calls to one mm-switch: register the ad's source, switch
//! the viewer onto it, switch the viewer back and drop the source. All three must
//! reach the **same** node, and it must be the node holding that viewer.
//!
//! The reason this needs its own guard rather than a comment is that getting it
//! wrong produces no error. `POST /api/switch` for an unknown viewer returns 404
//! — which the ad path logs at `warn` and moves past — while the ad's FileSource
//! sits registered on a node with nobody to show it to, and the viewer never
//! leaves the programme. Meanwhile the impression looks served.
//!
//! The window that makes this real: the switch-back is issued from a timer up to
//! `duration + grace` seconds after the switch, and from the `/ads/events`
//! handler whenever the client reports a skip. A viewer can be re-placed in
//! between. So the switch-back verifies affinity against the pool's *current*
//! binding instead of trusting the node recorded at switch time.

use std::sync::Arc;

use mm_core::fleet::{NodeId, ViewerId};
use mm_core::metrics_global::AD_SWITCH_AFFINITY_MISMATCH;
use mm_core::switch_client::SwitchClient;

use crate::switch_pool::SwitchPool;

/// Why an ad switch could not be routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdSwitchError {
    /// The viewer is now on a different node than the one this ad was set up on.
    /// Switching on either node is wrong: the old one no longer has the viewer,
    /// the new one has never heard of the ad source.
    NodeAffinityMismatch {
        viewer: ViewerId,
        expected: Option<NodeId>,
        actual: Option<NodeId>,
    },
    /// The node this ad was set up on is no longer in the pool. Its viewers went
    /// with it; there is nothing to switch back.
    NodeGone { node: NodeId },
}

impl AdSwitchError {
    /// Stable, bounded label for the metric. Never carries a viewer or node id —
    /// those are unbounded and would blow up cardinality.
    ///
    /// The discriminator is `actual`, not `expected`: "unplaced" means we no
    /// longer know where the viewer is, which is what a lost binding or an
    /// evicted node looks like. `expected: None` with a real `actual` is the
    /// opposite case — an origin-served viewer that got placed mid-ad — and that
    /// is a move.
    pub fn reason(&self) -> &'static str {
        match self {
            AdSwitchError::NodeAffinityMismatch { actual: None, .. } => "unplaced",
            AdSwitchError::NodeAffinityMismatch { .. } => "moved",
            AdSwitchError::NodeGone { .. } => "node_gone",
        }
    }
}

impl std::fmt::Display for AdSwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdSwitchError::NodeAffinityMismatch {
                viewer,
                expected,
                actual,
            } => write!(
                f,
                "viewer {viewer} is on {actual:?} but the ad was set up on {expected:?}"
            ),
            AdSwitchError::NodeGone { node } => write!(f, "node {node} is no longer in the pool"),
        }
    }
}

/// Resolve the client for an ad switch-back, refusing to issue it against the
/// wrong node.
///
/// `set_up_on` is the node recorded when the ad started (`None` = the origin).
/// Returns the client to use, or the reason the switch must not be sent — and
/// counts the reason, because a silent no-op here is an ad charged for and never
/// shown.
pub async fn resolve_switch_back(
    pool: &SwitchPool,
    viewer: &ViewerId,
    set_up_on: Option<&NodeId>,
) -> Result<Arc<SwitchClient>, AdSwitchError> {
    let current = pool.node_of_viewer(viewer).await;

    if current.as_ref() != set_up_on {
        let err = AdSwitchError::NodeAffinityMismatch {
            viewer: viewer.clone(),
            expected: set_up_on.cloned(),
            actual: current,
        };
        AD_SWITCH_AFFINITY_MISMATCH
            .with_label_values(&[err.reason()])
            .inc();
        return Err(err);
    }

    match pool.client_for_node(set_up_on).await {
        Some(client) => Ok(client),
        None => {
            let err = AdSwitchError::NodeGone {
                // Unreachable with set_up_on == None, which always resolves to
                // the origin; kept total rather than unwrapping.
                node: set_up_on.cloned().unwrap_or_else(|| NodeId::new("origin")),
            };
            AD_SWITCH_AFFINITY_MISMATCH
                .with_label_values(&[err.reason()])
                .inc();
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_core::fleet::{FleetNode, NodeFlavor, NodeState, Ownership};

    fn client(host: &str) -> Arc<SwitchClient> {
        Arc::new(SwitchClient::new(&format!("http://{host}.invalid:7890")))
    }

    fn node(id: &str) -> FleetNode {
        FleetNode {
            id: NodeId::new(id),
            flavor: NodeFlavor::Fanout,
            ownership: Ownership::Rented,
            state: NodeState::Healthy,
            viewer_capacity: 250,
            viewers_current: 0,
        }
    }

    async fn pool_with_nodes(ids: &[&str]) -> SwitchPool {
        let pool = SwitchPool::new(client("origin"));
        for id in ids {
            pool.upsert(&node(id), client(id)).await;
        }
        pool
    }

    /// FR-405. A silent no-op bills an impression nobody saw.
    #[tokio::test]
    async fn ad_switch_to_wrong_node_is_rejected_not_ignored() {
        let pool = pool_with_nodes(&["a", "b"]).await;
        pool.bind_viewer(ViewerId::new("v1"), NodeId::new("a")).await;

        // The ad was set up on `b`; the viewer is on `a`.
        let err = resolve_switch_back(&pool, &ViewerId::new("v1"), Some(&NodeId::new("b")))
            .await
            .expect_err("a switch-back to the wrong node must be refused, not attempted");

        assert!(matches!(err, AdSwitchError::NodeAffinityMismatch { .. }));
        assert_eq!(err.reason(), "moved");
    }

    #[tokio::test]
    async fn matching_node_resolves_to_that_nodes_client() {
        let pool = pool_with_nodes(&["a", "b"]).await;
        pool.bind_viewer(ViewerId::new("v1"), NodeId::new("a")).await;

        let got = resolve_switch_back(&pool, &ViewerId::new("v1"), Some(&NodeId::new("a")))
            .await
            .expect("matching affinity must resolve");
        assert_eq!(got.base_url(), "http://a.invalid:7890");
    }

    #[tokio::test]
    async fn an_origin_served_viewer_resolves_to_the_origin() {
        // The fleetless case: nothing placed this viewer, the ad was set up on
        // the origin, and that is exactly right.
        let pool = SwitchPool::new(client("origin"));
        let got = resolve_switch_back(&pool, &ViewerId::new("v1"), None)
            .await
            .expect("an unplaced viewer on an origin-set-up ad must resolve to the origin");
        assert_eq!(got.base_url(), "http://origin.invalid:7890");
    }

    /// The viewer was placed after the ad started on the origin. Switching back
    /// on the origin would no-op; switching on the new node would name an ad
    /// source that does not exist there.
    #[tokio::test]
    async fn a_viewer_placed_after_an_origin_ad_started_is_a_mismatch() {
        let pool = pool_with_nodes(&["a"]).await;
        pool.bind_viewer(ViewerId::new("v1"), NodeId::new("a")).await;

        let err = resolve_switch_back(&pool, &ViewerId::new("v1"), None)
            .await
            .expect_err("must not switch back on the origin for a now-placed viewer");
        assert_eq!(err.reason(), "moved");
    }

    #[tokio::test]
    async fn a_viewer_whose_binding_vanished_is_a_mismatch_not_a_fallback() {
        let pool = pool_with_nodes(&["a"]).await;
        pool.bind_viewer(ViewerId::new("v1"), NodeId::new("a")).await;
        pool.evict(&NodeId::new("a")).await;

        let err = resolve_switch_back(&pool, &ViewerId::new("v1"), Some(&NodeId::new("a")))
            .await
            .expect_err("the node is gone; there is nothing to switch back");
        assert_eq!(
            err.reason(),
            "unplaced",
            "eviction cleared the binding, so the viewer now looks unplaced — the \
             switch must still be refused rather than sent to the origin"
        );
    }

    #[tokio::test]
    async fn the_metric_label_set_is_bounded() {
        // Cardinality guard: these labels must never carry a viewer or node id.
        for reason in ["moved", "node_gone", "unplaced"] {
            AD_SWITCH_AFFINITY_MISMATCH
                .with_label_values(&[reason])
                .inc();
        }
    }
}

#[cfg(test)]
mod entry_tests {
    use crate::state::AdSwitchEntry;
    use mm_ads::creative::AdSlot;
    use mm_core::fleet::NodeId;

    /// FR-410 + design §16.1: a migrating viewer must not be charged twice.
    #[test]
    fn migration_driven_preroll_is_not_billable() {
        let e = AdSwitchEntry::preroll_for_migration(
            NodeId::new("a"),
            "viewer-s1-@u-hs".into(),
            "stream-s1".into(),
            "ad-migration-1".into(),
        );
        assert_eq!(e.slot, AdSlot::PreRoll);
        assert!(
            !e.billable,
            "a migration is our operational business, not the viewer's — billing it \
             charges the same viewer twice for one programme"
        );
        assert_eq!(e.node_id, Some(NodeId::new("a")));
    }
}
