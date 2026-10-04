//! The boundary between "we decided to spend money" and "money is being spent".
//!
//! Everything above this trait is testable without an account, which is the
//! point: the ordering invariants, the sweepers and the reconcile loop are where
//! the cost bugs live, and none of them should have to wait for a provider
//! decision (see dev plan §B.0 — which provider sells hourly instances is still
//! open).
//!
//! What must NOT be inferred from a green test against [`DryRunProvider`]: that
//! a real provider destroys what it is told to. That claim transfers only at a
//! real `impl`, and each one owes its own integration test.

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mm_core::fleet::{NodeFlavor, NodeId};

/// What to ask a provider for. Deliberately small: anything the provider does
/// not need in order to create the instance belongs in the desired row, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    /// Our stable id, which is also the Terraform `for_each` key. Passed to the
    /// provider as a tag/label wherever it supports one, because it is the only
    /// thing that lets the orphan sweeper tell "a machine we forgot" from "a
    /// machine belonging to something else entirely".
    pub mm_node_id: NodeId,
    pub flavor: NodeFlavor,
    pub region: String,
    pub size: String,
    /// Cloud-init / user-data. Carries `MM_SWITCH_NODE_FLAVOR` and the node's
    /// auth secret, without which a fleet node refuses to boot (FR-348).
    pub user_data: String,
}

/// What a provider gives back. `provider_id` is the handle every later call uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceHandle {
    pub provider_id: String,
    pub public_ip: Option<String>,
    /// When the provider created it. The orphan sweeper spares anything younger
    /// than its grace — and anything of unknown age — because a create in flight
    /// has a machine and no node row yet. A provider that never fills this in
    /// therefore never has an orphan reaped: report it.
    pub created_at: Option<DateTime<Utc>>,
}

/// Why a provider call failed, split by what the caller should do about it.
///
/// The split is load-bearing. Retrying a permanent failure forever is how a paid
/// machine outlives its deadline while the sweeper reports itself busy; giving up
/// on a transient one is how it outlives its deadline while the sweeper reports
/// itself finished.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Rate limit, timeout, 5xx. Retry with backoff.
    #[error("transient provider failure: {0}")]
    Transient(String),

    /// Bad credentials, unknown region, malformed request. No amount of retrying
    /// fixes it, so it must reach a human.
    #[error("permanent provider failure: {0}")]
    Permanent(String),

    /// This zone cannot supply this size right now (a GPU stock-out). Not this
    /// call's fault and not a human's problem: the caller should try another zone
    /// or provider, or this one again later — but never the same zone in a tight
    /// loop, which is how a stock-out turns into a rate limit.
    #[error("provider has no capacity: {0}")]
    Capacity(String),

    /// The account may not have more of this resource. Raised by a support
    /// ticket, not by retrying — and on day one it is the common case: the
    /// default GPU quota is one (Scaleway) or zero (AWS, GCP, Exoscale).
    #[error("provider quota exhausted: {0}")]
    Quota(String),
}

impl ProviderError {
    /// Retry the same call, with backoff.
    pub fn is_transient(&self) -> bool {
        matches!(self, ProviderError::Transient(_))
    }

    /// Try another zone or provider (or this one later).
    pub fn is_capacity(&self) -> bool {
        matches!(self, ProviderError::Capacity(_))
    }

    pub fn is_quota(&self) -> bool {
        matches!(self, ProviderError::Quota(_))
    }

    /// Page someone: nothing automatic will make this succeed.
    pub fn needs_human(&self) -> bool {
        matches!(self, ProviderError::Permanent(_) | ProviderError::Quota(_))
    }
}

/// A source of rented machines.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Short, stable name used as a metric label and stored on the node row.
    /// Must come from a fixed set — it is a Prometheus label value.
    fn name(&self) -> &'static str;

    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError>;

    /// Destroy an instance.
    ///
    /// **MUST be idempotent**: destroying an instance that is already gone is
    /// `Ok(())`, not an error. The sweeper retries, and a provider that reports
    /// failure for an already-destroyed instance keeps its row alive forever —
    /// so the fleet looks like it is still costing money and the real leak hides
    /// behind the noise.
    async fn destroy(&self, provider_id: &str) -> Result<(), ProviderError>;

    /// Every instance this provider believes it is running for us.
    ///
    /// Where a provider's volumes can outlive their instance (Scaleway SBS is only
    /// detached by `terminate`), this also reports the id of each instance that is
    /// gone but left a billable volume of ours behind. `destroy` of that id deletes
    /// it, so `destroy` must treat "instance gone" as "now remove what it left",
    /// not as "nothing to do". Who calls that `destroy`: the orphan sweeper for an
    /// id with no node row; the deadline sweeper for a known node, which stays in
    /// `destroying` until `destroy` returns Ok. A node marked `gone` is never
    /// retried, which is why `destroy` returns Ok only once every volume is gone.
    ///
    /// Used only by the orphan sweeper, and its failure mode is the reason that
    /// sweeper is careful: an `Err` here must never be treated as an empty list,
    /// because an empty list is indistinguishable from "every instance we know
    /// about is an orphan".
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError>;
}

