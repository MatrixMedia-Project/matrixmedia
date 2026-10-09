//! The fleet loop (spec §6.2 "Reconcile and rent" and "Sweepers", §6.3 Test boot): every
//! provider call the fleet makes, in one sequential pass, so no two steps race over a node.
//!
//! A tick, in order:
//! 1. `off` drains first: every API-made node's teardown is ordered, queued test boots refused.
//! 2. Ordered teardowns are completed, before anything that spends money.
//! 3. The desired rows of test boots that can no longer run are removed.
//! 4. Creates whose outcome is unknown are resolved: a machine found is destroyed; nothing found
//!    waits out the settle window, and then ends the test boot (it is never sent again).
//! 5. One queued test boot is claimed; pending desired rows are rented (≤ 5 creates).
//! 6. Test boots advance: a report, ten minutes without one, or the deadline orders the teardown.
//! 7. The teardowns ordered above are completed; finished test boots are confirmed gone, by the
//!    node tag.
//! 8. Sweepers, tfvars and gauges (part B).
//!
//! The leader lock is asked at the top of every step, before every provider call and before a
//! test boot's terminal writes. A runner that finds it has lost the lock returns
//! [`FleetError::LostLeadership`] at once and writes nothing more: whatever it left is
//! recorded, and the next leader's tick settles it. That error and the tick's own top-level
//! reads are the only things that end a tick: a failure about one node or one request is
//! reported in `skipped` and the tick goes on, so one bad row never stops a destroy.
//!
//! A test boot gets exactly one create. Its request records `create_attempted` before the
//! create is sent, and nothing sends a second one: a create that timed out, or whose answer
//! was lost, may still land, and a lookup that finds nothing right after it proves nothing.
//! The boot is ended instead, once the settle window ([`SETTLE_SECS`]) has passed, and the
//! operator runs it again.
//!
//! The test boot's single-use token is minted, hashed and stored in [`rent_test_boot`] and
//! nowhere else. Its plaintext lives only in the cloud-init handed to the provider; it never
//! reaches a request result, a progress patch, an audit row, a node row or a log line, and
//! every piece of text that came back from a provider goes through
//! [`mm_fleet::redact::provider_text`] before it is recorded.

use std::collections::HashSet;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mm_core::config::FleetMode;
use mm_core::fleet::{NodeFlavor, NodeId, Ownership};
use mm_fleet::adapters::{AdapterSource, ImageFor, RoutedProvider};
use mm_fleet::desired::{DesiredStore, TeardownTarget};
use mm_fleet::nodes_db::{self, ApiNode, PendingDesired};
use mm_fleet::placement::{self, CREATES_PER_TICK, Limits, PlacementRequest, PlacementStrategy};
use mm_fleet::placement_db;
use mm_fleet::providers_db as pdb;
use mm_fleet::redact::provider_text;
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
/// How long after a create was sent its outcome is still treated as possibly in flight: longer
/// than the provider client's 60 s whole-request timeout plus two ticks. Until then an empty
/// lookup proves nothing and the row is left alone; after it, a test boot whose machine still
/// has not appeared is ended.
pub const SETTLE_SECS: i64 = 180;
/// Who the runner's own audit rows name.
const RUNNER_ACTOR: &str = "mm-fleet-runner";
/// Why a test boot whose create may have landed was ended.
const OUTCOME_STAYED_UNKNOWN: &str = "the create's outcome stayed unknown; nothing was found";

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

/// Reports a failure about one node or request and goes on: `skipped` names it and the log
/// says why. The text may hold a provider's words, so it is redacted.
fn skip(report: &mut FleetReport, id: &str, what: &str, why: impl Display) {
    let why = provider_text(&why.to_string());
    tracing::warn!(item = id, what, error = %why, "skipped; tried again next tick");
    report
        .skipped
        .push((id.to_string(), format!("{what}: {why}")));
}

/// The value of a write whose failure is the item's, not the tick's: `None` after reporting it.
fn or_skip<T, E: Display>(
    report: &mut FleetReport,
    id: &str,
    what: &str,
    r: Result<T, E>,
) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            skip(report, id, what, e);
            None
        }
    }
}

