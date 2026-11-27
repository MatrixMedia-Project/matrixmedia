//! Node→client resolution for the broadcast fleet (WS-A Task 3).
//!
//! mm-core has held exactly one `SwitchClient` since the switch existed, which
//! is why `docs/improvements/04-mm-switch-decomposition.md` concluded that
//! "sharding requires a control-plane change, not just more containers". This is
//! that change: one place that answers *which* switch a given call should go to.
//!
//! Two rules shape everything here.
//!
//! **The origin is always the answer when there is no fleet.** A single-host OSS
//! install (FR-102) has no nodes, and it must behave exactly as it did before
//! this type existed. Every resolution method therefore falls back to the origin
//! rather than failing, and `empty_pool_falls_back_to_origin` is the contract
//! with every existing self-hosted deployment.
//!
//! **A viewer-scoped call must go to the viewer's own node.** An ad switch sent
//! to the wrong node does not error — mm-switch just does not know that viewer,
//! so it no-ops. The impression is then billed for an ad nobody saw (FR-405).
//! That is why `for_viewer` exists and why it returns `Option` instead of
//! silently falling back to the origin: for a viewer we have placed, the origin
//! is a *wrong* answer, not a safe default.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use mm_core::fleet::{FleetNode, NodeId, ViewerId};
use mm_core::switch_client::SwitchClient;
use mm_core::types::StreamId;
use tokio::sync::RwLock;

/// Resolves which mm-switch instance a call belongs to.
pub struct SwitchPool {
    nodes: RwLock<HashMap<NodeId, Entry>>,
    /// Which node each placed viewer sits on. Without this `for_viewer` can
    /// never answer, and every ad switch silently goes to the origin.
    viewers: RwLock<HashMap<ViewerId, NodeId>>,
    /// The streams each relay has the picture of: its cascade source read active at the
    /// pool sync's last ensure (see [`Self::set_carried`]). Kept apart from `nodes` so the
    /// sync's per-tick `upsert` does not clear it before the tick's own `set_carried`.
    carried: RwLock<HashMap<NodeId, HashSet<String>>>,
    /// People watching each live stream at the last census; `None` before the first.
    stream_viewers: RwLock<Option<HashMap<String, u32>>>,
    origin: Arc<SwitchClient>,
}

struct Entry {
    node: FleetNode,
    client: Arc<SwitchClient>,
    /// When a rented node is destroyed; `None` for a node that is not rented.
    destroy_deadline: Option<DateTime<Utc>>,
}

/// How close to its destroy deadline a relay may be and still take new viewers.
///
/// The pool retires an idle relay 5 minutes before its deadline
/// (`mm_fleet::pool_autoscale::RETIRE_MARGIN`), so nobody may be placed on one in the window
/// before that: a viewer placed there would hold a node that is about to be let go. A kept
/// relay's deadline is always about an hour away (each pool tick extends it by a full lease
/// window), so this never excludes a relay the pool wants.
pub const PLACEMENT_DEADLINE_GUARD: Duration = Duration::minutes(10);

/// Where a viewer-scoped call should go, and whether its outcome may be billed.
#[derive(Clone)]
pub struct ViewerRoute {
    pub client: Arc<SwitchClient>,
    /// `None` means the origin.
    pub node_id: Option<NodeId>,
    /// False when we could not establish which node holds this viewer while a
    /// fleet exists. The ad was not shown, so an impression must not be charged.
    pub billable: bool,
}

