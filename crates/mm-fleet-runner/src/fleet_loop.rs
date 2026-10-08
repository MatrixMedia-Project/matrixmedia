//! The fleet loop (spec §6.2 "Reconcile and rent" and "Sweepers", §6.3 Test boot): every
//! provider call the fleet makes, in one sequential pass, so no two steps race over a node.
//!
//! A tick, in order:
//! 1. `off` drains first: every rented node's teardown is ordered, queued test boots refused.
//! 2. The desired rows of test boots that can no longer run are removed.
//! 3. Creates whose outcome is unknown are resolved (found → destroyed; not found → forgotten).
//! 4. One queued test boot is claimed; pending desired rows are rented (≤ 5 creates).
//! 5. Test boots advance: a report, or ten minutes without one, orders the teardown.
//! 6. Every ordered teardown is completed; finished test boots are confirmed gone.
//! 7. Sweepers, tfvars and gauges (part B).
//!
//! The leader lock is asked at the top of every step and before every provider call. A runner
//! that finds it has lost the lock returns [`FleetError::LostLeadership`] at once and writes
//! nothing more: whatever it left is recorded, and the next leader's tick settles it.
//!
//! The test boot's single-use token is minted, hashed and stored in [`rent_test_boot`] and
//! nowhere else. Its plaintext lives only in the cloud-init handed to the provider; it never
//! reaches a request result, a progress patch, an audit row, a node row or a log line, and any
//! text that came back from a provider is scrubbed of it before it is recorded.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mm_core::config::FleetMode;
use mm_core::fleet::{NodeFlavor, NodeId, NodeState, Ownership};
use mm_fleet::adapters::{AdapterSource, ImageFor, RoutedProvider};
use mm_fleet::desired::{DesiredStore, StoreError, TeardownTarget};
use mm_fleet::nodes_db::{self, PendingDesired};
use mm_fleet::placement::{self, CREATES_PER_TICK, Limits, PlacementRequest, PlacementStrategy};
use mm_fleet::placement_db;
use mm_fleet::providers_db as pdb;
use mm_fleet::rent::{self, RentCtx, RentOutcome, RentRequest};
use mm_fleet::requests_db::{self as rq, RequestRow};
use mm_fleet::roles::{Backend, Purpose, Role};
use mm_fleet::runner_settings::{self, FleetSnapshot};
use mm_fleet::test_boot::{self, BOOT_WAIT_SECS};
use mm_fleet::test_boot_db;
use mm_fleet::tfvars::TfvarsWriter;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::leader::LeaderCheck;

pub const FLEET_TICK_SECS: u64 = 10;
/// Every 30 ticks (5 minutes) the orphan sweeper runs.
pub const ORPHAN_EVERY_TICKS: u64 = 30;
/// Who the runner's own audit rows name.
const RUNNER_ACTOR: &str = "mm-fleet-runner";
/// What a secret is replaced with in text that may have echoed it.
const REDACTED: &str = "[redacted]";

pub struct FleetCtx {
    pub pool: PgPool,
    pub store: DesiredStore,
    pub adapters: Arc<dyn AdapterSource>,
    pub strategy: Arc<dyn PlacementStrategy>,
    pub leader: Arc<dyn LeaderCheck>,
    pub tfvars: Option<TfvarsWriter>,
    pub backoff: Vec<Duration>,
}

#[derive(Debug, Default, PartialEq)]
pub struct FleetReport {
    pub drained: Vec<String>,
    /// Test boots whose desired row was removed because their request can no longer run.
    pub cleaned: Vec<String>,
    pub resolved: Vec<(String, &'static str)>,
    pub claimed: Vec<String>,
    pub created: Vec<String>,
    pub not_created: Vec<(String, String)>,
    pub ordered: Vec<String>,
    pub destroyed: Vec<String>,
    pub destroy_failed: Vec<(String, String)>,
    pub finished: Vec<(String, bool)>,
    pub deadline_reaped: Vec<String>,
    pub orphans: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    #[error("this runner lost the leader lock; it stops so that only the leader acts")]
    LostLeadership,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Store(String),
}

fn store_err(e: StoreError) -> FleetError {
    match e {
        StoreError::Db(d) => FleetError::Db(d),
        other => FleetError::Store(other.to_string()),
    }
}

async fn ensure_leader(ctx: &FleetCtx) -> Result<(), FleetError> {
    if ctx.leader.still_leader().await {
        Ok(())
    } else {
        Err(FleetError::LostLeadership)
    }
}

fn target(id: &str, provider_id: Option<String>) -> TeardownTarget {
    TeardownTarget {
        mm_node_id: NodeId::new(id),
        ownership: Ownership::Rented,
        flavor: NodeFlavor::Transcode,
        provider_id,
    }
}

/// `text` with every one of `secrets` replaced. For text that came back from a provider and is
/// about to be recorded: a provider that echoes the cloud-init in an error must not carry the
/// boot token into a request result, an audit row or a log line.
fn scrubbed(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|s| !s.is_empty())
        .fold(text.to_string(), |t, s| t.replace(s, REDACTED))
}