/// What a [`DryRunProvider`] was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Create(NodeId),
    Destroy(String),
    List,
}

/// Records intents instead of performing them.
///
/// This exists so the ordering invariants and both sweepers can be tested
/// exhaustively before anyone has a provider account — and so the tests that
/// matter most (the ones about not spending money) never depend on a network.
#[derive(Default)]
pub struct DryRunProvider {
    state: Mutex<DryRunState>,
}

#[derive(Default)]
struct DryRunState {
    intents: Vec<Intent>,
    live: Vec<InstanceHandle>,
    /// When set, the next call of the matching kind fails with this error.
    fail_create: Option<ProviderError>,
    fail_destroy: Option<ProviderError>,
    fail_list: Option<ProviderError>,
}

impl DryRunProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every call made, in order. The order is the assertion in the teardown
    /// tests — a test that only checks the end state passes for both orderings
    /// and is therefore worthless for an invariant about sequence.
    pub fn intents(&self) -> Vec<Intent> {
        self.state.lock().expect("dry-run lock").intents.clone()
    }

    /// Instances the dry run currently believes exist.
    pub fn live(&self) -> Vec<InstanceHandle> {
        self.state.lock().expect("dry-run lock").live.clone()
    }

    /// Pre-seed instances so `list` has something to report — used by the orphan
    /// sweeper's tests, where the interesting input is a machine we never created.
    pub fn seed(&self, provider_ids: &[&str]) {
        let mut st = self.state.lock().expect("dry-run lock");
        for id in provider_ids {
            st.live.push(InstanceHandle {
                provider_id: (*id).to_string(),
                public_ip: None,
                // Long dead: a seeded instance stands for "a machine we never
                // created", and the orphan grace must not be what a test measures
                // unless it asks to (`seed_created_at`).
                created_at: Some(DateTime::<Utc>::UNIX_EPOCH),
            });
        }
    }

    /// Pre-seed one instance with an explicit creation time (`None` = unknown).
    pub fn seed_created_at(&self, provider_id: &str, created_at: Option<DateTime<Utc>>) {
        self.state.lock().expect("dry-run lock").live.push(InstanceHandle {
            provider_id: provider_id.to_string(),
            public_ip: None,
            created_at,
        });
    }

    pub fn fail_next_create(&self, err: ProviderError) {
        self.state.lock().expect("dry-run lock").fail_create = Some(err);
    }

    pub fn fail_next_destroy(&self, err: ProviderError) {
        self.state.lock().expect("dry-run lock").fail_destroy = Some(err);
    }

    pub fn fail_next_list(&self, err: ProviderError) {
        self.state.lock().expect("dry-run lock").fail_list = Some(err);
    }
}

