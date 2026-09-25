//! The two backstops (WS-B Tasks B3 and B4).
//!
//! Neither of these is a capacity mechanism. Both exist because the primary path
//! can fail, and when it does the failure is silent and expensive: a machine keeps
//! billing and nothing in the product looks wrong. That is why
//! `mm_fleet_reaper_deadline_kills_total` and `mm_fleet_orphans_destroyed_total`
//! are alerted on ANY non-zero value (NFR-704) — a successful sweep is evidence of
//! an earlier failure, not of health.
//!
//! Both are split into a **pure selector** and a loop. The selectors take `now`
//! and a list; they read no clock and touch no network, so the dangerous
//! decisions — "destroy this" and "this is not ours" — are exhaustively testable.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use mm_core::fleet::NodeState;
use mm_core::metrics_global::{FLEET_ORPHANS_DESTROYED, FLEET_REAPER_DEADLINE_KILLS};

use crate::desired::{DesiredStore, ObservedNode, StoreError};
use crate::provider::{Provider, ProviderError};

// ─── B3: deadline sweeper ────────────────────────────────────────────────────

/// Nodes whose `destroy_deadline` has passed and which we are allowed to destroy.
///
/// Clock-free on purpose: `now` is an argument so "one second before" and "one
/// second after" are both testable, and so the selector cannot disagree with the
/// caller about what time it is mid-sweep.
///
/// Three filters, and each one has a failure it prevents:
///
/// * `Ownership::is_reapable` — otherwise a stray deadline on owned hardware
///   destroys a colocated machine, or a leased one forfeits its IPv4.
/// * `state != Gone` — destroying an already-destroyed node is a provider error
///   we would then alert on, hiding real ones.
/// * a deadline that exists and has passed.
pub fn due_for_reaping(now: DateTime<Utc>, nodes: &[ObservedNode]) -> Vec<&ObservedNode> {
    nodes
        .iter()
        .filter(|n| n.ownership.is_reapable())
        .filter(|n| n.state != NodeState::Gone)
        .filter(|n| n.destroy_deadline.is_some_and(|dl| dl <= now))
        .collect()
}

/// Outcome of one sweep, so the caller can log and the tests can assert without
/// scraping metrics.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub reaped: Vec<String>,
    pub failed: Vec<String>,
}

/// Destroy everything whose deadline has passed.
///
/// Every success increments `mm_fleet_reaper_deadline_kills_total`, which is
/// alerted on. That is deliberate and is the opposite of the usual convention: a
/// node should be torn down when its broadcast ends, so reaching the deadline
/// means the normal path did not run. The sweeper working is the bad news.
///
/// A failure does not abort the sweep. Each node is independent, and one provider
/// error must not leave the rest of an over-running fleet alive.
pub async fn sweep_deadlines(
    store: &DesiredStore,
    provider: &dyn Provider,
    now: DateTime<Utc>,
) -> Result<SweepReport, StoreError> {
    let nodes = store.load_nodes().await?;
    let mut report = SweepReport::default();

    for node in due_for_reaping(now, &nodes) {
        let overrun = node
            .destroy_deadline
            .map(|dl| (now - dl).num_seconds())
            .unwrap_or_default();

        match store.teardown(provider, &node.teardown_target()).await {
            Ok(()) => {
                FLEET_REAPER_DEADLINE_KILLS
                    .with_label_values(&[node.flavor.as_str()])
                    .inc();
                tracing::warn!(
                    node = %node.mm_node_id,
                    flavor = %node.flavor,
                    overrun_secs = overrun,
                    "fleet node destroyed by DEADLINE, not by teardown — the normal \
                     path did not run, and this node was billed for the overrun"
                );
                report.reaped.push(node.mm_node_id.as_str().to_string());
            }
            Err(e) => {
                tracing::error!(
                    node = %node.mm_node_id,
                    overrun_secs = overrun,
                    error = %e,
                    "deadline teardown FAILED — the node is still billing"
                );
                report.failed.push(node.mm_node_id.as_str().to_string());
            }
        }
    }

    Ok(report)
}

// ─── B4: orphan sweeper ──────────────────────────────────────────────────────