/// What an item of a step returned. Losing the lead ends the tick; any other error is the
/// item's alone.
fn item(report: &mut FleetReport, id: &str, r: Result<(), FleetError>) -> Result<(), FleetError> {
    match r {
        Err(FleetError::LostLeadership) => Err(FleetError::LostLeadership),
        Err(e) => {
            skip(report, id, "failed", e);
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

/// The phase a request's result last recorded.
fn phase_of(r: &RequestRow) -> Option<&str> {
    r.result.as_ref()?.get("phase")?.as_str()
}

/// Whether a create was already sent (or may have been) for this test boot.
fn create_attempted(r: &RequestRow) -> bool {
    r.result
        .as_ref()
        .and_then(|v| v.get("create_attempted"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// Whether a test boot's create is old enough that a machine which has not appeared by now is
/// not coming. A record without a time counts as settled: ending a boot costs nothing, while
/// waiting on one that can never be dated would only keep its row.
fn settled(r: &RequestRow, now: DateTime<Utc>) -> bool {
    r.result
        .as_ref()
        .and_then(|v| v.get("create_attempted_at"))
        .and_then(|v| serde_json::from_value::<DateTime<Utc>>(v.clone()).ok())
        .is_none_or(|at| now - at > chrono::Duration::seconds(SETTLE_SECS))
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
    // Teardowns that are owed come before anything that spends: a destroy that frees a cap
    // slot, or ends a bill, never waits behind a rental.
    let mut attempted = HashSet::new();
    complete_teardowns(ctx, &mut attempted, &mut report).await?;
    clean_dead_test_boots(ctx, &mut report).await?;
    resolve_may_exist(ctx, now, &mut report).await?;
    if snap.mode != FleetMode::Off {
        claim_test_boot(ctx, &mut report).await?;
        rent_pending(ctx, &snap, now, &mut report).await?;
    }
    advance_test_boots(ctx, now, &mut report).await?;
    // The orders made above (a report in, a deadline, a lookup that found a machine).
    complete_teardowns(ctx, &mut attempted, &mut report).await?;
    finish_test_boots(ctx, now, &mut report).await?;
    // Part B (Task 21): sweepers, tfvars, gauges.
    let _ = sweep_orphans_now;
    Ok(report)
}

/// Orders one teardown. A failure belongs to that node, not to the tick: it is reported and
/// logged, and the next tick orders it again. `true` when the order is in place.
async fn order(ctx: &FleetCtx, report: &mut FleetReport, t: &TeardownTarget) -> bool {
    or_skip(
        report,
        t.mm_node_id.as_str(),
        "ordering its teardown failed",
        ctx.store.order_teardown(t).await,
    )
    .is_some()
}

/// `off` (spec §6.5): order the teardown of every node the runner made through a provider API,
/// and of every test boot not yet created, and refuse queued test boots. The destroys complete
/// later in this tick. Nodes made by Terraform are not the runner's to touch: mm-core drains
/// those.
async fn drain(ctx: &FleetCtx, report: &mut FleetReport) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    for n in nodes_db::api_nodes_live(&ctx.pool).await? {
        if n.state != "destroying"
            && order(ctx, report, &target(&n.mm_node_id, n.provider_id.clone())).await
        {
            report.drained.push(n.mm_node_id);
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
            Some(rid) => match or_skip(
                report,
                &d.mm_node_id,
                "reading its request failed",
                rq::get(&ctx.pool, &rid).await,
            ) {
                Some(r) => r.is_some_and(|r| matches!(r.state.as_str(), "queued" | "running")),
                None => continue,
            },
            None => false,
        };
        if owned || !order(ctx, report, &target(&d.mm_node_id, None)).await {
            continue;
        }
        or_skip(
            report,
            &d.mm_node_id,
            "dropping its token failed",
            test_boot_db::drop_token(&ctx.pool, &NodeId::new(&d.mm_node_id)).await,
        );
        tracing::info!(node = %d.mm_node_id, "removed the desired row of a test boot that can no longer run");
        report.cleaned.push(d.mm_node_id);
    }
    Ok(())
}

/// A create that timed out or lost its answer: look for the machine by its node tag.
///
/// * Found: it is half made, so its teardown is ordered and only then its handle recorded
///   (never adopted). The order comes first: recording a handle on a node whose desired row
///   still stands would make a half-made machine look like a healthy booting node that bills
///   to its deadline. If the order fails the node stays `requested` with no handle, and the
///   next tick looks again.
/// * Nothing found: that proves nothing until the create has settled ([`SETTLE_SECS`] after it
///   was sent), because a create that timed out can still land. Until then the row is left
///   alone. After it, a test boot is ended (the row is closed through the destroy pass, which
///   looks once more) and never retried.
/// * Lookup failed: try again next tick.
///
/// The node table keeps no time for when a row was written, so the clock is the test boot
/// request's `create_attempted_at`. A row with no such record (no request) is not dated, so
/// it is never given up on here: the deadline sweeper owns it.
async fn resolve_may_exist(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    for n in nodes_db::may_exist(&ctx.pool).await? {
        let id = n.mm_node_id.clone();
        let r = resolve_one(ctx, now, n, report).await;
        item(report, &id, r)?;
    }
    Ok(())
}

async fn resolve_one(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
    n: ApiNode,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    let (Some(pref), Some(zone)) = (n.provider_ref.as_deref(), n.provider_zone.as_deref()) else {
        report.skipped.push((
            n.mm_node_id.clone(),
            "no provider recorded for a create of unknown outcome".into(),
        ));
        return Ok(());
    };
    let adapter = match ctx.adapters.adapter(pref, zone, ImageFor::Teardown).await {
        Ok(a) => a,
        Err(why) => {
            report.skipped.push((n.mm_node_id.clone(), why));
            return Ok(());
        }
    };
    ensure_leader(ctx).await?;
    let node = NodeId::new(&n.mm_node_id);
    match adapter.find(&node).await {
        Ok(Some(h)) => {
            let t = target(&n.mm_node_id, Some(h.provider_id.clone()));
            if !order(ctx, report, &t).await {
                return Ok(());
            }
            // `destroying` now, and mark_created keeps it so. A failure here is safe to pass
            // over: the destroy pass looks the machine up again by its tag.
            or_skip(
                report,
                &n.mm_node_id,
                "could not record the found machine's handle; the destroy pass looks it up",
                nodes_db::mark_created(&ctx.pool, &n.mm_node_id, &h).await,
            );
            report.resolved.push((n.mm_node_id, "found_destroying"));
        }
        Ok(None) => {
            let request = if n.purpose == Purpose::TestBoot.as_str() {
                match test_boot::request_id_for(&n.mm_node_id) {
                    Some(rid) => rq::get(&ctx.pool, &rid).await?,
                    None => None,
                }
            } else {
                None
            };
            match request {
                Some(req) if settled(&req, now) => {
                    fail_test_boot(ctx, report, &req, &node, OUTCOME_STAYED_UNKNOWN).await;
                    report.resolved.push((n.mm_node_id, "not_found"));
                }
                Some(_) => report.skipped.push((
                    n.mm_node_id,
                    "nothing found yet, but its create may still land; waiting".into(),
                )),
                None => report.skipped.push((
                    n.mm_node_id,
                    "nothing found, and no record of when its create was sent; waiting".into(),
                )),
            }
        }
        Err(e) => report.skipped.push((
            n.mm_node_id,
            format!("still unknown: {}", provider_text(&e.to_string())),
        )),
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
        or_skip(
            report,
            &r.id,
            "recording its claim failed",
            rq::progress(
                &ctx.pool,
                &r.id,
                json!({"phase": "creating", "node_id": node.as_str()}),
            )
            .await,
        );
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
                    // One create per test boot, ever: a second would sit beside a first that
                    // may still land. The boot ends when its settle window has passed.
                    Some(req) if create_attempted(req) => {
                        report.skipped.push((
                            d.mm_node_id.clone(),
                            "its create was already sent; it is never sent twice".into(),
                        ));
                        false
                    }
                    Some(req) => match rent_test_boot(ctx, snap, now, &d, req, report).await {
                        Ok(attempted) => attempted,
                        Err(FleetError::LostLeadership) => return Err(FleetError::LostLeadership),
                        Err(e) => {
                            skip(report, &d.mm_node_id, "renting failed", e);
                            false
                        }
                    },
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
///
/// Before the create is sent the request records that it was (`create_attempted`, with when).
/// That record is what stops a second create. Everything after the create is per-item: once a
/// machine may exist, no failed write may hide that from the caller.
async fn rent_test_boot(
    ctx: &FleetCtx,
    snap: &FleetSnapshot,
    now: DateTime<Utc>,
    d: &PendingDesired,
    req: &RequestRow,
    report: &mut FleetReport,
) -> Result<bool, FleetError> {
    ensure_leader(ctx).await?;
    let node = NodeId::new(&d.mm_node_id);
    let (Some(pid), Some(zone)) = (d.pinned_provider_id.as_deref(), d.pinned_zone.as_deref())
    else {
        fail_test_boot(
            ctx,
            report,
            req,
            &node,
            "the test boot names no provider or zone",
        )
        .await;
        return Ok(false);
    };
    let Some(report_url) = req.params.get("report_url").and_then(Value::as_str) else {
        fail_test_boot(ctx, report, req, &node, "the request carries no report URL").await;
        return Ok(false);
    };
    let deadline = d
        .destroy_deadline
        .unwrap_or(now + chrono::Duration::seconds(test_boot::DEADLINE_SECS));
    // A machine made after its deadline would only be destroyed by the deadline sweeper: pure
    // cost.
    if deadline <= now {
        fail_test_boot(
            ctx,
            report,
            req,
            &node,
            "its deadline passed before a machine was made",
        )
        .await;
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
        Err(refusal) => {
            fail_test_boot(
                ctx,
                report,
                req,
                &node,
                &format!("not eligible: {}", refusal.as_str()),
            )
            .await;
            return Ok(false);
        }
    };

    // The token: only its hash is stored; the plaintext goes into the cloud-init below and
    // nowhere else.
    let (token, hash) = test_boot::mint_token();
    test_boot_db::store_token(&ctx.pool, &node, &hash, deadline).await?;
    let user_data = test_boot::probe_cloud_init(report_url, &token);

    ensure_leader(ctx).await?;
    // The one create, recorded before it is sent. A request that is no longer running (it
    // expired meanwhile) is over: nothing is created for it.
    let recorded = rq::progress(
        &ctx.pool,
        &req.id,
        json!({"create_attempted": true, "create_attempted_at": now}),
    )
    .await?;
    if !recorded {
        test_boot_db::drop_token(&ctx.pool, &node).await?;
        return Ok(false);
    }
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
    // A runner that lost the lead mid-rent writes no failure for a request it no longer owns.
    // The request keeps `create_attempted`: the next leader does not send a second create, it
    // waits out the settle window and ends the boot.
    if outcome.lost_leadership() {
        return Err(FleetError::LostLeadership);
    }
    match outcome {
        RentOutcome::Created {
            candidate,
            provider_id,
            create_secs,
        } => {
            or_skip(
                report,
                &d.mm_node_id,
                "recording the created machine failed",
                rq::progress(
                    &ctx.pool,
                    &req.id,
                    json!({"phase": "booting", "provider_id": provider_id,
                        "zone": candidate.zone, "size": candidate.size, "create_secs": create_secs}),
                )
                .await,
            );
            report.created.push(d.mm_node_id.clone());
        }
        RentOutcome::MayExist { error, .. } => {
            let error = provider_text(&error);
            or_skip(
                report,
                &d.mm_node_id,
                "recording the unconfirmed create failed",
                rq::progress(
                    &ctx.pool,
                    &req.id,
                    json!({"phase": "create_unconfirmed", "error": error}),
                )
                .await,
            );
            report.not_created.push((d.mm_node_id.clone(), error));
        }
        RentOutcome::Abandoned { error, .. } => {
            let error = provider_text(&error);
            fail_test_boot(ctx, report, req, &node, &error).await;
            report.not_created.push((d.mm_node_id.clone(), error));
        }
        RentOutcome::NoneCreated { tried } => {
            let why = tried
                .iter()
                .map(|(_, w)| w.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            let why = provider_text(&why);
            fail_test_boot(ctx, report, req, &node, &why).await;
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
/// already redacted anything a provider sent back.
///
/// Every failure here is reported and the rest goes on, except that a teardown which could not
/// be ordered leaves the request running: the next tick ends it. A request that is no longer
/// running (expired, or already finished) is not finished or audited a second time.
async fn fail_test_boot(
    ctx: &FleetCtx,
    report: &mut FleetReport,
    req: &RequestRow,
    node: &NodeId,
    why: &str,
) {
    let id = node.as_str();
    if or_skip(
        report,
        id,
        "ordering its teardown failed",
        ctx.store.order_teardown(&target(id, None)).await,
    )
    .is_none()
    {
        return;
    }
    or_skip(
        report,
        id,
        "dropping its token failed",
        test_boot_db::drop_token(&ctx.pool, node).await,
    );
    if req.state != "running" {
        return;
    }
    if or_skip(
        report,
        id,
        "finishing its request failed",
        rq::finish(
            &ctx.pool,
            &req.id,
            false,
            json!({"phase": "failed", "error": why}),
        )
        .await,
    )
    .is_none()
    {
        return;
    }
    or_skip(
        report,
        id,
        "writing its audit row failed",
        pdb::append_audit(
            &ctx.pool,
            &pdb::AuditEntry {
                actor: RUNNER_ACTOR,
                action: "test_boot",
                target: &req.provider_id,
                reason: req.reason.as_deref(),
                detail: json!({"request_id": req.id, "node_id": id,
                    "requested_by": req.requested_by, "outcome": "failed", "error": why}),
            },
        )
        .await,
    );
    tracing::warn!(request = %req.id, node = %node, why, "test boot failed before it booted");
}

/// A report, ten minutes without one, or the deadline orders the machine's teardown. A test
/// boot whose machine never existed is ended when it is no longer pending (withdrawn by
/// `off`), or when its create was sent, left no row, and the settle window has passed. A
/// machine already `destroying` (an operator's Release, or `off`) is the end of the boot too:
/// its destroy completes below and the request finishes with the facts it has.
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
        let r_id = r.id.clone();
        let res = advance_one(ctx, now, r, &pending, report).await;
        item(report, &r_id, res)?;
    }
    Ok(())
}

async fn advance_one(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
    r: RequestRow,
    pending: &HashSet<String>,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    let node = test_boot::node_id_for(&r.id);
    let Some(n) = nodes_db::api_node(&ctx.pool, node.as_str()).await? else {
        if !pending.contains(node.as_str()) {
            fail_test_boot(ctx, report, &r, &node, "withdrawn before a machine existed").await;
        } else if create_attempted(&r) && settled(&r, now) {
            // The create was sent (or may have been) and left no row to look up: it is not
            // sent again, and the boot ends.
            fail_test_boot(ctx, report, &r, &node, OUTCOME_STAYED_UNKNOWN).await;
        }
        return Ok(());
    };
    match n.state.as_str() {
        "booting" | "healthy" => {}
        "destroying" => {
            if phase_of(&r) != Some("destroying") {
                rq::progress(&ctx.pool, &r.id, json!({"phase": "destroying"})).await?;
            }
            return Ok(());
        }
        _ => return Ok(()),
    }
    let why = if n.boot_report.is_some() {
        Some("report received")
    } else if n.destroy_deadline.is_some_and(|d| d <= now) {
        Some("deadline reached")
    } else {
        match n.billing_started_at {
            // A machine with a handle always has a start time; one without is not trusted to
            // be young.
            None => Some("no start time recorded"),
            Some(started) if now - started > chrono::Duration::seconds(BOOT_WAIT_SECS) => {
                Some("no report within 10 minutes")
            }
            Some(_) => None,
        }
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
    Ok(())
}

/// Completes every ordered teardown of a machine the runner created (mm-core orders them;
/// so do `off`, Release and this loop). A row without a handle is looked up first: its create
/// may have made a machine. `attempted` holds the nodes already tried this tick, so a second
/// pass in the same tick does not retry a destroy that just failed.
async fn complete_teardowns(
    ctx: &FleetCtx,
    attempted: &mut HashSet<String>,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    let owed: Vec<ApiNode> = nodes_db::api_nodes_live(&ctx.pool)
        .await?
        .into_iter()
        .filter(|n| n.state == "destroying" && !attempted.contains(&n.mm_node_id))
        .collect();
    for n in owed {
        attempted.insert(n.mm_node_id.clone());
        let id = n.mm_node_id.clone();
        let r = complete_one(ctx, n, report).await;
        item(report, &id, r)?;
    }
    Ok(())
}

async fn complete_one(
    ctx: &FleetCtx,
    n: ApiNode,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    let t = target(&n.mm_node_id, n.provider_id.clone());
    let done: Result<(), String> = match (n.provider_ref.as_deref(), n.provider_zone.as_deref()) {
        (Some(pref), Some(zone)) => {
            match ctx.adapters.adapter(pref, zone, ImageFor::Teardown).await {
                Ok(a) if n.provider_id.is_some() => ctx
                    .store
                    .complete_teardown(a.as_ref(), &t)
                    .await
                    .map_err(|e| e.to_string()),
                Ok(a) => match a.find(&NodeId::new(&n.mm_node_id)).await {
                    Ok(Some(h)) => {
                        // Recording the handle is a convenience: complete_teardown destroys
                        // the target's handle when the row has none.
                        or_skip(
                            report,
                            &n.mm_node_id,
                            "could not record the found machine's handle",
                            nodes_db::mark_created(&ctx.pool, &n.mm_node_id, &h).await,
                        );
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
            let why = provider_text(&why);
            tracing::error!(node = %n.mm_node_id, error = %why, "destroy failed; it stays destroying and is retried next tick");
            report.destroy_failed.push((n.mm_node_id, why));
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

/// A gone test-boot machine is confirmed absent at the provider before the request is done,
/// and absent means the lookup BY NODE TAG finds nothing: a machine the row never recorded (a
/// duplicate a timed-out create made) carries the tag too, and the recorded handle cannot see
/// it. Whatever the lookup returns is destroyed and the boot is looked at again next tick; it
/// finishes only when the lookup returns nothing. Then the result, the audit (an operator
/// cost, never a wallet charge) and the token's removal.
///
/// A machine found for a row with no handle is recorded on it (a `gone` row goes back to
/// `destroying`), so the destroy pass completes it. One found for a row that has a handle
/// is destroyed directly: the row's desired entry was deleted when the teardown was ordered,
/// and there is no second handle to record.
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
        let r_id = r.id.clone();
        let res = finish_one(ctx, now, r, report).await;
        item(report, &r_id, res)?;
    }
    Ok(())
}

async fn finish_one(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
    r: RequestRow,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    let node = test_boot::node_id_for(&r.id);
    let Some(n) = nodes_db::api_node(&ctx.pool, node.as_str()).await? else {
        return Ok(());
    };
    if n.state != "gone" {
        return Ok(());
    }
    let mut confirmed = false;
    if let (Some(pref), Some(zone)) = (n.provider_ref.as_deref(), n.provider_zone.as_deref()) {
        let adapter = match ctx.adapters.adapter(pref, zone, ImageFor::Teardown).await {
            Ok(a) => a,
            Err(why) => {
                rq::progress(
                    &ctx.pool,
                    &r.id,
                    json!({"phase": "confirming", "error": provider_text(&why)}),
                )
                .await?;
                return Ok(());
            }
        };
        match adapter.find(&node).await {
            Ok(None) => confirmed = true,
            Ok(Some(h)) => {
                ensure_leader(ctx).await?;
                tracing::error!(node = %node, provider_id = %h.provider_id, "a machine carrying a finished test boot's tag is still there; destroying it");
                let note = if n.provider_id.is_none() {
                    // No handle on the row: record this one, which puts the row back to
                    // `destroying` for the destroy pass.
                    match nodes_db::mark_created(&ctx.pool, &n.mm_node_id, &h).await {
                        Ok(_) => "a machine carrying the node tag was found after the teardown; recorded, to be destroyed".to_string(),
                        Err(e) => provider_text(&format!("a machine carrying the node tag was found and recording it failed: {e}")),
                    }
                } else {
                    match adapter.destroy(&h.provider_id).await {
                        Ok(()) => "a machine carrying the node tag was found after the teardown; destroyed again".to_string(),
                        Err(e) => provider_text(&e.to_string()),
                    }
                };
                rq::progress(
                    &ctx.pool,
                    &r.id,
                    json!({"phase": "confirming", "error": note}),
                )
                .await?;
                return Ok(());
            }
            Err(e) => {
                rq::progress(
                    &ctx.pool,
                    &r.id,
                    json!({"phase": "confirming", "error": provider_text(&e.to_string())}),
                )
                .await?;
                return Ok(());
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
    // The lookups above took time; a runner that lost the lead meanwhile finishes nothing.
    ensure_leader(ctx).await?;
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
    or_skip(
        report,
        &r.id,
        "dropping its token failed",
        test_boot_db::drop_token(&ctx.pool, &node).await,
    );
    or_skip(
        report,
        &r.id,
        "writing its audit row failed",
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
        .await,
    );
    tracing::info!(request = %r.id, node = %node, nvenc, confirmed, "test boot finished");
    report.finished.push((r.id.clone(), ok));
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