impl SwitchPool {
    /// A pool with no fleet: every resolution returns `origin`.
    pub fn new(origin: Arc<SwitchClient>) -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            viewers: RwLock::new(HashMap::new()),
            carried: RwLock::new(HashMap::new()),
            stream_viewers: RwLock::new(None),
            origin,
        }
    }

    pub fn origin(&self) -> Arc<SwitchClient> {
        self.origin.clone()
    }

    /// Borrowed form of [`Self::origin`], for contexts that borrow from the
    /// handler state for a whole call (the stream end path).
    pub fn origin_ref(&self) -> &Arc<SwitchClient> {
        &self.origin
    }

    /// The switch that holds a stream's ingest and recording. Always the origin:
    /// the publisher publishes there, and the recorder taps that fan-out
    /// (`source_file.go`), so moving it would move the recording with it.
    pub fn origin_for(&self, _stream: &StreamId) -> Arc<SwitchClient> {
        self.origin.clone()
    }

    pub async fn node_count(&self) -> usize {
        self.nodes.read().await.len()
    }

    /// Register or refresh a node. Called by the fleet runner as observations
    /// arrive; re-registering an existing id replaces its state and its client.
    pub async fn upsert(&self, node: &FleetNode, client: Arc<SwitchClient>) {
        self.upsert_with_deadline(node, client, None).await;
    }

    /// [`Self::upsert`] for a node with a known destroy deadline (a rented relay), so
    /// placement can keep new viewers off it in its last minutes
    /// ([`PLACEMENT_DEADLINE_GUARD`]). `None` is a node that is not rented.
    pub async fn upsert_with_deadline(
        &self,
        node: &FleetNode,
        client: Arc<SwitchClient>,
        destroy_deadline: Option<DateTime<Utc>>,
    ) {
        self.nodes.write().await.insert(
            node.id.clone(),
            Entry {
                node: node.clone(),
                client,
                destroy_deadline,
            },
        );
    }

    /// Record which streams `node` has the picture of: those whose cascade source on it
    /// read active this tick. Only such a relay is offered to a new viewer of a stream
    /// ([`Self::assign_fanout_node`]); a feed row alone records where a source pulls from,
    /// not that it has a picture. Ignored for a node the pool does not hold, so a late
    /// call cannot bring back a node that was just evicted.
    pub async fn set_carried(&self, node: &NodeId, streams: HashSet<String>) {
        let nodes = self.nodes.read().await;
        if nodes.contains_key(node) {
            self.carried.write().await.insert(node.clone(), streams);
        }
    }

    /// The streams `node` was last recorded as carrying (empty when none).
    pub async fn carried_by(&self, node: &NodeId) -> HashSet<String> {
        self.carried
            .read()
            .await
            .get(node)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace the per-stream audience with the census's: people only, never relay links.
    pub async fn set_stream_viewers(&self, counts: HashMap<String, u32>) {
        *self.stream_viewers.write().await = Some(counts);
    }

    /// People watching `stream` at the last census. `None` before the first census and for
    /// a stream the last census did not list: unknown, which places a viewer on the core.
    pub async fn viewers_of(&self, stream: &StreamId) -> Option<u32> {
        self.stream_viewers
            .read()
            .await
            .as_ref()?
            .get(&stream.0)
            .copied()
    }

    /// Remove a node, and with it every viewer binding that pointed at it.
    ///
    /// Dropping the bindings is not housekeeping. If they survive, `for_viewer`
    /// keeps naming a node that is gone: the lookup returns `None`, the caller
    /// takes its fallback path, and the map grows for the lifetime of the
    /// process. Callers must re-place those viewers.
    pub async fn evict(&self, id: &NodeId) -> Vec<ViewerId> {
        self.nodes.write().await.remove(id);
        self.carried.write().await.remove(id);

        let mut viewers = self.viewers.write().await;
        let orphaned: Vec<ViewerId> = viewers
            .iter()
            .filter(|(_, n)| *n == id)
            .map(|(v, _)| v.clone())
            .collect();
        for v in &orphaned {
            viewers.remove(v);
        }
        orphaned
    }

    /// Bind a viewer to the node now serving it.
    pub async fn bind_viewer(&self, viewer: ViewerId, node: NodeId) {
        self.viewers.write().await.insert(viewer, node);
    }

    pub async fn unbind_viewer(&self, viewer: &ViewerId) {
        self.viewers.write().await.remove(viewer);
    }

    /// The node holding this viewer, or `None` when the viewer is unplaced (and
    /// therefore on the origin). Ad switches MUST target this (FR-405).
    pub async fn for_viewer(&self, viewer: &ViewerId) -> Option<Arc<SwitchClient>> {
        let node_id = self.viewers.read().await.get(viewer)?.clone();
        self.nodes
            .read()
            .await
            .get(&node_id)
            .map(|e| e.client.clone())
    }

    /// Resolve a viewer-scoped call (FR-405).
    ///
    /// The interesting case is the fallback. A viewer with no binding is either
    /// on the origin — correct, because a fleetless install places nobody — or on
    /// a node whose binding we lost. Those look identical from the binding alone,
    /// and they differ in exactly one observable way: whether the fleet has any
    /// nodes at all. So `billable` is true when the viewer is placed, and true on
    /// the fallback only when there is no fleet to have misplaced them.
    ///
    /// Getting this wrong costs money in a way nothing surfaces: a switch sent to
    /// the wrong node does not error, mm-switch simply does not know that viewer
    /// and no-ops, and the impression is then billed for an ad nobody saw.
    pub async fn route_viewer(&self, viewer: &ViewerId) -> ViewerRoute {
        if let Some(node_id) = self.node_of_viewer(viewer).await {
            if let Some(client) = self.nodes.read().await.get(&node_id).map(|e| e.client.clone()) {
                return ViewerRoute {
                    client,
                    node_id: Some(node_id),
                    billable: true,
                };
            }
        }
        ViewerRoute {
            client: self.origin.clone(),
            node_id: None,
            billable: self.node_count().await == 0,
        }
    }

    /// The client for a specific node, or the origin when `node_id` is `None`.
    ///
    /// Used to send a switch-BACK to the same node that took the original
    /// switch. `None` when the node is named but gone: its viewers went with it,
    /// and pretending the origin will do would silently no-op.
    pub async fn client_for_node(&self, node_id: Option<&NodeId>) -> Option<Arc<SwitchClient>> {
        match node_id {
            None => Some(self.origin.clone()),
            Some(id) => self.nodes.read().await.get(id).map(|e| e.client.clone()),
        }
    }

    /// The node id holding this viewer, without resolving a client.
    pub async fn node_of_viewer(&self, viewer: &ViewerId) -> Option<NodeId> {
        self.viewers.read().await.get(viewer).cloned()
    }

    /// Pick a fan-out node for a new viewer of `stream`: the placeable node with
    /// the most headroom among those that carry `stream` and are not in their last
    /// minutes ([`PLACEMENT_DEADLINE_GUARD`]), or the origin when the fleet has nothing
    /// to offer.
    ///
    /// Ties break on node id rather than on hash order. A `max_by_key` over a
    /// `HashMap` is otherwise nondeterministic between runs, which turns "why
    /// did this viewer land there" into an unanswerable question and makes any
    /// test with two equal nodes flaky.
    pub async fn assign_fanout(&self, stream: &StreamId) -> Arc<SwitchClient> {
        match self.assign_fanout_node(stream).await {
            Some((_, client)) => client,
            None => self.origin.clone(),
        }
    }

    /// Same choice as `assign_fanout`, but also returns the node id so the
    /// caller can record the placement and bind the viewer. `None` means the origin.
    pub async fn assign_fanout_node(&self, stream: &StreamId) -> Option<(NodeId, Arc<SwitchClient>)> {
        self.assign_fanout_node_at(stream, Utc::now()).await
    }

    /// [`Self::assign_fanout_node`] at a given time, which the deadline guard is measured
    /// from.
    async fn assign_fanout_node_at(
        &self,
        stream: &StreamId,
        now: DateTime<Utc>,
    ) -> Option<(NodeId, Arc<SwitchClient>)> {
        let nodes = self.nodes.read().await;
        let carried = self.carried.read().await;
        nodes
            .iter()
            .filter(|(_, e)| e.node.is_placeable())
            .filter(|(id, _)| carried.get(*id).is_some_and(|s| s.contains(&stream.0)))
            .filter(|(_, e)| {
                e.destroy_deadline
                    .is_none_or(|deadline| deadline > now + PLACEMENT_DEADLINE_GUARD)
            })
            .max_by(|(a_id, a), (b_id, b)| {
                a.node
                    .headroom()
                    .cmp(&b.node.headroom())
                    // Reverse on the id so the SMALLEST id wins a tie, since
                    // max_by keeps the last maximum.
                    .then_with(|| b_id.cmp(a_id))
            })
            .map(|(id, e)| (id.clone(), e.client.clone()))
    }

    /// Reverse lookup used by tests and by log lines that have a client but not
    /// an id. Identity is by `Arc` pointer, so it only matches the exact client
    /// the pool was given.
    pub async fn node_id_of(&self, client: &Arc<SwitchClient>) -> Option<NodeId> {
        self.nodes
            .read()
            .await
            .iter()
            .find(|(_, e)| Arc::ptr_eq(&e.client, client))
            .map(|(id, _)| id.clone())
    }

    /// Snapshot of every node, for metrics and the operator console.
    pub async fn nodes(&self) -> Vec<FleetNode> {
        let mut out: Vec<FleetNode> = self
            .nodes
            .read()
            .await
            .values()
            .map(|e| e.node.clone())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_core::fleet::{NodeFlavor, NodeState, Ownership};

    fn client(host: &str) -> Arc<SwitchClient> {
        Arc::new(SwitchClient::new(&format!("http://{host}.invalid:7890")))
    }

    fn origin_client() -> Arc<SwitchClient> {
        client("origin")
    }

    fn node(id: &str, ownership: Ownership, capacity: u32, current: u32) -> FleetNode {
        FleetNode {
            id: NodeId::new(id),
            flavor: NodeFlavor::Fanout,
            ownership,
            state: NodeState::Healthy,
            viewer_capacity: capacity,
            viewers_current: current,
            funding: mm_core::fleet::funding::Funding::Wallet,
        }
    }

    fn draining_node(id: &str) -> FleetNode {
        let mut n = node(id, Ownership::Rented, 250, 0);
        n.state = NodeState::Draining;
        n
    }

    /// Record that each of `ids` has the picture of `stream`, as the pool sync does once a
    /// relay's source reads active.
    async fn carry(pool: &SwitchPool, ids: &[&str], stream: &str) {
        for id in ids {
            let mut streams = pool.carried_by(&NodeId::new(*id)).await;
            streams.insert(stream.to_string());
            pool.set_carried(&NodeId::new(*id), streams).await;
        }
    }

    /// THE CONTRACT WITH EVERY EXISTING SELF-HOSTED DEPLOYMENT.
    ///
    /// FR-102: a single-host OSS install has no fleet, and must behave exactly
    /// as it did before this type existed.
    #[tokio::test]
    async fn empty_pool_falls_back_to_origin() {
        let pool = SwitchPool::new(origin_client());

        let got = pool.assign_fanout(&StreamId("s1".into())).await;
        assert!(Arc::ptr_eq(&got, &pool.origin()));
        assert_eq!(pool.node_count().await, 0);
        assert!(pool.assign_fanout_node(&StreamId("s1".into())).await.is_none());
    }

    #[tokio::test]
    async fn assign_fanout_prefers_least_loaded_healthy_node() {
        let pool = SwitchPool::new(origin_client());
        pool.upsert(&node("a", Ownership::Rented, 250, 200), client("a")).await;
        pool.upsert(&node("b", Ownership::Leased, 250, 10), client("b")).await;
        carry(&pool, &["a", "b"], "s1").await;

        let got = pool.assign_fanout(&StreamId("s1".into())).await;
        assert_eq!(pool.node_id_of(&got).await.unwrap().as_str(), "b");
    }

    #[tokio::test]
    async fn unhealthy_and_full_nodes_are_not_assigned() {
        let pool = SwitchPool::new(origin_client());
        pool.upsert(&draining_node("c"), client("c")).await;
        pool.upsert(&node("d", Ownership::Rented, 250, 250), client("d")).await;
        carry(&pool, &["c", "d"], "s1").await;

        let got = pool.assign_fanout(&StreamId("s1".into())).await;
        assert!(
            Arc::ptr_eq(&got, &pool.origin()),
            "must fall back to the origin, not overfill a node or hand media to a draining one"
        );
    }

    #[tokio::test]
    async fn an_over_capacity_node_is_not_preferred() {
        // The underflow trap, end to end: `d` reports MORE viewers than its
        // capacity. On wrapping arithmetic its headroom is ~4 billion and it
        // wins every placement decision in the fleet.
        let pool = SwitchPool::new(origin_client());
        pool.upsert(&node("d", Ownership::Rented, 250, 300), client("d")).await;
        pool.upsert(&node("e", Ownership::Rented, 250, 249), client("e")).await;
        carry(&pool, &["d", "e"], "s1").await;

        let got = pool.assign_fanout(&StreamId("s1".into())).await;
        assert_eq!(
            pool.node_id_of(&got).await.unwrap().as_str(),
            "e",
            "the over-capacity node was preferred — headroom underflowed"
        );
    }

    #[tokio::test]
    async fn only_flavors_that_serve_webrtc_viewers_are_assigned() {
        let pool = SwitchPool::new(origin_client());
        for (id, flavor) in [("edge-1", NodeFlavor::Edge), ("tx-1", NodeFlavor::Transcode)] {
            let mut n = node(id, Ownership::Rented, 250, 0);
            n.flavor = flavor;
            pool.upsert(&n, client(id)).await;
        }
        carry(&pool, &["edge-1", "tx-1"], "s1").await;

        let got = pool.assign_fanout(&StreamId("s1".into())).await;
        assert!(
            Arc::ptr_eq(&got, &pool.origin()),
            "an LL-HLS edge and a transcoder serve no WebRTC viewers; assigning one \
             would hand a viewer to a node with no path to them"
        );
    }

    #[tokio::test]
    async fn evicted_node_is_never_returned() {
        let pool = SwitchPool::new(origin_client());
        pool.upsert(&node("e", Ownership::Rented, 250, 0), client("e")).await;
        carry(&pool, &["e"], "s1").await;
        pool.evict(&NodeId::new("e")).await;

        let got = pool.assign_fanout(&StreamId("s1".into())).await;
        assert!(Arc::ptr_eq(&got, &pool.origin()));
        assert_eq!(pool.node_count().await, 0);
        assert!(
            pool.carried_by(&NodeId::new("e")).await.is_empty(),
            "eviction forgets what the node carried"
        );
        // Back under the same id, it carries nothing until the sync says so again.
        pool.upsert(&node("e", Ownership::Rented, 250, 0), client("e"))
            .await;
        assert!(
            pool.assign_fanout_node(&StreamId("s1".into()))
                .await
                .is_none()
        );
    }

    /// Eviction must hand back the viewers it orphaned, or they are lost: the
    /// binding is gone, so nothing knows they need re-placing, and they sit on a
    /// machine that is being destroyed.
    #[tokio::test]
    async fn eviction_reports_and_clears_the_viewers_it_orphans() {
        let pool = SwitchPool::new(origin_client());
        pool.upsert(&node("n1", Ownership::Rented, 250, 2), client("n1")).await;
        pool.upsert(&node("n2", Ownership::Rented, 250, 1), client("n2")).await;
        pool.bind_viewer(ViewerId::new("v-1"), NodeId::new("n1")).await;
        pool.bind_viewer(ViewerId::new("v-2"), NodeId::new("n1")).await;
        pool.bind_viewer(ViewerId::new("v-3"), NodeId::new("n2")).await;

        let mut orphaned = pool.evict(&NodeId::new("n1")).await;
        orphaned.sort();
        assert_eq!(
            orphaned,
            vec![ViewerId::new("v-1"), ViewerId::new("v-2")],
            "eviction must name the viewers that need re-placing"
        );

        assert!(pool.for_viewer(&ViewerId::new("v-1")).await.is_none());
        assert!(
            pool.node_of_viewer(&ViewerId::new("v-1")).await.is_none(),
            "a stale binding to a destroyed node must not survive — it would grow \
             the map for the life of the process and keep naming a dead node"
        );
        assert!(
            pool.for_viewer(&ViewerId::new("v-3")).await.is_some(),
            "evicting n1 must not disturb n2's viewers"
        );
    }

    /// FR-405. This is the one that costs money when it is wrong.
    #[tokio::test]
    async fn an_ad_switch_resolves_to_the_viewers_own_node_not_the_origin() {
        let pool = SwitchPool::new(origin_client());
        let n2_client = client("n2");
        pool.upsert(&node("n1", Ownership::Rented, 250, 0), client("n1")).await;
        pool.upsert(&node("n2", Ownership::Rented, 250, 0), n2_client.clone()).await;
        pool.bind_viewer(ViewerId::new("v-9"), NodeId::new("n2")).await;

        let got = pool
            .for_viewer(&ViewerId::new("v-9"))
            .await
            .expect("a placed viewer must resolve");
        assert!(
            Arc::ptr_eq(&got, &n2_client),
            "an ad switch sent to the wrong node does not error — mm-switch does not \
             know the viewer and no-ops, and the impression is billed for an ad \
             nobody saw"
        );
    }

    #[tokio::test]
    async fn an_unplaced_viewer_has_no_node() {
        let pool = SwitchPool::new(origin_client());
        assert!(
            pool.for_viewer(&ViewerId::new("never-placed")).await.is_none(),
            "for_viewer must NOT fall back to the origin: for a viewer we placed, \
             the origin is a wrong answer, not a safe default"
        );
    }

    #[tokio::test]
    async fn upsert_replaces_state_for_an_existing_id() {
        let pool = SwitchPool::new(origin_client());
        pool.upsert(&node("a", Ownership::Rented, 250, 0), client("a")).await;
        pool.upsert(&node("b", Ownership::Rented, 250, 100), client("b")).await;
        carry(&pool, &["a", "b"], "s1").await;

        // `a` fills up. The pool must notice on the next observation.
        pool.upsert(&node("a", Ownership::Rented, 250, 250), client("a")).await;

        let got = pool.assign_fanout(&StreamId("s1".into())).await;
        assert_eq!(pool.node_id_of(&got).await.unwrap().as_str(), "b");
        assert_eq!(pool.node_count().await, 2, "upsert must replace, not duplicate");
    }

    /// Placement must be reproducible. With hash ordering, two equally empty
    /// nodes make both this test and every production placement decision a coin
    /// flip that changes between runs.
    #[tokio::test]
    async fn ties_break_deterministically_on_node_id() {
        for _ in 0..20 {
            let pool = SwitchPool::new(origin_client());
            for id in ["m", "a", "z"] {
                pool.upsert(&node(id, Ownership::Rented, 250, 0), client(id)).await;
            }
            carry(&pool, &["m", "a", "z"], "s1").await;
            let got = pool.assign_fanout(&StreamId("s1".into())).await;
            assert_eq!(pool.node_id_of(&got).await.unwrap().as_str(), "a");
        }
    }

    /// Review Focus 5: a relay without the picture of a stream would hand its viewer
    /// black video, so only a relay that carries the stream is offered.
    #[tokio::test]
    async fn a_viewer_is_only_sent_to_a_relay_that_carries_the_stream() {
        let pool = SwitchPool::new(origin_client());
        // `a` is emptier, so it would win on headroom alone.
        pool.upsert(&node("a", Ownership::Rented, 250, 0), client("a"))
            .await;
        pool.upsert(&node("b", Ownership::Rented, 250, 100), client("b"))
            .await;
        carry(&pool, &["b"], "s1").await;

        let (picked, _) = pool
            .assign_fanout_node(&StreamId("s1".into()))
            .await
            .expect("b carries s1");
        assert_eq!(picked.as_str(), "b");
        assert!(
            pool.assign_fanout_node(&StreamId("s2".into()))
                .await
                .is_none(),
            "nobody carries s2: its viewer goes to the origin"
        );
        let got = pool.assign_fanout(&StreamId("s2".into())).await;
        assert!(
            Arc::ptr_eq(&got, &pool.origin()),
            "assign_fanout follows the same rule"
        );
    }

    /// The sync records what a relay carries each tick; a new record replaces the old one.
    #[tokio::test]
    async fn a_relay_that_stops_carrying_a_stream_is_no_longer_offered() {
        let pool = SwitchPool::new(origin_client());
        pool.upsert(&node("a", Ownership::Rented, 250, 0), client("a"))
            .await;
        carry(&pool, &["a"], "s1").await;
        assert!(
            pool.assign_fanout_node(&StreamId("s1".into()))
                .await
                .is_some()
        );

        pool.set_carried(&NodeId::new("a"), HashSet::new()).await;
        assert!(
            pool.assign_fanout_node(&StreamId("s1".into()))
                .await
                .is_none()
        );

        // An upsert (every tick refreshes the node) does not clear it.
        carry(&pool, &["a"], "s1").await;
        pool.upsert(&node("a", Ownership::Rented, 250, 3), client("a"))
            .await;
        assert!(
            pool.assign_fanout_node(&StreamId("s1".into()))
                .await
                .is_some()
        );

        // A node the pool does not hold records nothing.
        pool.set_carried(&NodeId::new("ghost"), HashSet::from(["s1".to_string()]))
            .await;
        assert!(pool.carried_by(&NodeId::new("ghost")).await.is_empty());
    }

    #[tokio::test]
    async fn an_unknown_viewer_count_is_none() {
        let pool = SwitchPool::new(origin_client());
        assert_eq!(
            pool.viewers_of(&StreamId("s1".into())).await,
            None,
            "before any census the count is unknown"
        );

        pool.set_stream_viewers(HashMap::from([("s1".to_string(), 60)]))
            .await;
        assert_eq!(pool.viewers_of(&StreamId("s1".into())).await, Some(60));
        assert_eq!(
            pool.viewers_of(&StreamId("s2".into())).await,
            None,
            "a stream the last census did not list is unknown, not zero"
        );

        pool.set_stream_viewers(HashMap::from([("s2".to_string(), 0)]))
            .await;
        assert_eq!(
            pool.viewers_of(&StreamId("s1".into())).await,
            None,
            "replaced, not merged"
        );
        assert_eq!(pool.viewers_of(&StreamId("s2".into())).await, Some(0));
    }

    /// No new viewer on a relay in its last minutes: the pool retires an idle relay five
    /// minutes before its deadline, so a viewer placed there would sit on a node about to
    /// be let go.
    #[tokio::test]
    async fn a_relay_near_its_deadline_takes_no_new_viewers() {
        let now = Utc::now();
        let s1 = StreamId("s1".into());

        // Two carriers; the near one is emptier, so headroom alone would pick it.
        let pool = SwitchPool::new(origin_client());
        pool.upsert_with_deadline(
            &node("near", Ownership::Rented, 250, 0),
            client("near"),
            Some(now + Duration::minutes(6)),
        )
        .await;
        pool.upsert_with_deadline(
            &node("far", Ownership::Rented, 250, 100),
            client("far"),
            Some(now + Duration::minutes(55)),
        )
        .await;
        carry(&pool, &["near", "far"], "s1").await;
        for _ in 0..20 {
            let (picked, _) = pool.assign_fanout_node_at(&s1, now).await.expect("far");
            assert_eq!(picked.as_str(), "far");
        }

        // The near one alone: nobody is offered, the viewer goes to the origin.
        pool.evict(&NodeId::new("far")).await;
        assert!(pool.assign_fanout_node_at(&s1, now).await.is_none());

        // Exactly at the guard is still inside the last minutes.
        let pool = SwitchPool::new(origin_client());
        pool.upsert_with_deadline(
            &node("edge", Ownership::Rented, 250, 0),
            client("edge"),
            Some(now + PLACEMENT_DEADLINE_GUARD),
        )
        .await;
        carry(&pool, &["edge"], "s1").await;
        assert!(pool.assign_fanout_node_at(&s1, now).await.is_none());

        // A node with no deadline (not rented) is always eligible.
        pool.upsert(&node("owned", Ownership::Leased, 250, 200), client("owned"))
            .await;
        carry(&pool, &["owned"], "s1").await;
        let (picked, _) = pool.assign_fanout_node_at(&s1, now).await.expect("owned");
        assert_eq!(picked.as_str(), "owned");
    }

    #[tokio::test]
    async fn recording_and_ingest_stay_on_the_origin() {
        let pool = SwitchPool::new(origin_client());
        pool.upsert(&node("n1", Ownership::Rented, 250, 0), client("n1")).await;

        let got = pool.origin_for(&StreamId("s1".into()));
        assert!(
            Arc::ptr_eq(&got, &pool.origin()),
            "the publisher publishes to the origin and the recorder taps that \
             fan-out, so moving ingest would move the recording with it"
        );
    }
}

#[cfg(test)]
mod stickiness_tests {
    use super::*;
    use mm_core::fleet::{NodeFlavor, NodeState, Ownership};
    use std::sync::Arc;

    fn client(host: &str) -> Arc<SwitchClient> {
        Arc::new(SwitchClient::new(&format!("http://{host}.invalid:7890")))
    }

    fn node(id: &str, capacity: u32, current: u32) -> FleetNode {
        FleetNode {
            id: NodeId::new(id),
            flavor: NodeFlavor::Fanout,
            ownership: Ownership::Rented,
            state: NodeState::Healthy,
            viewer_capacity: capacity,
            viewers_current: current,
            funding: mm_core::fleet::funding::Funding::Wallet,
        }
    }

    /// The proxy reuses a viewer's existing node across re-offers. This is the pool
    /// behaviour that makes it possible: a bound viewer resolves to its own node
    /// even when another node is emptier.
    ///
    /// Without stickiness a reconnect registers the same viewer on a second node
    /// while the first keeps a stale entry until it is destroyed — and the ad
    /// binding moves while the viewer's PeerConnection does not.
    #[tokio::test]
    async fn a_bound_viewer_resolves_to_its_own_node_even_when_another_is_emptier() {
        let pool = SwitchPool::new(client("origin"));
        let busy = client("busy");
        pool.upsert(&node("busy", 250, 200), busy.clone()).await;
        pool.upsert(&node("empty", 250, 0), client("empty")).await;
        for id in ["busy", "empty"] {
            pool.set_carried(&NodeId::new(id), HashSet::from(["s1".to_string()]))
                .await;
        }
        pool.bind_viewer(ViewerId::new("v1"), NodeId::new("busy")).await;

        // assign_fanout_node would pick `empty`...
        let (picked, _) = pool
            .assign_fanout_node(&StreamId("s1".into()))
            .await
            .expect("a placeable node");
        assert_eq!(picked.as_str(), "empty");

        // ...but the viewer's own node is where they already are.
        let bound = pool
            .node_of_viewer(&ViewerId::new("v1"))
            .await
            .expect("bound");
        assert_eq!(bound.as_str(), "busy");
        let resolved = pool.client_for_node(Some(&bound)).await.expect("client");
        assert!(Arc::ptr_eq(&resolved, &busy));
    }

    /// And when that node is gone, the binding is gone with it, so the proxy falls
    /// through to a fresh assignment rather than resolving to nothing.
    #[tokio::test]
    async fn an_evicted_nodes_viewer_has_no_binding_to_be_sticky_to() {
        let pool = SwitchPool::new(client("origin"));
        pool.upsert(&node("gone", 250, 1), client("gone")).await;
        pool.bind_viewer(ViewerId::new("v1"), NodeId::new("gone")).await;
        pool.evict(&NodeId::new("gone")).await;

        assert!(pool.node_of_viewer(&ViewerId::new("v1")).await.is_none());
    }
}
