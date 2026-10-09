//! Renting one node (spec §5.3): try each candidate in order until one creates.
//!
//! * The node row, with its deadline, is written before the create call (FR-202).
//! * A create whose outcome is unknown is looked up by its node tag before anything is
//!   retried: provider-side names are not unique, so a blind retry can rent twice. The
//!   lookup waits out the backoff first, so it trails the failed create.
//! * A create that timed out is never forgotten and never sent again in this call. The
//!   timeout can be this module's timer or the adapter's own (`ProviderError::Timeout`: its
//!   HTTP client gives up long before the timer does). If the lookup finds a machine it is
//!   destroyed; if it finds none, or fails, the zone is held and the node is recorded as
//!   "may exist", so the next tick looks again once the provider has settled.
//! * A half-made machine is destroyed, never adopted: nothing proves it received its
//!   cloud-init or powered on. Its teardown is ordered before its handle is recorded, so
//!   it never looks like a healthy booting node.
//! * Every create call, the first and each retry, is preceded by a leadership check: a
//!   runner that lost the lock stops creating, and leaves nothing it cannot account for.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use mm_core::fleet::{NodeFlavor, NodeId, Ownership};
use sqlx::PgPool;

use crate::adapters::{AdapterSource, ImageFor};
use crate::desired::{DesiredStore, TeardownTarget};
use crate::leadership::LeaderCheck;
use crate::nodes_db::{self, InsertRefused, NewNode};
use crate::placement::Candidate;
use crate::placement_db;
use crate::provider::{InstanceHandle, InstanceSpec, Provider, ProviderError};
use crate::providers_db as pdb;
use crate::redact::provider_text;
use crate::roles::Purpose;

/// A create that has not answered in this long is treated as "outcome unknown".
pub const CREATE_TIMEOUT: Duration = Duration::from_secs(600);
/// A zone refused for quota sits out a day, or until a successful Test connection.
pub const QUOTA_HOLD_SECS: i64 = 86_400;
/// Waits before the lookup that follows a failed create; the last step is reused once the
/// steps run out. Each step that finds nothing is followed by one more create, so this is
/// also the retry budget.
pub const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
];

/// Why a candidate was not created when the runner lost the lead. A caller that must stop at
/// once on a lost lead asks [`RentOutcome::lost_leadership`]; the reason may carry a suffix.
pub const NOT_LEADER: &str = "this runner is no longer the leader";
/// Why a candidate was passed over without an attempt.
const PROVIDER_REFUSED_EARLIER: &str =
    "skipped: this provider refused a create earlier in this call";

pub struct RentRequest<'a> {
    pub mm_node_id: &'a NodeId,
    pub purpose: Purpose,
    pub destroy_deadline: DateTime<Utc>,
    pub created_by: Option<&'a str>,
    pub user_data: &'a str,
    pub image: ImageFor,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RentOutcome {
    Created {
        candidate: Candidate,
        provider_id: String,
        create_secs: f64,
    },
    /// A machine may exist that no node row holds a handle for. Nothing else is created
    /// meanwhile. Four ways to get here:
    /// * the lookup after a failed create failed, or the create timed out and the lookup
    ///   found nothing yet: the row stays `requested` without a handle, and later ticks
    ///   look again;
    /// * a half-made machine was found but ordering its teardown failed: the row stays
    ///   `requested` without a handle and the desired row stands, so the next tick's lookup
    ///   finds the machine again;
    /// * the create succeeded but recording its handle failed: the row stays `requested`,
    ///   and the lookup finds the machine;
    /// * the create succeeded but the row would not take the handle (it may hold another
    ///   handle, or be gone): that machine is recorded nowhere, and only the orphan sweep
    ///   reaps it.
    MayExist { candidate: Candidate, error: String },
    /// A half-made machine was found and its teardown ordered: its desired row is gone and
    /// the node is marked `destroying`. The destroy is done, or still owed to the next pass
    /// (the message says which). This node id is spent.
    Abandoned { candidate: Candidate, error: String },
    /// Every candidate refused and nothing exists. `tried` says why, per candidate.
    NoneCreated { tried: Vec<(Candidate, String)> },
}

impl RentOutcome {
    /// True when the lead was lost before a create call, so nothing was asked of the provider
    /// for the candidate it names. A runner that lost the lead writes nothing more: its caller
    /// stops instead of recording a failure for a request it no longer owns.
    pub fn lost_leadership(&self) -> bool {
        matches!(self, RentOutcome::NoneCreated { tried }
            if tried.iter().any(|(_, why)| why.starts_with(NOT_LEADER)))
    }
}

