//! The reconcile loop (WS-B Task B5).
//!
//! One tick: observe what exists, ask the pure planner what should exist, write
//! the difference to the desired set. Terraform (B6) turns the desired set into
//! machines; nothing here calls a provider except to tear down.
//!
//! ## Why this cannot spend money yet, by construction
//!
//! Planning needs a wallet balance and a projected cost (FR-308), and the wallet
//! is WS-D. [`NoBillingYet`] — the default — **fails** rather than quoting zero,
//! and a broadcast whose billing cannot be quoted is skipped. So the runner is
//! safe to deploy before WS-D exists: it observes, publishes metrics, and
//! provisions nothing. That is enforced by the code path, not by a comment.
//!
//! Returning a zero quote instead would have been worse than useless: the balance
//! gate is `projected_cost > available_balance`, and `0 > 0` is false, so a zero
//! quote **passes** the gate and authorises spending.
//!
//! ## The three teardown paths, all explicit
//!
//! `plan()` never destroys — a gate stops growth. Capacity goes away only when:
//!
//! 1. **the broadcast is no longer live** — here, [`FleetRunner::tick`];
//! 2. **`fleet=off`** — here, the kill-switch drain;
//! 3. **a deadline passed** — [`crate::sweeper::sweep_deadlines`];
//! 4. **the broadcaster no longer wants its transcoder** — opted out, or an
//!    operator released it (FR-314c) — here, in `plan_one`.
//!
//! All four go through [`crate::desired::DesiredStore::teardown`], and therefore
//! through `Ownership::is_reapable`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mm_core::config::FleetMode;
use mm_core::fleet::planner::{plan, FleetObservation, FleetPolicy};
use mm_core::fleet::transcode::TranscodeOptIn;
use mm_core::fleet::{FleetNode, NodeFlavor, NodeId, NodeState};
use mm_core::metrics_global::{publish_fleet_nodes, FLEET_PROVISION_SECONDS};

use crate::desired::{DesiredStore, ObservedNode, StoreError};
use crate::provider::Provider;
use crate::tfvars::TfvarsWriter;

/// A broadcast that is on air, with its current audience.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBroadcast {
    pub broadcast_id: String,
    pub viewers: u32,
}

/// Who is watching, and whether there is anything to watch.
#[async_trait]
pub trait BroadcastCensus: Send + Sync {
    async fn live_broadcasts(&self) -> Result<Vec<LiveBroadcast>, String>;

    /// Is real programme content flowing, as opposed to a waiting slate?
    ///
    /// Only the switch can answer this — it is "does a non-slate source exist for
    /// `stream-{id}`" — so the database-backed implementation cannot, and says so
    /// by answering `false`. With a gate that stops growth rather than destroying
    /// (planner §Gate 1), a conservative `false` costs a broadcast its fan-out
    /// *growth* and nothing it already has.
    async fn programme_is_live(&self, broadcast_id: &str) -> Result<bool, String>;
}

/// What a broadcast may spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastBilling {
    pub available_balance_minor: i64,
    pub projected_cost_minor: i64,
    /// Is the payer a paying broadcaster? FR-314a's "transcode is for paying
    /// broadcasters only", ANDed by the planner with the broadcaster's stored
    /// opt-in. This is a billing fact, NOT the opt-in: until FR-314b it was used as
    /// one, which would have given every funded broadcast a GPU.
    pub broadcaster_is_paying: bool,
    /// What ONE more transcoder would add to `projected_cost_minor` over the same
    /// horizon; `0` when the rate card has no `gpu_minute` price, which the planner
    /// reads as "may not order one".
    pub transcoder_cost_minor: i64,
}

#[async_trait]
pub trait BillingSource: Send + Sync {
    async fn quote(&self, broadcast_id: &str) -> Result<BroadcastBilling, String>;
}

/// The broadcaster's stored transcode choice for a broadcast (FR-314a/c).
///
/// Separate from [`BillingSource`] because it is a choice, not a price: the
/// demotion ladder quotes billing and has no use for it.
#[async_trait]
pub trait TranscodeOptIns: Send + Sync {
    async fn opt_in(&self, broadcast_id: &str) -> Result<TranscodeOptIn, String>;
}

/// Backed by `mm_streams` + `mm_creator_defaults` (V040).
pub struct PgTranscodeOptIns {
    pool: sqlx::PgPool,
}

