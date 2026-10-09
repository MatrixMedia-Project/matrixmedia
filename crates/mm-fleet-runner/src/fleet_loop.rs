//! The fleet loop (spec §6.2 "Reconcile and rent" and "Sweepers", §6.3 Test boot): every
//! provider call the fleet makes, in one sequential pass, so no two steps race over a node.
//!
//! A tick, in order:
//! 1. `off` drains first: every API-made node's teardown is ordered, queued test boots refused.
//! 2. Ordered teardowns are completed, before anything that spends money.
//! 3. The desired rows of test boots that can no longer run are removed.
//! 4. Creates whose outcome is unknown are resolved: a machine found is destroyed; nothing found
//!    waits out the settle window ([`settle_window`]), and then ends the test boot (it is never
//!    sent again) or forgets the broadcast row (it is rented again).
//! 5. One queued test boot is claimed; pending desired rows are rented (≤ 5 creates): a test
//!    boot on the provider and zone it pinned, a broadcast transcoder through placement, and
//!    only under `fleet.mode = on` (a role whose backend is Terraform is Terraform's, and a
//!    fan-out role through the API is not built).
//! 6. Test boots advance: a report, ten minutes without one, or the deadline orders the teardown.
//! 7. The teardowns ordered above are completed; finished test boots are confirmed gone, by the
//!    node tag.
//! 8. The sweepers, then the tfvars file. The deadline sweeper runs every tick and destroys
//!    through the provider and zone that made each machine; the orphan sweeper runs every
//!    [`ORPHAN_EVERY_TICKS`] ticks and, for each provider that has a token, lists only machines
//!    carrying the API fleet tag, so a Terraform machine is never in its list. Zones removed
//!    from a provider are not swept (a zone with a live node cannot be removed, so only an
//!    unknown machine there escapes). The file holds what Terraform owns and nothing the API
//!    path rents.
//!
//! The leader lock is asked at the top of every step, before every provider call and before a
//! test boot's terminal writes. A runner that finds it has lost the lock returns
//! [`FleetError::LostLeadership`] at once and writes nothing more: whatever it left is
//! recorded, and the next leader's tick settles it. That error and the tick's own top-level
//! steps (their reads, and `off`'s refusal of queued test boots) are the only things that end a
//! tick's work: a failure about one node or one request is reported in `skipped` and the tick
//! goes on, so one bad row never stops a destroy. A top-level step that fails still leaves the
//! backstops to run: the ordered teardowns are completed and the deadline sweeper runs (the
//! orphan sweeper too, on its turn); nothing is claimed or rented, the tfvars file is not
//! written, and the tick returns that step's error. Settings that cannot be read are taken at
//! their safe values for the backstops, so an `off` is not drained until they can be read again.
//! A lost lead runs nothing more.
//!
//! A test boot gets exactly one create. Its request records `create_attempted` before the
//! create is sent, and nothing sends a second one: a create that timed out, or whose answer
//! was lost, may still land, and a lookup that finds nothing right after it proves nothing.
//! The boot is ended instead, once the settle window ([`settle_window`]) has passed, and the
//! operator runs it again. A broadcast row has no request to end: once its window has passed and
//! the lookup still finds nothing, its node row is forgotten and the desired row is rented
//! again. The window runs on the node row's own `requested_at`, dated just before each create
//! is sent.
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
use mm_core::fleet::billing::BillingIncrement;
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
use mm_fleet::sweeper::{sweep_deadlines_skipping, sweep_orphans};
use mm_fleet::test_boot::{self, BOOT_WAIT_SECS};
use mm_fleet::test_boot_db;
use mm_fleet::tfvars::TfvarsWriter;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::leader::LeaderCheck;

pub const FLEET_TICK_SECS: u64 = 10;
/// Every 30 ticks (5 minutes) the orphan sweeper runs.
pub const ORPHAN_EVERY_TICKS: u64 = 30;
/// The margin added to a create's own time limit ([`rent::CREATE_TIMEOUT`]) before an empty
/// lookup is believed: longer than the provider client's 60 s whole-request timeout plus two
/// ticks, so a create that timed out has had time to land and show up in a lookup.
pub const SETTLE_SECS: i64 = 180;