pub struct RentCtx<'a> {
    pub pool: &'a PgPool,
    pub store: &'a DesiredStore,
    pub adapters: &'a dyn AdapterSource,
    /// Asked before every create call.
    pub leader: &'a dyn LeaderCheck,
    pub global_cap: i64,
    pub cooldown_secs: i64,
    pub backoff: &'a [Duration],
}

enum Attempt {
    Created(String, f64),
    /// Nothing was made. `provider_refused`: the provider refused for a reason only a human
    /// can fix, so its other candidates are skipped for the rest of this call.
    Next {
        why: String,
        provider_refused: bool,
    },
    MayExist(String),
    Abandoned(String),
    /// The lead was lost before a create call; nothing was asked of the provider.
    NotLeader,
}

pub async fn rent_one(
    ctx: &RentCtx<'_>,
    req: &RentRequest<'_>,
    candidates: &[Candidate],
) -> RentOutcome {
    rent_within(ctx, req, candidates, CREATE_TIMEOUT).await
}

/// Tests only (the `test-support` feature, never enabled in a production build): `rent_one`
/// with a create timeout short enough to wait out.
#[cfg(feature = "test-support")]
pub async fn rent_one_with_create_timeout(
    ctx: &RentCtx<'_>,
    req: &RentRequest<'_>,
    candidates: &[Candidate],
    create_timeout: Duration,
) -> RentOutcome {
    rent_within(ctx, req, candidates, create_timeout).await
}

async fn rent_within(
    ctx: &RentCtx<'_>,
    req: &RentRequest<'_>,
    candidates: &[Candidate],
    create_timeout: Duration,
) -> RentOutcome {
    let mut tried: Vec<(Candidate, String)> = Vec::new();
    let mut refused: HashSet<&str> = HashSet::new();
    for c in candidates {
        if refused.contains(c.provider_id.as_str()) {
            tried.push((c.clone(), PROVIDER_REFUSED_EARLIER.to_string()));
            continue;
        }
        let node = NewNode {
            mm_node_id: req.mm_node_id.as_str(),
            provider_ref: &c.provider_id,
            kind: &c.kind,
            zone: &c.zone,
            size: &c.size,
            purpose: req.purpose,
            destroy_deadline: req.destroy_deadline,
            created_by: req.created_by,
        };
        match nodes_db::insert_for_create(ctx.pool, &node, ctx.global_cap).await {
            Ok(()) => {}
            // Never create beside a row that exists (it may stand for a machine), never for a
            // node whose teardown was ordered, and never past the fleet-wide cap. Every other
            // candidate would refuse the same way, so none is tried.
            Err(
                e @ (InsertRefused::AlreadyExists
                | InsertRefused::NotDesired
                | InsertRefused::GlobalCap { .. }),
            ) => {
                tried.push((c.clone(), e.to_string()));
                return RentOutcome::NoneCreated { tried };
            }
            Err(e) => {
                tried.push((c.clone(), e.to_string()));
                continue;
            }
        }
        let adapter = match ctx
            .adapters
            .adapter(&c.provider_id, &c.zone, req.image)
            .await
        {
            Ok(a) => a,
            Err(why) => {
                if let Some(e) = clear_attempt(ctx, req).await {
                    tried.push((
                        c.clone(),
                        format!("{why}; and clearing the attempt failed: {e}"),
                    ));
                    return RentOutcome::NoneCreated { tried };
                }
                tried.push((c.clone(), why));
                continue;
            }
        };
        match attempt(ctx, req, c, adapter.as_ref(), create_timeout).await {
            Attempt::Created(provider_id, create_secs) => {
                return RentOutcome::Created {
                    candidate: c.clone(),
                    provider_id,
                    create_secs,
                };
            }
            Attempt::MayExist(error) => {
                return RentOutcome::MayExist {
                    candidate: c.clone(),
                    error,
                };
            }
            Attempt::Abandoned(error) => {
                return RentOutcome::Abandoned {
                    candidate: c.clone(),
                    error,
                };
            }
            Attempt::Next {
                why,
                provider_refused,
            } => {
                if let Some(e) = clear_attempt(ctx, req).await {
                    // With the row still there the next insert would refuse; the next tick retries.
                    tried.push((
                        c.clone(),
                        format!("{why}; and clearing the attempt failed: {e}"),
                    ));
                    return RentOutcome::NoneCreated { tried };
                }
                if provider_refused {
                    refused.insert(c.provider_id.as_str());
                }
                tried.push((c.clone(), why));
            }
            Attempt::NotLeader => {
                // Nothing was asked of the provider, or the lookup proved nothing was made.
                // If clearing the row fails, it stays as "may exist": the next leader looks.
                let why = match clear_attempt(ctx, req).await {
                    None => NOT_LEADER.to_string(),
                    Some(e) => format!("{NOT_LEADER}; and clearing the attempt failed: {e}"),
                };
                tried.push((c.clone(), why));
                return RentOutcome::NoneCreated { tried };
            }
        }
    }
    RentOutcome::NoneCreated { tried }
}