/// The phase a request's result last recorded.
fn phase_of(r: &RequestRow) -> Option<&str> {
    r.result.as_ref()?.get("phase")?.as_str()
}

pub async fn fleet_tick(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
    sweep_orphans_now: bool,
) -> Result<FleetReport, FleetError> {
    ensure_leader(ctx).await?;
    let snap = runner_settings::read(&ctx.pool).await?;
    let mut report = FleetReport::default();
    if snap.mode == FleetMode::Off {
        drain(ctx, &mut report).await?;
    }
    clean_dead_test_boots(ctx, &mut report).await?;
    resolve_may_exist(ctx, &mut report).await?;
    if snap.mode != FleetMode::Off {
        claim_test_boot(ctx, &mut report).await?;
        rent_pending(ctx, &snap, now, &mut report).await?;
    }
    advance_test_boots(ctx, now, &mut report).await?;
    complete_teardowns(ctx, &mut report).await?;
    finish_test_boots(ctx, now, &mut report).await?;
    // Part B (Task 21): sweepers, tfvars, gauges.
    let _ = sweep_orphans_now;
    Ok(report)
}

/// Orders one teardown. A failure belongs to that node, not to the tick: it is reported and
/// logged, and the next tick orders it again. `true` when the order is in place.
async fn order(ctx: &FleetCtx, report: &mut FleetReport, t: &TeardownTarget) -> bool {
    match ctx.store.order_teardown(t).await {
        Ok(_) => true,
        Err(e) => {
            tracing::error!(node = %t.mm_node_id, error = %e, "ordering a teardown failed; it is tried again next tick");
            report.skipped.push((
                t.mm_node_id.as_str().to_string(),
                format!("ordering its teardown failed: {e}"),
            ));
            false
        }
    }
}

/// `off` (spec §6.5): order the teardown of every rented node and every test boot not yet
/// created, and refuse queued test boots. The destroys complete later in this tick.
async fn drain(ctx: &FleetCtx, report: &mut FleetReport) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    for n in ctx.store.load_nodes().await.map_err(store_err)? {
        if n.is_reapable_now()
            && n.state != NodeState::Destroying
            && order(ctx, report, &n.teardown_target()).await
        {
            report.drained.push(n.mm_node_id.as_str().to_string());
        }
    }
    for d in nodes_db::pending_desired(&ctx.pool).await? {
        if d.purpose == Purpose::TestBoot.as_str() {
            order(ctx, report, &target(&d.mm_node_id, None)).await;
        }
    }
    rq::fail_queued(
        &ctx.pool,
        "test_boot",
        "fleet.mode is off: nothing may be rented",
    )
    .await?;
    Ok(())
}

/// A test boot whose request was refused while queued (`off`), expired unclaimed, finished, or
/// never existed leaves its pinned desired row behind, and that row counts toward the
/// fleet-wide GPU cap until it is removed (it is a slot spoken for, with no machine behind it).
/// Nothing rents it: only a claimed boot is created. So the row goes, with any token stored
/// for it. A queued or running request still owns its row.
async fn clean_dead_test_boots(ctx: &FleetCtx, report: &mut FleetReport) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    for d in nodes_db::pending_desired(&ctx.pool).await? {
        if d.purpose != Purpose::TestBoot.as_str() {
            continue;
        }
        let owned = match test_boot::request_id_for(&d.mm_node_id) {
            Some(rid) => rq::get(&ctx.pool, &rid)
                .await?
                .is_some_and(|r| matches!(r.state.as_str(), "queued" | "running")),
            None => false,
        };
        if owned {
            continue;
        }
        if order(ctx, report, &target(&d.mm_node_id, None)).await {
            test_boot_db::drop_token(&ctx.pool, &NodeId::new(&d.mm_node_id)).await?;
            tracing::info!(node = %d.mm_node_id, "removed the desired row of a test boot that can no longer run");
            report.cleaned.push(d.mm_node_id);
        }
    }
    Ok(())
}