#[async_trait]
impl Provider for DryRunProvider {
    fn name(&self) -> &'static str {
        "dry-run"
    }

    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        let mut st = self.state.lock().expect("dry-run lock");
        st.intents.push(Intent::Create(spec.mm_node_id.clone()));
        if let Some(err) = st.fail_create.take() {
            return Err(err);
        }
        let handle = InstanceHandle {
            provider_id: format!("dry-run-{}", spec.mm_node_id),
            public_ip: Some("203.0.113.1".to_string()),
            created_at: Some(Utc::now()),
        };
        st.live.push(handle.clone());
        Ok(handle)
    }

    async fn destroy(&self, provider_id: &str) -> Result<(), ProviderError> {
        let mut st = self.state.lock().expect("dry-run lock");
        st.intents.push(Intent::Destroy(provider_id.to_string()));
        if let Some(err) = st.fail_destroy.take() {
            return Err(err);
        }
        // Idempotent by construction: retaining everything that is not this id
        // succeeds whether or not the id was ever present.
        st.live.retain(|h| h.provider_id != provider_id);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        let mut st = self.state.lock().expect("dry-run lock");
        st.intents.push(Intent::List);
        if let Some(err) = st.fail_list.take() {
            return Err(err);
        }
        Ok(st.live.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> InstanceSpec {
        InstanceSpec {
            mm_node_id: NodeId::new(id),
            flavor: NodeFlavor::Fanout,
            region: "eu-ams".into(),
            size: "small".into(),
            user_data: "#cloud-config\n".into(),
        }
    }

    #[tokio::test]
    async fn dry_run_records_intents_without_performing_them() {
        let p = DryRunProvider::default();

        let h = p.create(&spec("n1")).await.expect("create");
        assert_eq!(h.provider_id, "dry-run-n1");

        p.destroy("dry-run-n1").await.expect("destroy");

        assert_eq!(
            p.intents(),
            vec![
                Intent::Create(NodeId::new("n1")),
                Intent::Destroy("dry-run-n1".into()),
            ]
        );
        assert!(p.live().is_empty(), "the destroyed instance must be gone");
    }

    /// Idempotence is load-bearing, not politeness. The sweeper retries; a
    /// provider that errors on an already-destroyed instance keeps its row alive
    /// forever, so the fleet looks like it is still costing money and the real
    /// leak hides behind the noise.
    #[tokio::test]
    async fn destroying_an_unknown_instance_is_success_not_failure() {
        let p = DryRunProvider::default();
        p.destroy("never-existed")
            .await
            .expect("destroy must be idempotent");
    }

    #[tokio::test]
    async fn destroying_twice_is_success_both_times() {
        let p = DryRunProvider::default();
        p.create(&spec("n1")).await.expect("create");
        p.destroy("dry-run-n1").await.expect("first destroy");
        p.destroy("dry-run-n1").await.expect("second destroy");
        assert!(p.live().is_empty());
    }

    /// A create that fails must still be recorded as attempted. The provider may
    /// have created the instance and failed to tell us, which is exactly why
    /// destroy_deadline is written before this call and why the orphan sweeper
    /// exists.
    #[tokio::test]
    async fn a_failed_create_is_still_recorded_as_an_attempt() {
        let p = DryRunProvider::default();
        p.fail_next_create(ProviderError::Transient("timeout".into()));

        let err = p.create(&spec("n1")).await.expect_err("must fail");
        assert!(err.is_transient());
        assert_eq!(
            p.intents(),
            vec![Intent::Create(NodeId::new("n1"))],
            "a create we cannot confirm is the one case where a machine may exist \
             that we have no handle for"
        );
    }

    #[test]
    fn transient_and_permanent_are_distinguishable() {
        assert!(ProviderError::Transient("429".into()).is_transient());
        assert!(!ProviderError::Permanent("bad credentials".into()).is_transient());
    }

    /// Three questions a caller asks of a failure, and each variant answers them
    /// differently: retry the same call? try somewhere else? page a human?
    #[test]
    fn capacity_and_quota_answer_the_three_questions_differently() {
        let transient = ProviderError::Transient("503".into());
        let capacity = ProviderError::Capacity("out_of_stock L4-1-24G".into());
        let quota = ProviderError::Quota("quotas_exceeded L4-1-24G 1/1".into());
        let permanent = ProviderError::Permanent("401".into());

        // Retry the same call?
        assert!(transient.is_transient());
        assert!(!capacity.is_transient(), "retrying the same zone blindly is the trap");
        assert!(!quota.is_transient());
        assert!(!permanent.is_transient());

        // Try another zone or provider?
        assert!(capacity.is_capacity());
        assert!(!quota.is_capacity() && !transient.is_capacity() && !permanent.is_capacity());

        // Page a human?
        assert!(quota.needs_human(), "a quota is raised by a support ticket");
        assert!(permanent.needs_human());
        assert!(!capacity.needs_human(), "a stock-out is weather, not an incident");
        assert!(!transient.needs_human());

        assert!(quota.is_quota() && !capacity.is_quota());
    }

    #[tokio::test]
    async fn list_reports_seeded_instances_so_orphans_can_be_simulated() {
        let p = DryRunProvider::default();
        p.seed(&["someone-elses-vm", "dry-run-n9"]);

        let listed = p.list().await.expect("list");
        let ids: Vec<&str> = listed.iter().map(|h| h.provider_id.as_str()).collect();
        assert_eq!(ids, vec!["someone-elses-vm", "dry-run-n9"]);
    }

    #[tokio::test]
    async fn a_list_failure_is_an_error_and_not_an_empty_list() {
        // The single most dangerous confusion in this crate: an empty list is
        // indistinguishable from "every instance we know about is an orphan", so
        // the orphan sweeper must see Err and stop, not Ok(vec![]).
        let p = DryRunProvider::default();
        p.seed(&["vm-1"]);
        p.fail_next_list(ProviderError::Transient("503".into()));

        let err = p.list().await.expect_err("must surface the failure");
        assert!(err.is_transient());

        // And it recovers, so a retry sees the truth rather than a cached lie.
        assert_eq!(p.list().await.expect("retry").len(), 1);
    }

    #[test]
    fn the_provider_name_is_a_bounded_metric_label() {
        // `provider` is a Prometheus label on mm_fleet_orphans_destroyed_total.
        // A &'static str from a fixed set is what keeps that bounded.
        let p = DryRunProvider::default();
        assert_eq!(p.name(), "dry-run");
    }
}