/// Instances the provider is running that we have no record of.
///
/// A pure set difference, so the dangerous half is testable without a provider.
/// The danger is not in the arithmetic — it is in what the caller does when the
/// listing fails; see [`sweep_orphans`].
pub fn orphans(at_provider: &[String], known: &HashSet<String>) -> Vec<String> {
    at_provider
        .iter()
        .filter(|id| !known.contains(*id))
        .cloned()
        .collect()
}

/// Destroy provider instances we have no record of.
///
/// **A listing failure returns `Err` and destroys nothing.** This is the single
/// most dangerous confusion available in this crate: an empty list is
/// indistinguishable from "every instance is an orphan", so treating an error as
/// an empty list — or, worse, defaulting to `Vec::new()` — would destroy the
/// entire fleet on a provider 503.
///
/// The `known` set is drawn from every node row we hold **regardless of state**,
/// including `gone`. A row is kept after destruction precisely so its billing can
/// be closed, and a `gone` row whose machine somehow survived is a node we know
/// about — treating it as an orphan would be right by accident and wrong by
/// reasoning, because the same logic would reap a node mid-create whose row exists
/// and whose state has not caught up.
pub async fn sweep_orphans(
    store: &DesiredStore,
    provider: &dyn Provider,
    now: DateTime<Utc>,
) -> Result<SweepReport, ProviderError> {
    let _ = now;

    // Ours first. If this fails we must not list, because an empty `known` set
    // makes every running instance an orphan.
    let nodes = store
        .load_nodes()
        .await
        .map_err(|e| ProviderError::Transient(format!("cannot read our own nodes: {e}")))?;
    let known: HashSet<String> = nodes.iter().filter_map(|n| n.provider_id.clone()).collect();

    let listed = provider.list().await?;
    let at_provider: Vec<String> = listed.into_iter().map(|h| h.provider_id).collect();

    let mut report = SweepReport::default();
    for id in orphans(&at_provider, &known) {
        match provider.destroy(&id).await {
            Ok(()) => {
                FLEET_ORPHANS_DESTROYED
                    .with_label_values(&[provider.name()])
                    .inc();
                tracing::warn!(
                    provider = provider.name(),
                    provider_id = %id,
                    "destroyed an ORPHAN instance — we were being billed for a machine \
                     we had no record of, so the desired-set bookkeeping failed"
                );
                report.reaped.push(id);
            }
            Err(e) => {
                tracing::error!(provider_id = %id, error = %e, "orphan destroy failed");
                report.failed.push(id);
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use mm_core::fleet::{NodeFlavor, NodeId, Ownership};

    fn node(
        id: &str,
        ownership: Ownership,
        state: NodeState,
        deadline: Option<DateTime<Utc>>,
    ) -> ObservedNode {
        ObservedNode {
            mm_node_id: NodeId::new(id),
            flavor: NodeFlavor::Fanout,
            ownership,
            state,
            provider_id: Some(format!("prov-{id}")),
            destroy_deadline: deadline,
            viewer_capacity: 250,
            viewers_current: 0,
        }
    }

    fn ids(nodes: Vec<&ObservedNode>) -> Vec<&str> {
        nodes.iter().map(|n| n.mm_node_id.as_str()).collect()
    }

    // ── the deadline selector ────────────────────────────────────────────────

    #[test]
    fn a_rented_node_one_second_past_its_deadline_is_due() {
        let now = Utc::now();
        let nodes = [node(
            "late",
            Ownership::Rented,
            NodeState::Healthy,
            Some(now - Duration::seconds(1)),
        )];
        assert_eq!(ids(due_for_reaping(now, &nodes)), vec!["late"]);
    }

    #[test]
    fn a_rented_node_one_second_before_its_deadline_is_not_due() {
        let now = Utc::now();
        let nodes = [node(
            "early",
            Ownership::Rented,
            NodeState::Healthy,
            Some(now + Duration::seconds(1)),
        )];
        assert!(due_for_reaping(now, &nodes).is_empty());
    }

    #[test]
    fn a_node_exactly_at_its_deadline_is_due() {
        // <= rather than <: a deadline that has arrived has arrived, and the
        // alternative leaves a node alive until the next tick for no reason.
        let now = Utc::now();
        let nodes = [node("exact", Ownership::Rented, NodeState::Healthy, Some(now))];
        assert_eq!(ids(due_for_reaping(now, &nodes)), vec!["exact"]);
    }

    /// THE GUARD TEST. A deadline on owned hardware is itself a bug, and the
    /// sweeper must not act on it — that machine is colocated and paid for.
    #[test]
    fn owned_and_leased_nodes_are_never_due_however_old_their_deadline() {
        let now = Utc::now();
        let ancient = Some(DateTime::from_timestamp(0, 0).unwrap().to_utc());
        for ownership in [Ownership::Owned, Ownership::Leased] {
            let nodes = [node("protected", ownership, NodeState::Healthy, ancient)];
            assert!(
                due_for_reaping(now, &nodes).is_empty(),
                "{ownership} node with a 1970 deadline was selected for destruction — \
                 is_reapable is not in the selector's path"
            );
        }
    }

    #[test]
    fn a_gone_node_is_not_due() {
        let now = Utc::now();
        let nodes = [node(
            "already-gone",
            Ownership::Rented,
            NodeState::Gone,
            Some(now - Duration::hours(5)),
        )];
        assert!(
            due_for_reaping(now, &nodes).is_empty(),
            "destroying an already-destroyed node is a provider error we would then \
             alert on, hiding the real ones"
        );
    }

    #[test]
    fn a_node_with_no_deadline_is_never_due() {
        let now = Utc::now();
        let nodes = [node("no-deadline", Ownership::Rented, NodeState::Healthy, None)];
        assert!(due_for_reaping(now, &nodes).is_empty());
    }

    /// Draining and destroying nodes ARE due. Draining is bounded, and a node
    /// stuck in either state past its deadline is the exact leak this exists for.
    #[test]
    fn draining_and_destroying_nodes_past_deadline_are_due() {
        let now = Utc::now();
        for state in [NodeState::Draining, NodeState::Destroying, NodeState::Booting] {
            let nodes = [node("stuck", Ownership::Rented, state, Some(now - Duration::minutes(1)))];
            assert_eq!(
                ids(due_for_reaping(now, &nodes)),
                vec!["stuck"],
                "a node stuck in {state} past its deadline is still billing"
            );
        }
    }

    #[test]
    fn the_selector_returns_every_due_node_not_just_the_first() {
        let now = Utc::now();
        let past = Some(now - Duration::minutes(5));
        let nodes = [
            node("a", Ownership::Rented, NodeState::Healthy, past),
            node("b", Ownership::Owned, NodeState::Healthy, past),
            node("c", Ownership::Rented, NodeState::Healthy, past),
        ];
        assert_eq!(ids(due_for_reaping(now, &nodes)), vec!["a", "c"]);
    }

    // ── the orphan selector ──────────────────────────────────────────────────

    #[test]
    fn an_instance_we_have_no_record_of_is_an_orphan() {
        let known: HashSet<String> = ["prov-a".to_string()].into_iter().collect();
        let at_provider = vec!["prov-a".to_string(), "someone-elses".to_string()];
        assert_eq!(orphans(&at_provider, &known), vec!["someone-elses"]);
    }

    #[test]
    fn nothing_is_an_orphan_when_everything_is_known() {
        let known: HashSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        assert!(orphans(&["a".to_string(), "b".to_string()], &known).is_empty());
    }

    /// The arithmetic that makes an empty `known` set catastrophic, stated as a
    /// test so the reason sweep_orphans refuses to proceed on a read failure is
    /// visible rather than inferred.
    #[test]
    fn an_empty_known_set_makes_every_instance_an_orphan() {
        let known = HashSet::new();
        let at_provider = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(orphans(&at_provider, &known).len(), 3);
    }

    #[test]
    fn an_empty_provider_listing_yields_no_orphans() {
        let known: HashSet<String> = ["a".to_string()].into_iter().collect();
        assert!(orphans(&[], &known).is_empty());
    }
}