impl PgTranscodeOptIns {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TranscodeOptIns for PgTranscodeOptIns {
    async fn opt_in(&self, broadcast_id: &str) -> Result<TranscodeOptIn, String> {
        // A broadcast with no row cannot have chosen anything, and the census only
        // lists rows that exist — so a missing one is an error, not "opted out":
        // an error skips the broadcast, visibly, in the tick report.
        mm_db::transcode_db::for_broadcast(&self.pool, broadcast_id)
            .await
            .map_err(|e| format!("reading the transcode opt-in failed: {e}"))?
            .map(|b| b.opt_in)
            .ok_or_else(|| format!("no broadcast {broadcast_id} to read a transcode opt-in from"))
    }
}

/// The default until WS-D ships: refuses to quote.
///
/// **Not a zero quote.** `projected_cost > available_balance` with both zero is
/// false, so a zero quote passes the balance gate and authorises spending. An
/// error skips the broadcast.
pub struct NoBillingYet;

#[async_trait]
impl BillingSource for NoBillingYet {
    async fn quote(&self, broadcast_id: &str) -> Result<BroadcastBilling, String> {
        Err(format!(
            "no wallet for broadcast {broadcast_id}: prepaid billing is WS-D and does not exist yet, \
             so nothing may be provisioned on its behalf"
        ))
    }
}

/// What one tick did, so callers can log and tests can assert without scraping
/// metrics.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub mode: &'static str,
    /// Broadcasts whose desired set was written.
    pub planned: Vec<String>,
    /// Broadcasts skipped, with why — an unquotable wallet, a census failure.
    pub skipped: Vec<(String, String)>,
    /// Nodes torn down because their broadcast ended, or because of `fleet=off`.
    pub torn_down: Vec<String>,
    pub teardown_failures: Vec<String>,
    pub nodes_observed: usize,
    /// How many nodes the rendered tfvars file now names. `None` when no
    /// Terraform directory is configured, which is different from `Some(0)`.
    pub tfvars_nodes: Option<usize>,
}

pub struct FleetRunner {
    store: DesiredStore,
    census: Box<dyn BroadcastCensus>,
    billing: Box<dyn BillingSource>,
    transcode: Box<dyn TranscodeOptIns>,
    policy: FleetPolicy,
    /// Where the desired set is rendered for Terraform. `None` means "do not
    /// render", which is what every deployment without a Terraform working
    /// directory wants — and what the tests use when they are asserting the
    /// database rather than the file.
    tfvars: Option<TfvarsWriter>,
    /// Nodes whose provision time has already been observed. In memory, so a
    /// restart loses it: a missed histogram sample is acceptable, a duplicated one
    /// would skew the only measurement we have of provision-to-ready.
    timed: Mutex<HashSet<NodeId>>,
}

impl FleetRunner {
    /// `transcode` is required rather than defaulted: a runner wired without it
    /// would silently ignore every broadcaster's opt-in (FR-314a).
    pub fn new(
        store: DesiredStore,
        census: Box<dyn BroadcastCensus>,
        billing: Box<dyn BillingSource>,
        transcode: Box<dyn TranscodeOptIns>,
        policy: FleetPolicy,
    ) -> Self {
        Self {
            store,
            census,
            billing,
            transcode,
            policy,
            tfvars: None,
            timed: Mutex::new(HashSet::new()),
        }
    }

    /// Render the desired set to `dir/desired_nodes.auto.tfvars.json` at the end of
    /// every tick that changed it.
    pub fn with_tfvars(mut self, writer: TfvarsWriter) -> Self {
        self.tfvars = Some(writer);
        self
    }