/// Removes the row of an attempt that made nothing, so the next candidate (or the next
/// tick) can insert again. `Some(why)` when the row stays: a database error, or a row that
/// is no longer a plain `requested` one (a teardown was ordered meanwhile). Then no other
/// candidate can insert for this node id, and the caller stops.
async fn clear_attempt(ctx: &RentCtx<'_>, req: &RentRequest<'_>) -> Option<String> {
    match nodes_db::forget_uncreated(ctx.pool, req.mm_node_id.as_str()).await {
        Ok(true) => None,
        Ok(false) => Some("the row is no longer a plain request".to_string()),
        Err(e) => Some(e.to_string()),
    }
}

/// One create call, given up on if it has not answered within `limit`. The flag says it
/// timed out, by this timer or by the adapter's own (its HTTP client gives up after 60 s,
/// long before `limit`): the provider may still be making the machine, so "nothing exists"
/// can no longer be concluded from a lookup that finds nothing. Either way the error handed
/// back is a plain `Transient` and the flag carries the difference.
async fn create_within(
    limit: Duration,
    adapter: &dyn Provider,
    spec: &InstanceSpec,
) -> (Result<InstanceHandle, ProviderError>, bool) {
    match tokio::time::timeout(limit, adapter.create(spec)).await {
        Ok(Err(ProviderError::Timeout(m))) => (Err(ProviderError::Transient(m)), true),
        Ok(r) => (r, false),
        Err(_) => (
            Err(ProviderError::Transient(format!(
                "create did not answer within {limit:?}"
            ))),
            true,
        ),
    }
}

/// The error with its text made safe to store, return and log: a provider that echoes the
/// request that failed (a test boot's cloud-init carries its boot token) must not carry a
/// secret into a status row, a request result or a log line. Adapters redact the bodies they
/// turn into text; this is the same rule at the boundary every adapter passes through.
fn redacted(e: ProviderError) -> ProviderError {
    match e {
        ProviderError::Transient(m) => ProviderError::Transient(provider_text(&m)),
        ProviderError::Permanent(m) => ProviderError::Permanent(provider_text(&m)),
        ProviderError::Capacity(m) => ProviderError::Capacity(provider_text(&m)),
        ProviderError::Quota(m) => ProviderError::Quota(provider_text(&m)),
        ProviderError::Timeout(m) => ProviderError::Timeout(provider_text(&m)),
    }
}

/// How long to wait before the lookup that follows the `retries`-th failed create: that
/// retry's backoff step, or the last one once the steps run out. The final lookup is the one
/// that decides nothing exists, so it trails the failure as well.
fn wait_before_lookup(backoff: &[Duration], retries: usize) -> Duration {
    backoff
        .get(retries)
        .or(backoff.last())
        .copied()
        .unwrap_or(Duration::ZERO)
}