/// A create that timed out or lost its answer: look for the machine by its node tag. Found →
/// it is half made, so its teardown is ordered and only then its handle recorded (never
/// adopted). Not found → forget the row so the create can be tried again. Lookup failed → try
/// again next tick.
///
/// The order comes first: recording a handle on a node whose desired row still stands would
/// make a half-made machine look like a healthy booting node that bills to its deadline. If the
/// order fails the node stays `requested` with no handle, and the next tick looks again.
async fn resolve_may_exist(ctx: &FleetCtx, report: &mut FleetReport) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    for n in nodes_db::may_exist(&ctx.pool).await? {
        let (Some(pref), Some(zone)) = (n.provider_ref.as_deref(), n.provider_zone.as_deref())
        else {
            report.skipped.push((
                n.mm_node_id.clone(),
                "no provider recorded for a create of unknown outcome".into(),
            ));
            continue;
        };
        let adapter = match ctx.adapters.adapter(pref, zone, ImageFor::Teardown).await {
            Ok(a) => a,
            Err(why) => {
                report.skipped.push((n.mm_node_id.clone(), why));
                continue;
            }
        };
        ensure_leader(ctx).await?;
        match adapter.find(&NodeId::new(&n.mm_node_id)).await {
            Ok(Some(h)) => {
                let t = target(&n.mm_node_id, Some(h.provider_id.clone()));
                if !order(ctx, report, &t).await {
                    continue;
                }
                // `destroying` now, and mark_created keeps it so. A failure here is safe to
                // pass over: the destroy pass looks the machine up again by its tag.
                if let Err(e) = nodes_db::mark_created(&ctx.pool, &n.mm_node_id, &h).await {
                    tracing::warn!(node = %n.mm_node_id, error = %e, "could not record the found machine's handle; the destroy pass looks it up");
                }
                report.resolved.push((n.mm_node_id, "found_destroying"));
            }
            Ok(None) => {
                if nodes_db::forget_uncreated(&ctx.pool, &n.mm_node_id).await? {
                    report.resolved.push((n.mm_node_id, "not_found"));
                } else {
                    report
                        .skipped
                        .push((n.mm_node_id, "its row changed while the lookup ran".into()));
                }
            }
            Err(e) => report
                .skipped
                .push((n.mm_node_id, format!("still unknown: {e}"))),
        }
    }
    Ok(())
}

/// One test boot at a time (spec §6.3): claim the oldest queued one only when none runs.
async fn claim_test_boot(ctx: &FleetCtx, report: &mut FleetReport) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    if !rq::running(&ctx.pool, "test_boot").await?.is_empty() {
        return Ok(());
    }
    if let Some(r) = rq::claim_next(&ctx.pool, "test_boot").await? {
        let node = test_boot::node_id_for(&r.id);
        rq::progress(
            &ctx.pool,
            &r.id,
            json!({"phase": "creating", "node_id": node.as_str()}),
        )
        .await?;
        report.claimed.push(r.id);
    }
    Ok(())
}

/// Rents what is pending, oldest first (`pending_desired` orders by when it was wanted), until
/// `CREATES_PER_TICK` attempts have been made.
///
/// The budget bounds the rentals that attempted a machine, and each makes at most one. The
/// provider API calls one tick can make are bounded by candidates × (1 + backoff retries) for
/// each of those, and a create that timed out sends no retry: it is looked up, never sent
/// again, in the same call. The budget's job is to bound machines and runaway ticks; a create
/// that fails outright costs nothing.
async fn rent_pending(
    ctx: &FleetCtx,
    snap: &FleetSnapshot,
    now: DateTime<Utc>,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    let running = rq::running(&ctx.pool, "test_boot").await?;
    let mut budget = CREATES_PER_TICK;
    for d in nodes_db::pending_desired(&ctx.pool).await? {
        if budget == 0 {
            report.skipped.push((
                d.mm_node_id.clone(),
                "the create budget for this tick is used up".into(),
            ));
            continue;
        }
        let attempted = match Purpose::parse(&d.purpose) {
            Some(Purpose::TestBoot) => {
                // Only a claimed test boot is created; a queued one waits for its claim and an
                // expired one is never run.
                match running
                    .iter()
                    .find(|r| test_boot::node_id_for(&r.id).as_str() == d.mm_node_id)
                {
                    Some(req) => rent_test_boot(ctx, snap, now, &d, req, report).await?,
                    None => false,
                }
            }
            Some(Purpose::Broadcast) => rent_broadcast(ctx, snap, now, &d, report).await?,
            None => false,
        };
        if attempted {
            budget -= 1;
        }
    }
    Ok(())
}