    /// One reconcile pass.
    ///
    /// `mode` is passed in rather than read from config so a change takes effect
    /// on the next tick with no restart (FR-341), and so the tests can drive all
    /// three modes against one runner.
    pub async fn tick(
        &self,
        provider: &dyn Provider,
        mode: FleetMode,
        now: DateTime<Utc>,
    ) -> Result<TickReport, StoreError> {
        let nodes = self.store.load_nodes().await?;
        let desired = self.store.load_all().await?;

        let mut report = TickReport {
            mode: match mode {
                FleetMode::On => "on",
                FleetMode::Frozen => "frozen",
                FleetMode::Off => "off",
            },
            nodes_observed: nodes.len(),
            ..Default::default()
        };

        // Metrics publish in EVERY mode, including both kill-switch modes. A
        // frozen subsystem you cannot see is a subsystem you cannot decide to
        // unfreeze.
        publish_fleet_nodes(&to_fleet_nodes(&nodes));
        self.observe_provision_times(&nodes, &desired, now);

        if mode == FleetMode::Off {
            // The hard stop. Teardown goes through the NORMAL path, so the
            // deadline counters stay at zero — which is what E-16 asserts, and
            // what distinguishes "an operator stopped the fleet" from "the
            // backstop caught a leak".
            for node in nodes.iter().filter(|n| n.is_reapable_now()) {
                self.tear_down(provider, node, &mut report).await;
            }
            // `off` is the one caller allowed past the shrink guard: removing the
            // whole fleet is the instruction, not a symptom of a partial read.
            self.render_tfvars(&mut report, true).await;
            return Ok(report);
        }

        // Broadcasts that have desired rows but are no longer on air. Done in
        // BOTH remaining modes: `frozen` stops provisioning, it does not mean
        // "keep paying for finished broadcasts".
        let live = match self.census.live_broadcasts().await {
            Ok(live) => live,
            Err(e) => {
                // A census failure must not look like "no broadcasts are live",
                // which would tear the whole fleet down.
                report
                    .skipped
                    .push(("*".into(), format!("census unavailable: {e}")));
                return Ok(report);
            }
        };
        let live_ids: HashSet<&str> = live.iter().map(|b| b.broadcast_id.as_str()).collect();

        for node in &nodes {
            let Some(bc) = desired
                .iter()
                .find(|d| d.mm_node_id == node.mm_node_id)
                .and_then(|d| d.broadcast_id.clone())
            else {
                continue;
            };
            if !live_ids.contains(bc.as_str()) && node.is_reapable_now() {
                self.tear_down(provider, node, &mut report).await;
            }
        }

        if !mode.allows_placement() {
            // `frozen`: observed, published, finished broadcasts released, and
            // nothing planned or provisioned.
            self.render_tfvars(&mut report, false).await;
            return Ok(report);
        }

        for bc in live {
            match self.plan_one(provider, &bc, &nodes, now, &mut report).await {
                Ok(()) => report.planned.push(bc.broadcast_id),
                Err(why) => report.skipped.push((bc.broadcast_id, why)),
            }
        }

        self.render_tfvars(&mut report, false).await;
        Ok(report)
    }

    /// Render the desired set for Terraform, from a FRESH read.
    ///
    /// Re-reading rather than rendering the set this tick assembled is deliberate:
    /// the file must describe the database, and any node this tick tore down or
    /// failed to write must be reflected as it actually is. Every key missing from
    /// that file is a machine Terraform destroys, so the only safe source is the
    /// thing that is true.
    ///
    /// A render failure does not fail the tick — the database is already correct and
    /// the next tick retries — but it is logged at error, because until it succeeds
    /// Terraform is acting on a stale desired set.
    async fn render_tfvars(&self, report: &mut TickReport, allow_shrink: bool) {
        let Some(writer) = self.tfvars.as_ref() else {
            return;
        };
        let rows = match self.store.load_all().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!(error = %e, "cannot read the desired set to render tfvars — \
                    NOT writing the file: a partial read would look like a teardown");
                return;
            }
        };
        match writer.write(&rows, allow_shrink) {
            Ok(written) => report.tfvars_nodes = Some(written.len()),
            Err(e) => tracing::error!(
                error = %e,
                "rendering desired_nodes.auto.tfvars.json failed — Terraform is now \
                 acting on a stale desired set"
            ),
        }
    }

    async fn plan_one(
        &self,
        provider: &dyn Provider,
        bc: &LiveBroadcast,
        nodes: &[ObservedNode],
        now: DateTime<Utc>,
        report: &mut TickReport,
    ) -> Result<(), String> {
        // The opt-in first, and acted on before billing is asked anything: a
        // transcoder the broadcaster no longer wants — opted out, or released by an
        // operator (FR-314c) — is torn down here, explicitly. Not left to Terraform
        // noticing a missing desired row: the tfvars shrink guard refuses to remove
        // a lone GPU, and it would bill until the deadline sweeper. And not behind
        // the quote: a broadcast whose wallet cannot be quoted must still be able to
        // stop spending.
        let transcode = self.transcode.opt_in(&bc.broadcast_id).await?;
        if !transcode.wants_transcoder() {
            for node in unwanted_transcoders(nodes, &bc.broadcast_id) {
                self.tear_down(provider, node, report).await;
            }
        }

        let billing = self.billing.quote(&bc.broadcast_id).await?;
        let programme_is_live = self.census.programme_is_live(&bc.broadcast_id).await?;

        let obs = FleetObservation {
            broadcast_id: bc.broadcast_id.clone(),
            programme_is_live,
            transcode,
            broadcaster_is_paying: billing.broadcaster_is_paying,
            viewers_projected: bc.viewers,
            available_balance_minor: billing.available_balance_minor,
            projected_cost_minor: billing.projected_cost_minor,
            transcoder_cost_minor: billing.transcoder_cost_minor,
            nodes: nodes_for_broadcast(nodes, &bc.broadcast_id),
        };

        let want = plan(&obs, &self.policy);
        self.store
            .upsert_for_broadcast(&bc.broadcast_id, &want, now)
            .await
            .map_err(|e| format!("writing the desired set failed: {e}"))
    }

    async fn tear_down(
        &self,
        provider: &dyn Provider,
        node: &ObservedNode,
        report: &mut TickReport,
    ) {
        match self.store.teardown(provider, &node.teardown_target()).await {
            Ok(()) => report.torn_down.push(node.mm_node_id.as_str().to_string()),
            Err(e) => {
                tracing::error!(node = %node.mm_node_id, error = %e, "fleet teardown failed");
                report
                    .teardown_failures
                    .push(node.mm_node_id.as_str().to_string());
            }
        }
    }

    /// Observe `Requested`→`Healthy` for any node not yet timed.
    ///
    /// This is the only place both ends of provision-to-ready are visible, and it
    /// is one of the four measurements the design leaves open.
    fn observe_provision_times(
        &self,
        nodes: &[ObservedNode],
        desired: &[crate::desired::DesiredRow],
        now: DateTime<Utc>,
    ) {
        let requested: HashMap<&NodeId, DateTime<Utc>> =
            desired.iter().map(|d| (&d.mm_node_id, d.requested_at)).collect();

        let mut timed = self.timed.lock().expect("provision-time set");
        for node in nodes.iter().filter(|n| n.state == NodeState::Healthy) {
            if timed.contains(&node.mm_node_id) {
                continue;
            }
            if let Some(secs) = provision_seconds(requested.get(&node.mm_node_id).copied(), now) {
                FLEET_PROVISION_SECONDS
                    .with_label_values(&[node.flavor.as_str()])
                    .observe(secs);
                timed.insert(node.mm_node_id.clone());
            }
        }
    }
}