async fn attempt(
    ctx: &RentCtx<'_>,
    req: &RentRequest<'_>,
    c: &Candidate,
    adapter: &dyn Provider,
    create_timeout: Duration,
) -> Attempt {
    let spec = InstanceSpec {
        mm_node_id: req.mm_node_id.clone(),
        flavor: NodeFlavor::Transcode,
        region: c.zone.clone(),
        size: c.size.clone(),
        user_data: req.user_data.to_string(),
    };
    // At 0 before the first create in this zone, so the alerts can see its first outcome.
    crate::metrics::register_create_series(&c.provider_id, &c.zone);
    let mut retries = 0usize;
    loop {
        // Before every create call, retries included: a retry can follow a long backoff.
        if !ctx.leader.still_leader().await {
            return Attempt::NotLeader;
        }
        let started = Instant::now();
        let (result, timed_out) = create_within(create_timeout, adapter, &spec).await;
        // Everything below stores, returns or logs the provider's words, so they are made safe
        // once, here.
        let result = result.map_err(redacted);
        let now = Utc::now();
        match result {
            Ok(h) => {
                crate::metrics::count_create(&c.provider_id, &c.zone, "ok");
                return match nodes_db::mark_created(ctx.pool, req.mm_node_id.as_str(), &h).await {
                    Ok(true) => Attempt::Created(h.provider_id, started.elapsed().as_secs_f64()),
                    // The row already holds a handle (or is gone), so this machine is
                    // recorded nowhere. It carries its node tag; only the orphan sweep
                    // reaps a handle no row knows.
                    Ok(false) => Attempt::MayExist(format!(
                        "created {} but its row would not take the handle",
                        h.provider_id
                    )),
                    // It exists and carries its node tag; the next tick finds and destroys it.
                    Err(e) => Attempt::MayExist(format!(
                        "created {} but recording it failed: {e}",
                        h.provider_id
                    )),
                };
            }
            Err(ProviderError::Capacity(m)) => {
                crate::metrics::count_create(&c.provider_id, &c.zone, "capacity");
                hold(
                    ctx,
                    c,
                    now + chrono::Duration::seconds(ctx.cooldown_secs),
                    "capacity",
                )
                .await;
                return Attempt::Next {
                    why: format!("no capacity: {m}"),
                    provider_refused: false,
                };
            }
            Err(ProviderError::Quota(m)) => {
                crate::metrics::count_create(&c.provider_id, &c.zone, "quota");
                hold(
                    ctx,
                    c,
                    now + chrono::Duration::seconds(QUOTA_HOLD_SECS),
                    "quota",
                )
                .await;
                return Attempt::Next {
                    why: format!("quota: {m}"),
                    provider_refused: false,
                };
            }
            Err(ProviderError::Permanent(m)) => {
                crate::metrics::count_create(&c.provider_id, &c.zone, "permanent");
                if let Err(e) = pdb::flag_needs_you(ctx.pool, &c.provider_id, &m).await {
                    tracing::warn!(provider = %c.provider_id, error = %e, "could not flag the provider");
                }
                // Skipped until a check passes: its other candidates are not tried.
                return Attempt::Next {
                    why: format!("refused: {m}"),
                    provider_refused: true,
                };
            }
            // `create_within` has already turned an adapter's `Timeout` into a `Transient` with
            // `timed_out` set, so both reach here as one case.
            Err(ProviderError::Transient(m) | ProviderError::Timeout(m)) => {
                crate::metrics::count_create(&c.provider_id, &c.zone, "transient");
                // Wait first, so the lookup trails the failed create.
                tokio::time::sleep(wait_before_lookup(ctx.backoff, retries)).await;
                match adapter.find(req.mm_node_id).await {
                    Ok(Some(h)) => return destroy_half_made(ctx, req, adapter, h, &m).await,
                    // A timed-out create is never forgotten, and never retried beside the
                    // call that may still land: the zone is held, the row stays `requested`
                    // without a handle, and the next tick's lookup runs after the provider
                    // has settled.
                    Ok(None) if timed_out => {
                        hold(
                            ctx,
                            c,
                            now + chrono::Duration::seconds(ctx.cooldown_secs),
                            "capacity",
                        )
                        .await;
                        return Attempt::MayExist(format!(
                            "{m}; no machine found yet, so it may still be coming"
                        ));
                    }
                    Ok(None) if retries < ctx.backoff.len() => retries += 1,
                    Ok(None) => {
                        hold(
                            ctx,
                            c,
                            now + chrono::Duration::seconds(ctx.cooldown_secs),
                            "capacity",
                        )
                        .await;
                        return Attempt::Next {
                            why: format!("kept failing ({m}); treated as no capacity"),
                            provider_refused: false,
                        };
                    }
                    Err(e) => {
                        // After a timeout the zone is held whatever the lookup says: it just
                        // failed to answer a create.
                        if timed_out {
                            hold(
                                ctx,
                                c,
                                now + chrono::Duration::seconds(ctx.cooldown_secs),
                                "capacity",
                            )
                            .await;
                        }
                        return Attempt::MayExist(format!(
                            "create failed ({m}) and the lookup failed ({})",
                            provider_text(&e.to_string())
                        ));
                    }
                }
            }
        }
    }
}

async fn hold(ctx: &RentCtx<'_>, c: &Candidate, until: DateTime<Utc>, reason: &str) {
    if let Err(e) =
        placement_db::set_cooldown(ctx.pool, &c.provider_id, &c.zone, until, reason).await
    {
        tracing::warn!(provider = %c.provider_id, zone = %c.zone, error = %e, "could not hold the zone");
    }
}