/// How long after a node's row was last dated (just before each create is sent) an empty
/// lookup still proves nothing: the whole time the create can block ([`rent::CREATE_TIMEOUT`]),
/// then [`SETTLE_SECS`]. The row is dated at the start of the last call, so counting from it
/// covers that call itself. Until then a row that may hold a machine is left alone and looked at
/// again; from then on a test boot whose machine has not appeared is ended, a broadcast row is
/// forgotten, and a teardown with no handle is closed.
pub fn settle_window() -> chrono::Duration {
    chrono::Duration::from_std(rent::CREATE_TIMEOUT).expect("the create timeout fits a Duration")
        + chrono::Duration::seconds(SETTLE_SECS)
}
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

/// When a test boot's request recorded that its create was sent.
fn attempted_at(r: &RequestRow) -> Option<DateTime<Utc>> {
    r.result
        .as_ref()
        .and_then(|v| v.get("create_attempted_at"))
        .and_then(|v| serde_json::from_value::<DateTime<Utc>>(v.clone()).ok())
}

/// Whether a create that began at `at` is old enough that a machine which has not appeared by
/// now is not coming ([`settle_window`] has passed). Fails closed: a create with no time is
/// never settled, because ending a boot or closing a row on an empty lookup that cannot be
/// dated is how a machine that lands late goes on billing unseen.
fn settled_since(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    at.is_some_and(|at| now - at >= settle_window())
}

/// A test boot whose create was sent and left no node row: dated by its request alone.
fn settled(r: &RequestRow, now: DateTime<Utc>) -> bool {
    settled_since(attempted_at(r), now)
}

pub async fn fleet_tick(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
    sweep_orphans_now: bool,
) -> Result<FleetReport, FleetError> {
    ensure_leader(ctx).await?;
    let mut report = FleetReport::default();
    let mut attempted = HashSet::new();
    // A step that fails ends the tick's work but not its backstops. Those need the node rows and
    // the providers' clients, not the requests or the settings, so a read they do not need (a
    // table a release forgot to grant, say) must not keep them idle while machines bill.
    // Settings that cannot be read are taken at their safe values.
    let (snap, failed) = match runner_settings::read(&ctx.pool).await {
        Ok(snap) => {
            let r = work(ctx, &snap, now, &mut attempted, &mut report).await;
            (snap, r.err())
        }
        Err(e) => (FleetSnapshot::safe(), Some(FleetError::from(e))),
    };
    match failed {
        None => {
            sweep(ctx, &snap, now, sweep_orphans_now, &mut report).await?;
            render(ctx, &snap).await?;
            Ok(report)
        }
        Some(FleetError::LostLeadership) => Err(FleetError::LostLeadership),
        // Nothing that spends, and no tfvars file: only what ends a bill, then the error.
        Some(e) => {
            backstop(
                "completing the ordered teardowns",
                complete_teardowns(ctx, now, &mut attempted, &mut report).await,
            )?;
            backstop(
                "the sweep",
                sweep(ctx, &snap, now, sweep_orphans_now, &mut report).await,
            )?;
            // The caller logs the error but not the report, so what the backstops did is said here.
            log_report(&report);
            Err(e)
        }
    }
}

/// The tick's steps before the sweepers, in order. The first failure ends them.
async fn work(
    ctx: &FleetCtx,
    snap: &FleetSnapshot,
    now: DateTime<Utc>,
    attempted: &mut HashSet<String>,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    if snap.mode == FleetMode::Off {
        drain(ctx, report).await?;
    }
    // Teardowns that are owed come before anything that spends: a destroy that frees a cap
    // slot, or ends a bill, never waits behind a rental.
    complete_teardowns(ctx, now, attempted, report).await?;
    clean_dead_test_boots(ctx, report).await?;
    resolve_may_exist(ctx, now, report).await?;
    if snap.mode != FleetMode::Off {
        claim_test_boot(ctx, report).await?;
        rent_pending(ctx, snap, now, report).await?;
    }
    advance_test_boots(ctx, now, report).await?;
    // The orders made above (a report in, a deadline, a lookup that found a machine).
    complete_teardowns(ctx, now, attempted, report).await?;
    finish_test_boots(ctx, now, report).await
}