/// Seconds from "we wanted it" to now, or `None` when that cannot be known or
/// would be nonsense.
///
/// A negative interval is discarded rather than clamped to zero. It means the
/// clocks disagree — the row was written by another process, or `now` came from a
/// test — and a pile of zeroes in the histogram would read as "provisioning is
/// instant", which is a worse lie than a missing sample.
pub fn provision_seconds(requested_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<f64> {
    let requested_at = requested_at?;
    let secs = (now - requested_at).num_milliseconds() as f64 / 1000.0;
    (secs >= 0.0).then_some(secs)
}

/// Nodes belonging to one broadcast, as the planner's observation wants them.
fn nodes_for_broadcast(nodes: &[ObservedNode], broadcast_id: &str) -> Vec<FleetNode> {
    // Node ids are `bc-{broadcast}-{flavor}[-{ordinal}]` (planner::DesiredNode), so
    // the prefix is the link. Matching on the id rather than joining the desired
    // table matters because teardown deletes the desired row first: a node mid-
    // teardown must still be visible to its own broadcast's plan.
    let prefix = format!("bc-{broadcast_id}-");
    nodes
        .iter()
        .filter(|n| n.mm_node_id.as_str().starts_with(&prefix))
        .map(observed_to_fleet_node)
        .collect()
}

/// This broadcast's transcoders that a teardown can still act on: reapable, and
/// not already `Gone` or `Destroying`. A `Destroying` node's destroy already failed
/// once; the retry belongs to the deadline sweeper (`sweep_deadlines` re-attempts
/// any non-gone node past `mm_fleet_nodes.destroy_deadline`), not to every tick.
/// The orphan sweeper does NOT retry it: the node's row makes its provider id
/// "known".
fn unwanted_transcoders<'a>(
    nodes: &'a [ObservedNode],
    broadcast_id: &str,
) -> impl Iterator<Item = &'a ObservedNode> {
    let prefix = format!("bc-{broadcast_id}-");
    nodes.iter().filter(move |n| {
        n.flavor == NodeFlavor::Transcode
            && n.mm_node_id.as_str().starts_with(&prefix)
            && n.is_reapable_now()
            && n.state != NodeState::Destroying
    })
}

fn to_fleet_nodes(nodes: &[ObservedNode]) -> Vec<FleetNode> {
    nodes.iter().map(observed_to_fleet_node).collect()
}

fn observed_to_fleet_node(n: &ObservedNode) -> FleetNode {
    FleetNode {
        id: n.mm_node_id.clone(),
        flavor: n.flavor,
        ownership: n.ownership,
        state: n.state,
        // Carried through, not zeroed. The planner reads capacity only through
        // `headroom()`, so a node passed in as 0/0 has zero spare capacity — and
        // the planner would then order a replacement for a machine that is
        // already serving. `viewer_capacity` is 0 only until the node reports,
        // and for a Requested/Booting node the planner substitutes the policy's
        // assumption anyway.
        viewer_capacity: n.viewer_capacity,
        viewers_current: n.viewers_current,
    }
}

impl ObservedNode {
    /// May this node be torn down right now?
    pub fn is_reapable_now(&self) -> bool {
        self.ownership.is_reapable() && self.state != NodeState::Gone
    }
}