/// Rents the one machine of a claimed test boot, on the provider and zone the operator pinned.
/// `true` when a machine was attempted (it counts against the tick's budget).
async fn rent_test_boot(
    ctx: &FleetCtx,
    snap: &FleetSnapshot,
    now: DateTime<Utc>,
    d: &PendingDesired,
    req: &RequestRow,
    report: &mut FleetReport,
) -> Result<bool, FleetError> {
    let node = NodeId::new(&d.mm_node_id);
    let (Some(pid), Some(zone)) = (d.pinned_provider_id.as_deref(), d.pinned_zone.as_deref())
    else {
        fail_test_boot(ctx, req, &node, "the test boot names no provider or zone").await?;
        return Ok(false);
    };
    let Some(report_url) = req.params.get("report_url").and_then(Value::as_str) else {
        fail_test_boot(ctx, req, &node, "the request carries no report URL").await?;
        return Ok(false);
    };
    let deadline = d
        .destroy_deadline
        .unwrap_or(now + chrono::Duration::seconds(test_boot::DEADLINE_SECS));
    // A machine made after its deadline would only be destroyed by the deadline sweeper: pure
    // cost. This is what ends a boot whose creates keep coming back unconfirmed.
    if deadline <= now {
        fail_test_boot(
            ctx,
            req,
            &node,
            "its deadline passed before a machine was made",
        )
        .await?;
        return Ok(false);
    }
    // The API checked all this when it queued the boot, outside the locks a create takes: the
    // runner re-applies the rules to what is true now.
    let (facts, live) = placement_db::load_facts(&ctx.pool).await?;
    let preq = PlacementRequest {
        role: Role::Transcode,
        region: d.region.clone(),
        purpose: Purpose::TestBoot,
        backend: Backend::Api,
        now,
    };
    let limits = Limits {
        max_gpu_nodes: snap.max_gpu_nodes,
        gpu_nodes_live: live,
    };
    let candidate = match placement::pinned(&facts, pid, zone, &preq, &limits) {
        Ok(c) => c,
        Err(skip) => {
            fail_test_boot(ctx, req, &node, &format!("not eligible: {}", skip.as_str())).await?;
            return Ok(false);
        }
    };

    // The token: only its hash is stored; the plaintext goes into the cloud-init below and
    // nowhere else. Text a provider sends back is scrubbed of both before it is recorded.
    let (token, hash) = test_boot::mint_token();
    let hash_hex = hex::encode(&hash);
    let secrets = [token.as_str(), hash_hex.as_str()];
    test_boot_db::store_token(&ctx.pool, &node, &hash, deadline).await?;
    let user_data = test_boot::probe_cloud_init(report_url, &token);

    ensure_leader(ctx).await?;
    let rent_ctx = RentCtx {
        pool: &ctx.pool,
        store: &ctx.store,
        adapters: ctx.adapters.as_ref(),
        leader: ctx.leader.as_ref(),
        global_cap: snap.max_gpu_nodes,
        cooldown_secs: snap.capacity_cooldown_secs,
        backoff: &ctx.backoff,
    };
    let outcome = rent::rent_one(
        &rent_ctx,
        &RentRequest {
            mm_node_id: &node,
            purpose: Purpose::TestBoot,
            destroy_deadline: deadline,
            created_by: d.created_by.as_deref(),
            user_data: &user_data,
            image: ImageFor::TestBoot,
        },
        std::slice::from_ref(&candidate),
    )
    .await;
    // A runner that lost the lead mid-rent writes no failure for a request it no longer owns:
    // the request stays running, the next leader's tick rents it.
    if outcome.lost_leadership() {
        return Err(FleetError::LostLeadership);
    }
    match outcome {
        RentOutcome::Created {
            candidate,
            provider_id,
            create_secs,
        } => {
            rq::progress(
                &ctx.pool,
                &req.id,
                json!({"phase": "booting", "provider_id": provider_id,
                    "zone": candidate.zone, "size": candidate.size, "create_secs": create_secs}),
            )
            .await?;
            report.created.push(d.mm_node_id.clone());
        }
        RentOutcome::MayExist { error, .. } => {
            let error = scrubbed(&error, &secrets);
            rq::progress(
                &ctx.pool,
                &req.id,
                json!({"phase": "create_unconfirmed", "error": error}),
            )
            .await?;
            report.not_created.push((d.mm_node_id.clone(), error));
        }
        RentOutcome::Abandoned { error, .. } => {
            let error = scrubbed(&error, &secrets);
            fail_test_boot(ctx, req, &node, &error).await?;
            report.not_created.push((d.mm_node_id.clone(), error));
        }
        RentOutcome::NoneCreated { tried } => {
            let why = tried
                .iter()
                .map(|(_, w)| w.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            let why = scrubbed(&why, &secrets);
            fail_test_boot(ctx, req, &node, &why).await?;
            report.not_created.push((d.mm_node_id.clone(), why));
        }
    }
    Ok(true)
}

/// Part B (Task 21) rents broadcast rows; until then nothing is attempted.
async fn rent_broadcast(
    _ctx: &FleetCtx,
    _snap: &FleetSnapshot,
    _now: DateTime<Utc>,
    _d: &PendingDesired,
    _report: &mut FleetReport,
) -> Result<bool, FleetError> {
    Ok(false)
}

/// Ends a test boot that has no machine (any more): its desired row and token go, the
/// request fails, the audit says so. `why` is recorded and logged as given, so the caller has
/// already scrubbed anything a provider sent back.
async fn fail_test_boot(
    ctx: &FleetCtx,
    req: &RequestRow,
    node: &NodeId,
    why: &str,
) -> Result<(), FleetError> {
    ctx.store
        .order_teardown(&target(node.as_str(), None))
        .await
        .map_err(store_err)?;
    test_boot_db::drop_token(&ctx.pool, node).await?;
    rq::finish(
        &ctx.pool,
        &req.id,
        false,
        json!({"phase": "failed", "error": why}),
    )
    .await?;
    pdb::append_audit(
        &ctx.pool,
        &pdb::AuditEntry {
            actor: RUNNER_ACTOR,
            action: "test_boot",
            target: &req.provider_id,
            reason: req.reason.as_deref(),
            detail: json!({"request_id": req.id, "node_id": node.as_str(),
                "requested_by": req.requested_by, "outcome": "failed", "error": why}),
        },
    )
    .await?;
    tracing::warn!(request = %req.id, node = %node, why, "test boot failed before it booted");
    Ok(())
}

/// A report, or ten minutes without one, orders the machine's teardown. A test boot whose
/// machine never existed and is no longer pending (withdrawn by `off`) is ended. A machine
/// already `destroying` (an operator's Release, or `off`) is the end of the boot too: its
/// destroy completes below and the request finishes with the facts it has.
async fn advance_test_boots(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    let pending: HashSet<String> = nodes_db::pending_desired(&ctx.pool)
        .await?
        .into_iter()
        .map(|d| d.mm_node_id)
        .collect();
    for r in rq::running(&ctx.pool, "test_boot").await? {
        let node = test_boot::node_id_for(&r.id);
        let Some(n) = nodes_db::api_node(&ctx.pool, node.as_str()).await? else {
            if !pending.contains(node.as_str()) {
                fail_test_boot(ctx, &r, &node, "withdrawn before a machine existed").await?;
            }
            continue;
        };
        match n.state.as_str() {
            "booting" | "healthy" => {}
            "destroying" => {
                if phase_of(&r) != Some("destroying") {
                    rq::progress(&ctx.pool, &r.id, json!({"phase": "destroying"})).await?;
                }
                continue;
            }
            _ => continue,
        }
        let started = n.billing_started_at.unwrap_or(now);
        let why = if n.boot_report.is_some() {
            Some("report received")
        } else if now - started > chrono::Duration::seconds(BOOT_WAIT_SECS) {
            Some("no report within 10 minutes")
        } else {
            None
        };
        match why {
            Some(why) => {
                if order(ctx, report, &target(&n.mm_node_id, n.provider_id.clone())).await {
                    rq::progress(
                        &ctx.pool,
                        &r.id,
                        json!({"phase": "destroying", "teardown_reason": why}),
                    )
                    .await?;
                    report.ordered.push(n.mm_node_id);
                }
            }
            None => {
                if phase_of(&r) != Some("booting") {
                    rq::progress(&ctx.pool, &r.id, json!({"phase": "booting"})).await?;
                }
            }
        }
    }
    Ok(())
}

/// Completes every ordered teardown of a machine the runner created (mm-core orders them;
/// so do `off`, Release and this loop). A row without a handle is looked up first: its create
/// may have made a machine.
async fn complete_teardowns(ctx: &FleetCtx, report: &mut FleetReport) -> Result<(), FleetError> {
    for n in nodes_db::api_nodes_live(&ctx.pool)
        .await?
        .into_iter()
        .filter(|n| n.state == "destroying")
    {
        ensure_leader(ctx).await?;
        let t = target(&n.mm_node_id, n.provider_id.clone());
        let done: Result<(), String> = match (n.provider_ref.as_deref(), n.provider_zone.as_deref())
        {
            (Some(pref), Some(zone)) => {
                match ctx.adapters.adapter(pref, zone, ImageFor::Teardown).await {
                    Ok(a) if n.provider_id.is_some() => ctx
                        .store
                        .complete_teardown(a.as_ref(), &t)
                        .await
                        .map_err(|e| e.to_string()),
                    Ok(a) => match a.find(&NodeId::new(&n.mm_node_id)).await {
                        Ok(Some(h)) => {
                            nodes_db::mark_created(&ctx.pool, &n.mm_node_id, &h).await?;
                            ctx.store
                                .complete_teardown(
                                    a.as_ref(),
                                    &target(&n.mm_node_id, Some(h.provider_id)),
                                )
                                .await
                                .map_err(|e| e.to_string())
                        }
                        Ok(None) => ctx
                            .store
                            .complete_teardown(a.as_ref(), &t)
                            .await
                            .map_err(|e| e.to_string()),
                        Err(e) => Err(format!(
                            "cannot tell whether its create made a machine: {e}"
                        )),
                    },
                    Err(why) => Err(why),
                }
            }
            // Never reached a provider: nothing to call.
            _ if n.provider_id.is_none() => ctx
                .store
                .complete_teardown(&RoutedProvider::empty(), &t)
                .await
                .map_err(|e| e.to_string()),
            _ => Err("no provider is recorded for this machine".to_string()),
        };
        match done {
            Ok(()) => report.destroyed.push(n.mm_node_id),
            Err(why) => {
                tracing::error!(node = %n.mm_node_id, error = %why, "destroy failed; it stays destroying and is retried next tick");
                report.destroy_failed.push((n.mm_node_id, why));
            }
        }
    }
    Ok(())
}

async fn price_and_currency(
    pool: &PgPool,
    provider: Option<&str>,
    size: Option<&str>,
) -> Result<(Option<f64>, Option<&'static str>), FleetError> {
    let Some(p) = (match provider {
        Some(id) => pdb::get(pool, id).await?,
        None => None,
    }) else {
        return Ok((None, None));
    };
    let price = p
        .status
        .as_ref()
        .zip(size)
        .and_then(|(s, size)| s.prices.get(size))
        .and_then(Value::as_f64);
    Ok((price, Some(pdb::price_currency(&p.row.kind))))
}

/// A gone test-boot machine is confirmed absent at the provider before the request is done:
/// still listed → destroyed again and checked next tick. Then the result, the audit (an
/// operator cost, never a wallet charge) and the token's removal.
///
/// The billed minutes are what the machine ran, never capped to the 15-minute ceiling the
/// dashboard shows before the run: a boot that overran is reported at what it cost.
async fn finish_test_boots(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    for r in rq::running(&ctx.pool, "test_boot").await? {
        let node = test_boot::node_id_for(&r.id);
        let Some(n) = nodes_db::api_node(&ctx.pool, node.as_str()).await? else {
            continue;
        };
        if n.state != "gone" {
            continue;
        }
        let mut confirmed = false;
        if let (Some(pid), Some(pref), Some(zone)) = (
            n.provider_id.as_deref(),
            n.provider_ref.as_deref(),
            n.provider_zone.as_deref(),
        ) {
            let adapter = match ctx.adapters.adapter(pref, zone, ImageFor::Teardown).await {
                Ok(a) => a,
                Err(why) => {
                    rq::progress(
                        &ctx.pool,
                        &r.id,
                        json!({"phase": "confirming", "error": why}),
                    )
                    .await?;
                    continue;
                }
            };
            match adapter.list().await {
                Ok(listed) if listed.iter().any(|h| h.provider_id == pid) => {
                    ensure_leader(ctx).await?;
                    tracing::error!(node = %node, provider_id = pid, "a destroyed test machine is still listed; destroying it again");
                    let again = adapter.destroy(pid).await.err().map(|e| e.to_string());
                    rq::progress(&ctx.pool, &r.id, json!({"phase": "confirming",
                        "error": again.unwrap_or_else(|| "still listed after its destroy; destroyed again".into())})).await?;
                    continue;
                }
                Ok(_) => confirmed = true,
                Err(e) => {
                    rq::progress(
                        &ctx.pool,
                        &r.id,
                        json!({"phase": "confirming", "error": e.to_string()}),
                    )
                    .await?;
                    continue;
                }
            }
        }
        let boot = n.boot_report.as_ref().and_then(test_boot::report_of);
        let (price, currency) =
            price_and_currency(&ctx.pool, n.provider_ref.as_deref(), n.size.as_deref()).await?;
        let minutes = n
            .billing_started_at
            .map(|t| test_boot::billed_minutes(t, now));
        let est = minutes.and_then(|m| test_boot::estimate_cost(price, m));
        let nvenc = boot.as_ref().map_or("no_report", |b| b.nvenc.as_str());
        let ok = confirmed && nvenc == "ok";
        let never_created = n
            .provider_id
            .is_none()
            .then_some("the create never completed");
        rq::finish(
            &ctx.pool,
            &r.id,
            ok,
            json!({
                "phase": "done", "nvenc": nvenc,
                "gpu": boot.as_ref().map(|b| b.gpu.clone()),
                "nvenc_error": boot.as_ref().and_then(|b| b.nvenc_error.clone()),
                "boot_secs": boot.as_ref().map(|b| b.uptime_secs),
                "billed_minutes": minutes, "price_per_hour": price, "currency": currency, "est_cost": est,
                "confirmed_absent": confirmed, "error": never_created,
            }),
        )
        .await?;
        test_boot_db::drop_token(&ctx.pool, &node).await?;
        pdb::append_audit(
            &ctx.pool,
            &pdb::AuditEntry {
                actor: RUNNER_ACTOR,
                action: "test_boot",
                target: &r.provider_id,
                reason: r.reason.as_deref(),
                detail: json!({"request_id": r.id, "node_id": node.as_str(), "requested_by": r.requested_by,
                    "outcome": if ok { "ok" } else { "failed" },
                    "nvenc": nvenc, "billed_minutes": minutes, "est_cost": est, "currency": currency,
                    "confirmed_absent": confirmed, "charged_to": "operator"}),
            },
        )
        .await?;
        tracing::info!(request = %r.id, node = %node, nvenc, confirmed, "test boot finished");
        report.finished.push((r.id.clone(), ok));
    }
    Ok(())
}

/// One info line per tick that did anything; quiet ticks stay quiet.
pub fn log_report(r: &FleetReport) {
    if *r == FleetReport::default() {
        return;
    }
    tracing::info!(
        drained = r.drained.len(),
        cleaned = r.cleaned.len(),
        resolved = r.resolved.len(),
        claimed = r.claimed.len(),
        created = r.created.len(),
        not_created = r.not_created.len(),
        ordered = r.ordered.len(),
        destroyed = r.destroyed.len(),
        destroy_failed = r.destroy_failed.len(),
        finished = r.finished.len(),
        deadline_reaped = r.deadline_reaped.len(),
        orphans = r.orphans.len(),
        skipped = r.skipped.len(),
        "fleet tick"
    );
}