/// A backstop run after a failed step. Losing the lead still ends the tick at once; any other
/// failure is logged here, and the tick returns the step's error.
fn backstop(what: &str, r: Result<(), FleetError>) -> Result<(), FleetError> {
    match r {
        Err(FleetError::LostLeadership) => Err(FleetError::LostLeadership),
        Err(e) => {
            tracing::error!(error = %provider_text(&e.to_string()), "{what} failed too, after a failed step");
            Ok(())
        }
        Ok(()) => Ok(()),
    }
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
/// later in this tick, except that a node whose create's outcome is still unknown (no handle,
/// not yet settled) stays `destroying` and is looked for every tick until it has settled. Nodes
/// made by Terraform are not the runner's to touch: mm-core drains those.
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
/// * Nothing found: that proves nothing until the create has settled ([`settle_window`] after
///   its row was written), because a create that timed out can still land. Until then the row is left
///   alone. After it, a test boot is ended (the row is closed through the destroy pass, which
///   looks once more) and never retried; a row with no request to end (a broadcast one) is
///   forgotten, which hands its desired row back to the next rental.
/// * Lookup failed: try again next tick.
///
/// The clock is the node row's own `requested_at`, dated before the last create was sent; for a
/// test boot, the request's `create_attempted_at` counts too when it is later. Whichever is
/// later starts the window, so the window never opens early.
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
            // The row was read at the top of the tick; if it has gone since, there is nothing
            // left to settle.
            let Some(written) = nodes_db::requested_at(&ctx.pool, &n.mm_node_id).await? else {
                return Ok(());
            };
            let sent = Some(written).max(request.as_ref().and_then(attempted_at));
            if !settled_since(sent, now) {
                report.skipped.push((
                    n.mm_node_id,
                    "nothing found yet, but its create may still land; waiting".into(),
                ));
                return Ok(());
            }
            // The lookup took time; a runner that lost the lead meanwhile ends nothing.
            ensure_leader(ctx).await?;
            match request {
                // `fail_test_boot` has already reported (in `skipped`) a boot whose teardown
                // could not be ordered: it is still running, so it is not "resolved".
                Some(req) => {
                    if fail_test_boot(ctx, report, &req, &node, OUTCOME_STAYED_UNKNOWN).await {
                        report.resolved.push((n.mm_node_id, "not_found"));
                    }
                }
                // No request to end: the row of a create that, a settle window later, left
                // nothing. Forgetting it lets the rental try again under the same id.
                None => match nodes_db::forget_uncreated(&ctx.pool, &n.mm_node_id).await {
                    Ok(true) => report.resolved.push((n.mm_node_id, "forgotten")),
                    Ok(false) => report.skipped.push((
                        n.mm_node_id,
                        "its row changed meanwhile; looked at again next tick".into(),
                    )),
                    Err(e) => skip(report, &n.mm_node_id, "forgetting its row failed", e),
                },
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
            Some(Purpose::Broadcast) => match rent_broadcast(ctx, snap, now, &d, report).await {
                Ok(attempted) => attempted,
                Err(FleetError::LostLeadership) => return Err(FleetError::LostLeadership),
                // One row's trouble (a read that failed) is that row's: the rest still rent.
                Err(e) => {
                    skip(report, &d.mm_node_id, "renting failed", e);
                    false
                }
            },
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

/// Rents the machine of a broadcast row whose role's backend is `api` (spec §6.2), and only
/// under `on`: `frozen` and `off` rent nothing, and a role whose backend is Terraform is
/// Terraform's (the row is rendered to tfvars). This path rents GPU transcoders only; an api
/// fan-out row is reported, never rented. `true` when a machine was attempted.
///
/// The machine goes where placement says (the strategy ranks the eligible candidates; the
/// rules are not its to bend). Its deadline is the desired row's at this moment, copied once
/// into the node row. A create of unknown outcome is only reported here: the next tick looks
/// it up, and a row that stays empty past its settle window is forgotten and rented again.
async fn rent_broadcast(
    ctx: &FleetCtx,
    snap: &FleetSnapshot,
    now: DateTime<Utc>,
    d: &PendingDesired,
    report: &mut FleetReport,
) -> Result<bool, FleetError> {
    if snap.mode != FleetMode::On {
        return Ok(false);
    }
    let Some(role) = NodeFlavor::parse(&d.flavor).and_then(Role::from_flavor) else {
        report
            .skipped
            .push((d.mm_node_id.clone(), "a flavor that is never rented".into()));
        return Ok(false);
    };
    if snap.backend_for(role) != Backend::Api {
        return Ok(false); // Terraform's; rendered to tfvars below
    }
    if role != Role::Transcode {
        report
            .skipped
            .push((d.mm_node_id.clone(), "api_fanout_not_built".into()));
        return Ok(false);
    }
    let Some(deadline) = d.destroy_deadline else {
        report.skipped.push((
            d.mm_node_id.clone(),
            "a rented desired row without a deadline".into(),
        ));
        return Ok(false);
    };
    // A machine made this close to its deadline would be torn down by the deadline sweeper
    // before a lookup of its create could be believed ([`settle_window`]): rented for nothing,
    // and possibly beyond what the row knows. A deadline already past is the same case.
    if deadline - now < settle_window() {
        report.skipped.push((
            d.mm_node_id.clone(),
            "its deadline is too close to rent a machine for it".into(),
        ));
        return Ok(false);
    }
    ensure_leader(ctx).await?;
    let (facts, live) = placement_db::load_facts(&ctx.pool).await?;
    let preq = PlacementRequest {
        role,
        region: d.region.clone(),
        purpose: Purpose::Broadcast,
        backend: Backend::Api,
        now,
    };
    let limits = Limits {
        max_gpu_nodes: snap.max_gpu_nodes,
        gpu_nodes_live: live,
    };
    let placed = placement::place(ctx.strategy.as_ref(), &facts, &preq, &limits);
    if placed.candidates.is_empty() {
        let why = placed
            .excluded
            .iter()
            .map(|e| {
                format!(
                    "{}{}: {}",
                    e.provider_id,
                    e.zone
                        .as_deref()
                        .map(|z| format!("/{z}"))
                        .unwrap_or_default(),
                    e.reason.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        report.skipped.push((
            d.mm_node_id.clone(),
            format!("no eligible provider ({why})"),
        ));
        return Ok(false);
    }
    let node = NodeId::new(&d.mm_node_id);
    // Names the node and its flavor and nothing else: no secret rides in a broadcast's user data.
    let user_data = test_boot::transcode_cloud_init(&node);
    ensure_leader(ctx).await?;
    tracing::info!(
        node = %node,
        strategy = ctx.strategy.name(),
        candidates = placed.candidates.len(),
        "renting a broadcast transcoder"
    );
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
            purpose: Purpose::Broadcast,
            destroy_deadline: deadline,
            created_by: None,
            user_data: &user_data,
            image: ImageFor::Broadcast,
        },
        &placed.candidates,
    )
    .await;
    // A runner that lost the lead mid-rent writes nothing more: whatever it left is recorded,
    // and the next leader's tick settles it.
    if outcome.lost_leadership() {
        return Err(FleetError::LostLeadership);
    }
    // A broadcast row has no request to carry the reason, so the log does: redacted, since it
    // may hold a provider's words.
    let not_created = match outcome {
        RentOutcome::Created { .. } => {
            report.created.push(d.mm_node_id.clone());
            return Ok(true);
        }
        RentOutcome::MayExist { error, .. } | RentOutcome::Abandoned { error, .. } => error,
        RentOutcome::NoneCreated { tried } => tried
            .iter()
            .map(|(_, w)| w.as_str())
            .collect::<Vec<_>>()
            .join("; "),
    };
    let why = provider_text(&not_created);
    tracing::warn!(node = %node, error = %why, "a broadcast rental did not create a machine");
    report.not_created.push((d.mm_node_id.clone(), why));
    Ok(true)
}

/// Ends a test boot that has no machine (any more): its desired row and token go, the
/// request fails, the audit says so. `why` is recorded and logged as given, so the caller has
/// already redacted anything a provider sent back.
///
/// Every failure here is reported and the rest goes on, except that a teardown which could not
/// be ordered leaves the request running: the next tick ends it. A request that is no longer
/// running (expired, or already finished) is not finished or audited a second time.
///
/// `false` when the teardown could not be ordered (reported in `skipped`; nothing was ended).
async fn fail_test_boot(
    ctx: &FleetCtx,
    report: &mut FleetReport,
    req: &RequestRow,
    node: &NodeId,
    why: &str,
) -> bool {
    let id = node.as_str();
    if or_skip(
        report,
        id,
        "ordering its teardown failed",
        ctx.store.order_teardown(&target(id, None)).await,
    )
    .is_none()
    {
        return false;
    }
    or_skip(
        report,
        id,
        "dropping its token failed",
        test_boot_db::drop_token(&ctx.pool, node).await,
    );
    if req.state != "running" {
        return true;
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
        return true;
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
    true
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
/// may have made a machine. Found, the machine is destroyed. Not found, that closes the row
/// only once the create has settled ([`settle_window`] after its row was written): a create
/// that timed out can still land, so until then the row stays `destroying` and is looked at
/// every tick. `attempted` holds the nodes already tried this tick, so a second pass in the
/// same tick does not retry a destroy that just failed.
async fn complete_teardowns(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
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
        let r = complete_one(ctx, now, n, report).await;
        item(report, &id, r)?;
    }
    Ok(())
}

async fn complete_one(
    ctx: &FleetCtx,
    now: DateTime<Utc>,
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
                    Ok(None) => {
                        // Nothing found proves nothing until the create has settled. A row
                        // that has vanished since it was read has nothing left to close.
                        let Some(written) =
                            nodes_db::requested_at(&ctx.pool, &n.mm_node_id).await?
                        else {
                            return Ok(());
                        };
                        if !settled_since(Some(written), now) {
                            report.skipped.push((
                                n.mm_node_id,
                                "nothing found yet, but its create may still land; looked at again"
                                    .into(),
                            ));
                            return Ok(());
                        }
                        ctx.store
                            .complete_teardown(a.as_ref(), &t)
                            .await
                            .map_err(|e| e.to_string())
                    }
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

/// The backstops. Neither is a capacity mechanism: a successful sweep is evidence that an
/// earlier path failed, which is why both count what they find.
///
/// The deadline sweeper runs every tick, through the provider and zone that made each
/// machine (a [`RoutedProvider`] built from the API nodes that have a handle). A machine whose
/// provider cannot be reached stays `destroying`; a handle recorded under two providers is
/// refused. A failure of the whole sweep is logged and never ends the tick.
///
/// The orphan sweeper runs when `orphans_now`, for every provider that has a token (disabled
/// and bench-gated ones included, since their machines keep billing), in each of its zones,
/// through a client that lists only machines carrying the API fleet tag. A listing that fails
/// destroys nothing, and a provider or zone that fails is that one's alone.
async fn sweep(
    ctx: &FleetCtx,
    snap: &FleetSnapshot,
    now: DateTime<Utc>,
    orphans_now: bool,
    report: &mut FleetReport,
) -> Result<(), FleetError> {
    ensure_leader(ctx).await?;
    let live = nodes_db::api_nodes_live(&ctx.pool).await?;
    mm_fleet::metrics::GPU_NODES_RUNNING
        .set(live.iter().filter(|n| n.flavor == "transcode").count() as i64);
    let routed = RoutedProvider::build(ctx.adapters.as_ref(), &live).await;
    // A row with no handle may stand for a create whose outcome is unknown. The deadline sweeper
    // would close it as `gone` with no provider call at all, and a machine that lands later
    // would be recorded nowhere. So it never sees them: one that is past its deadline has its
    // teardown ordered here, and the destroy pass looks for its machine by the node tag on every
    // tick and closes it only once the create has settled.
    let mut handle_less: HashSet<NodeId> = HashSet::new();
    for n in live.iter().filter(|n| n.provider_id.is_none()) {
        handle_less.insert(NodeId::new(&n.mm_node_id));
        let overdue = n.destroy_deadline.is_some_and(|d| d <= now);
        if overdue
            && n.state != "destroying"
            && order(ctx, report, &target(&n.mm_node_id, None)).await
        {
            report.ordered.push(n.mm_node_id.clone());
        }
    }
    // Every adapter built today bills GPUs per minute: an hourly one would bring a per-node
    // increment here.
    match sweep_deadlines_skipping(
        &ctx.store,
        &routed,
        BillingIncrement::PerMinute,
        now,
        &handle_less,
    )
    .await
    {
        Ok(r) => {
            for id in r.failed {
                report.skipped.push((
                    id,
                    "its deadline teardown failed; it stays destroying".into(),
                ));
            }
            report.deadline_reaped = r.reaped;
        }
        Err(e) => tracing::error!(error = %e, "deadline sweep failed"),
    }
    if !orphans_now {
        return Ok(());
    }
    let min_age = chrono::Duration::seconds(snap.orphan_min_age_secs);
    for p in pdb::list(&ctx.pool).await? {
        if p.credential.is_none() || !placement::adapter_built(&p.row.kind) {
            continue;
        }
        for z in &p.zones {
            ensure_leader(ctx).await?;
            let item = format!("{}/{}", p.row.id, z.zone);
            let adapter = match ctx
                .adapters
                .adapter(&p.row.id, &z.zone, ImageFor::Teardown)
                .await
            {
                Ok(a) => a,
                Err(why) => {
                    skip(report, &item, "orphan sweep has no client", why);
                    continue;
                }
            };
            match sweep_orphans(&ctx.store, adapter.as_ref(), now, min_age).await {
                Ok(r) => {
                    let found = (r.reaped.len() + r.failed.len()) as u64;
                    if found > 0 {
                        mm_fleet::metrics::ORPHANS_FOUND
                            .with_label_values(&[p.row.id.as_str()])
                            .inc_by(found);
                    }
                    for id in r.failed {
                        report
                            .skipped
                            .push((id, "orphan destroy failed; looked at again".into()));
                    }
                    report.orphans.extend(r.reaped);
                }
                Err(e) => skip(
                    report,
                    &item,
                    "orphan sweep could not list; nothing destroyed",
                    e,
                ),
            }
        }
    }
    // Housekeeping: a table that cannot be cleaned this time is cleaned the next, not a reason to
    // end the tick.
    or_skip(
        report,
        "zone holds",
        "purging the expired ones failed",
        placement_db::purge_expired_cooldowns(&ctx.pool).await,
    );
    Ok(())
}

/// Writes what Terraform owns to the tfvars file: the broadcast rows of each role whose backend
/// is `terraform`. Best effort, as the file always was: the database is already right, a failed
/// render is logged at error (until it succeeds Terraform acts on a stale set) and the next
/// tick renders again. The file is the most destructive artifact the fleet writes, so only the
/// leader writes it.
async fn render(ctx: &FleetCtx, snap: &FleetSnapshot) -> Result<(), FleetError> {
    let Some(writer) = &ctx.tfvars else {
        return Ok(());
    };
    let mut flavors: Vec<&str> = Vec::new();
    if snap.create_backend_transcode == Backend::Terraform {
        flavors.push("transcode");
    }
    if snap.create_backend_fanout == Backend::Terraform {
        flavors.extend(["fanout", "edge"]);
    }
    ensure_leader(ctx).await?;
    if let Err(e) =
        mm_fleet::tfvars::render_terraform_roles(&ctx.store, &ctx.pool, writer, &flavors).await
    {
        tracing::error!(error = %provider_text(&e), "rendering tfvars failed; Terraform is acting on a stale desired set");
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
