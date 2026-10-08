//! Renting one node (spec §5.3): try each candidate in order until one creates.
//!
//! * The node row, with its deadline, is written before the create call (FR-202).
//! * A create whose outcome is unknown is looked up by its node tag before anything is
//!   retried: provider-side names are not unique, so a blind retry can rent twice.
//! * A half-made machine is destroyed, never adopted: nothing proves it received its
//!   cloud-init or powered on.
//! * Every create call, the first and each retry, is preceded by a leadership check: a
//!   runner that lost the lock stops creating, and leaves nothing it cannot account for.

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
use crate::roles::Purpose;

/// A create that has not answered in this long is treated as "outcome unknown".
pub const CREATE_TIMEOUT: Duration = Duration::from_secs(600);
/// A zone refused for quota sits out a day, or until a successful Test connection.
pub const QUOTA_HOLD_SECS: i64 = 86_400;
/// Waits before re-trying a transient failure that the lookup proved made nothing.
pub const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
];

/// Why a candidate was not created when the runner lost the lead.
const NOT_LEADER: &str = "this runner is no longer the leader";

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
    /// The outcome is unknown and the lookup failed: the node row stays without a handle and
    /// later ticks look again. Nothing else is created meanwhile.
    MayExist { candidate: Candidate, error: String },
    /// A half-made machine was found and its destroy ordered; this node id is spent.
    Abandoned { candidate: Candidate, error: String },
    /// Every candidate refused and nothing exists. `tried` says why, per candidate.
    NoneCreated { tried: Vec<(Candidate, String)> },
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
    Next(String),
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
    let mut tried: Vec<(Candidate, String)> = Vec::new();
    for c in candidates {
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
            // Never create beside a row that exists (it may stand for a machine), and never
            // for a node whose teardown was ordered. Every other candidate would refuse the
            // same way, so none is tried.
            Err(e @ (InsertRefused::AlreadyExists | InsertRefused::NotDesired)) => {
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
                let _ = nodes_db::forget_uncreated(ctx.pool, req.mm_node_id.as_str()).await;
                tried.push((c.clone(), why));
                continue;
            }
        };
        match attempt(ctx, req, c, adapter.as_ref()).await {
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
            Attempt::Next(why) => {
                if let Err(e) = nodes_db::forget_uncreated(ctx.pool, req.mm_node_id.as_str()).await
                {
                    // With the row still there the next insert would refuse; the next tick retries.
                    tried.push((
                        c.clone(),
                        format!("{why}; and clearing the attempt failed: {e}"),
                    ));
                    return RentOutcome::NoneCreated { tried };
                }
                tried.push((c.clone(), why));
            }
            Attempt::NotLeader => {
                // Nothing was asked of the provider, or the lookup proved nothing was made.
                // If clearing the row fails, it stays as "may exist": the next leader looks.
                let why = match nodes_db::forget_uncreated(ctx.pool, req.mm_node_id.as_str()).await
                {
                    Ok(_) => NOT_LEADER.to_string(),
                    Err(e) => format!("{NOT_LEADER}; and clearing the attempt failed: {e}"),
                };
                tried.push((c.clone(), why));
                return RentOutcome::NoneCreated { tried };
            }
        }
    }
    RentOutcome::NoneCreated { tried }
}

async fn attempt(
    ctx: &RentCtx<'_>,
    req: &RentRequest<'_>,
    c: &Candidate,
    adapter: &dyn Provider,
) -> Attempt {
    let spec = InstanceSpec {
        mm_node_id: req.mm_node_id.clone(),
        flavor: NodeFlavor::Transcode,
        region: c.zone.clone(),
        size: c.size.clone(),
        user_data: req.user_data.to_string(),
    };
    let mut retries = 0usize;
    loop {
        // Before every create call, retries included: a retry can follow a long backoff.
        if !ctx.leader.still_leader().await {
            return Attempt::NotLeader;
        }
        let started = Instant::now();
        let result = match tokio::time::timeout(CREATE_TIMEOUT, adapter.create(&spec)).await {
            Ok(r) => r,
            Err(_) => Err(ProviderError::Transient(format!(
                "create did not answer within {} s",
                CREATE_TIMEOUT.as_secs()
            ))),
        };
        let now = Utc::now();
        match result {
            Ok(h) => {
                crate::metrics::count_create(&c.provider_id, &c.zone, "ok");
                return match nodes_db::mark_created(ctx.pool, req.mm_node_id.as_str(), &h).await {
                    Ok(true) => Attempt::Created(h.provider_id, started.elapsed().as_secs_f64()),
                    // The row already holds a handle, so this machine is recorded nowhere. It
                    // carries its node tag; the orphan sweep reaps a handle no row knows.
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
                return Attempt::Next(format!("no capacity: {m}"));
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
                return Attempt::Next(format!("quota: {m}"));
            }
            Err(ProviderError::Permanent(m)) => {
                crate::metrics::count_create(&c.provider_id, &c.zone, "permanent");
                if let Err(e) = pdb::flag_needs_you(ctx.pool, &c.provider_id, &m).await {
                    tracing::warn!(provider = %c.provider_id, error = %e, "could not flag the provider");
                }
                return Attempt::Next(format!("refused: {m}"));
            }
            Err(ProviderError::Transient(m)) => {
                crate::metrics::count_create(&c.provider_id, &c.zone, "transient");
                match adapter.find(req.mm_node_id).await {
                    Ok(Some(h)) => return destroy_half_made(ctx, req, adapter, h, &m).await,
                    Ok(None) if retries < ctx.backoff.len() => {
                        tokio::time::sleep(ctx.backoff[retries]).await;
                        retries += 1;
                    }
                    Ok(None) => {
                        hold(
                            ctx,
                            c,
                            now + chrono::Duration::seconds(ctx.cooldown_secs),
                            "capacity",
                        )
                        .await;
                        return Attempt::Next(format!(
                            "kept failing ({m}); treated as no capacity"
                        ));
                    }
                    Err(e) => {
                        return Attempt::MayExist(format!(
                            "create failed ({m}) and the lookup failed ({e})"
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
    // Safe to ignore: complete_teardown falls back to the target's handle when the row has none.
    let _ = nodes_db::mark_created(ctx.pool, req.mm_node_id.as_str(), &h).await;
    let target = TeardownTarget {
        mm_node_id: req.mm_node_id.clone(),
        ownership: Ownership::Rented,
        flavor: NodeFlavor::Transcode,
        provider_id: Some(h.provider_id.clone()),
    };
    if let Err(e) = ctx.store.order_teardown(&target).await {
        return Attempt::Abandoned(format!(
            "create failed ({cause}); a half-made machine exists and ordering its destroy failed: {e}"
        ));
    }
    match ctx.store.complete_teardown(adapter, &target).await {
        Ok(()) => Attempt::Abandoned(format!(
            "create failed ({cause}); the half-made machine was destroyed"
        )),
        Err(e) => Attempt::Abandoned(format!(
            "create failed ({cause}); the half-made machine is being destroyed ({e})"
        )),
    }
}