async fn destroy_half_made(
    ctx: &RentCtx<'_>,
    req: &RentRequest<'_>,
    adapter: &dyn Provider,
    h: InstanceHandle,
    cause: &str,
) -> Attempt {
    tracing::warn!(node = %req.mm_node_id, provider_id = %h.provider_id, "a failed create left a machine; destroying it");
    let target = TeardownTarget {
        mm_node_id: req.mm_node_id.clone(),
        ownership: Ownership::Rented,
        flavor: NodeFlavor::Transcode,
        provider_id: Some(h.provider_id.clone()),
    };
    // Order the teardown BEFORE recording the handle. If the order fails the row stays
    // `requested` without a handle, which the may-exist pass settles by looking the machine
    // up; recording first would leave a half-made machine looking like a healthy booting node
    // (it bills to its deadline and could be adopted).
    if let Err(e) = ctx.store.order_teardown(&target).await {
        return Attempt::MayExist(format!(
            "create failed ({cause}); a half-made machine {} exists and ordering its destroy failed: {e}",
            h.provider_id
        ));
    }
    // The row is `destroying` now, and mark_created keeps it so. The handle lets a destroy
    // that fails be retried from the row. Safe to ignore a failure: complete_teardown falls
    // back to the target's handle when the row has none.
    let _ = nodes_db::mark_created(ctx.pool, req.mm_node_id.as_str(), &h).await;
    match ctx.store.complete_teardown(adapter, &target).await {
        Ok(()) => Attempt::Abandoned(format!(
            "create failed ({cause}); the half-made machine was destroyed"
        )),
        Err(e) => Attempt::Abandoned(format!(
            "create failed ({cause}); the half-made machine is being destroyed ({})",
            provider_text(&e.to_string())
        )),
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;

    struct Slow;

    #[async_trait]
    impl Provider for Slow {
        fn name(&self) -> &'static str {
            "slow"
        }
        async fn create(&self, _: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Err(ProviderError::Permanent("never reached".into()))
        }
        async fn destroy(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
            Ok(vec![])
        }
    }

    struct Refuses;

    #[async_trait]
    impl Provider for Refuses {
        fn name(&self) -> &'static str {
            "refuses"
        }
        async fn create(&self, _: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
            Err(ProviderError::Transient("503".into()))
        }
        async fn destroy(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
            Ok(vec![])
        }
    }

    struct ClientGaveUp;

    #[async_trait]
    impl Provider for ClientGaveUp {
        fn name(&self) -> &'static str {
            "client-gave-up"
        }
        async fn create(&self, _: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
            Err(ProviderError::Timeout(
                "create request failed: timed out".into(),
            ))
        }
        async fn destroy(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
            Ok(vec![])
        }
    }

    fn spec() -> InstanceSpec {
        InstanceSpec {
            mm_node_id: NodeId::new("tb-1"),
            flavor: NodeFlavor::Transcode,
            region: "z-a".into(),
            size: "GPU-S".into(),
            user_data: String::new(),
        }
    }

    #[tokio::test]
    async fn a_create_that_does_not_answer_in_time_is_a_transient_failure_that_says_it_timed_out() {
        let (result, timed_out) = create_within(Duration::from_millis(30), &Slow, &spec()).await;
        assert!(timed_out);
        match result {
            Err(ProviderError::Transient(m)) => {
                assert!(m.starts_with("create did not answer within"), "{m}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// The adapter's HTTP client gives up after 60 s, long before the 600 s timer: that is the
    /// same unknown outcome, and must carry the same flag.
    #[tokio::test]
    async fn a_timeout_the_adapter_reports_is_a_timeout_here_too() {
        let (result, timed_out) =
            create_within(Duration::from_secs(5), &ClientGaveUp, &spec()).await;
        assert!(timed_out);
        match result {
            Err(ProviderError::Transient(m)) => assert!(m.contains("timed out"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_create_that_answers_is_not_a_timeout_even_when_it_fails() {
        let (result, timed_out) = create_within(Duration::from_secs(5), &Refuses, &spec()).await;
        assert!(!timed_out);
        assert!(matches!(result, Err(ProviderError::Transient(_))));
    }

    #[test]
    fn the_lookup_waits_out_the_step_for_its_retry_and_the_last_step_after_that() {
        let b = [
            Duration::from_secs(1),
            Duration::from_secs(4),
            Duration::from_secs(16),
        ];
        let waits: Vec<u64> = (0..5)
            .map(|r| wait_before_lookup(&b, r).as_secs())
            .collect();
        assert_eq!(waits, vec![1, 4, 16, 16, 16]);
        assert_eq!(wait_before_lookup(&[], 0), Duration::ZERO);
    }
}
