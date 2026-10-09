//! The runner's loops (spec §6.2). Each is supervised: a panic or an unexpected return
//! restarts it with backoff, the same shape mm-server uses for the meter and ladder loops.
//!
//! Four loops run: the heartbeat, the provider checks, the operator requests (Test connection)
//! and the fleet loop (`fleet_loop`), which is the only one that creates or destroys a
//! machine. The unsealed token lives only inside `evaluate` and the adapters: a log line
//! carries a provider id, kind and state, and a status row carries the provider's own status
//! line, never the credential.
//!
//! The fleet loop is also the one that notices a lost leader lock. It cancels every loop,
//! `run_forever` returns once they have all stopped, and the caller (`app::finish_run`) then
//! ends the run with an error, so the process exits non-zero, without releasing a lock that is
//! already gone.
//!
//! The checks publish two metrics: `mm_fleet_provider_check_seconds` (how long each check took)
//! and `mm_fleet_provider_state` (one series per live provider, on its current state).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mm_fleet::adapters::{self, StandIn};
use mm_fleet::control_db::{self, Heartbeat};
use mm_fleet::endpoint::EndpointError;
use mm_fleet::placement_db;
use mm_fleet::providers_db::{self as pdb, ProviderFull, StatusRow};
use mm_fleet::requests_db as rq;
use mm_fleet::runner_settings;
use mm_fleet::sealed::Keypair;
use serde_json::json;
use sqlx::PgPool;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

pub const HEARTBEAT_SECS: u64 = 15;
pub const CHECK_SECS: u64 = 300;
pub const REQUEST_POLL_MS: u64 = 2000;
/// How often the checks loop looks for a profile or token change to re-check at once.
const CHANGE_POLL_SECS: u64 = 5;
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the runner's loops share. `Clone`: every field is a handle.
#[derive(Clone)]
pub struct RunnerParts {
    pub pool: PgPool,
    pub kp: Arc<Keypair>,
    pub leader: Arc<dyn crate::leader::LeaderCheck>,
    pub strategy: Arc<dyn mm_fleet::placement::PlacementStrategy>,
    /// The tfvars file the runner writes (`MM_FLEET_TFVARS_PATH`). The file is written into its
    /// directory under the name Terraform loads automatically, so a path with any other name
    /// is still reported by the name it is written under. `None`: no file is written.
    pub tfvars_path: Option<PathBuf>,
}

/// Spawns the supervised loops and returns once `cancel` fires and they have stopped. The
/// fleet loop cancels everything itself if the leader lock is lost.
pub async fn run_forever(parts: RunnerParts, cancel: CancellationToken) {
    let (pool, kp) = (parts.pool.clone(), parts.kp.clone());
    let tfvars_file = tfvars_file(parts.tfvars_path.as_deref());
    let h = {
        let (pool, kp, token) = (pool.clone(), kp.clone(), cancel.clone());
        let leader = parts.leader.clone();
        supervise("heartbeat", cancel.clone(), move || {
            heartbeat_loop(
                pool.clone(),
                kp.clone(),
                token.clone(),
                tfvars_file.clone(),
                leader.clone(),
            )
        })
    };
    let c = spawn_loop("checks", &pool, &kp, &cancel, checks_loop);
    let r = spawn_loop("requests", &pool, &kp, &cancel, requests_loop);
    let f = {
        let (parts, token) = (parts.clone(), cancel.clone());
        supervise("fleet", cancel.clone(), move || {
            fleet_loop(parts.clone(), token.clone())
        })
    };
    cancel.cancelled().await;
    let _ = tokio::join!(h, c, r, f);
}

/// The file the tfvars writer produces for a configured path: its directory, the fixed name.
fn tfvars_file(configured: Option<&Path>) -> Option<PathBuf> {
    configured.and_then(Path::parent).map(|dir| {
        mm_fleet::tfvars::TfvarsWriter::new(dir)
            .path()
            .to_path_buf()
    })
}

/// A difference between the host's clock and the database's above this is logged.
pub const CLOCK_SKEW_WARN_SECS: i64 = 5;

/// How far the database's clock is from the host's, when that is worth saying.
pub fn clock_skew(host: DateTime<Utc>, database: DateTime<Utc>) -> Option<chrono::Duration> {
    let skew = database - host;
    (skew.abs() > chrono::Duration::seconds(CLOCK_SKEW_WARN_SECS)).then_some(skew)
}

/// One turn of the fleet loop. The tick's `now` is the DATABASE's clock, read once here: the
/// instants the settle check compares it with (`requested_at`, the stamps the previous ticks
/// wrote, every deadline) are all the database's, and a host whose clock runs ahead or behind
/// must not settle a create early or late. A skew above [`CLOCK_SKEW_WARN_SECS`] is logged, once
/// per orphan-sweep interval so a standing skew does not fill the log. `host_now` is the host's
/// clock, for that comparison only.
///
/// `Err(LostLeadership)` is the one outcome the caller must act on; every other failure is
/// logged and the loop goes on. If the database clock cannot be read the turn is skipped: the
/// database is not answering, and nothing it holds can be acted on.
pub async fn fleet_turn(
    ctx: &crate::fleet_loop::FleetCtx,
    host_now: DateTime<Utc>,
    tick: u64,
) -> Result<(), crate::fleet_loop::FleetError> {
    use crate::fleet_loop::{FleetError, ORPHAN_EVERY_TICKS, fleet_tick, log_report};
    let now = match db_clock(&ctx.pool).await {
        Ok(now) => now,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the database clock; no fleet tick this turn");
            return Ok(());
        }
    };
    if tick.is_multiple_of(ORPHAN_EVERY_TICKS)
        && let Some(skew) = clock_skew(host_now, now)
    {
        tracing::warn!(
            skew_secs = skew.num_seconds(),
            "the database's clock differs from this host's; fleet ticks run on the database's"
        );
    }
    match fleet_tick(ctx, now, tick.is_multiple_of(ORPHAN_EVERY_TICKS)).await {
        Ok(report) => {
            log_report(&report);
            Ok(())
        }
        Err(FleetError::LostLeadership) => Err(FleetError::LostLeadership),
        Err(e) => {
            tracing::error!(error = %e, "fleet tick failed");
            Ok(())
        }
    }
}

/// The fleet loop: one tick every [`FLEET_TICK_SECS`](crate::fleet_loop::FLEET_TICK_SECS), the
/// orphan sweep on the first and then every
/// [`ORPHAN_EVERY_TICKS`](crate::fleet_loop::ORPHAN_EVERY_TICKS)th. A tick that finds the
/// leader lock lost stops every loop, not only this one: nothing may act for a runner that is
/// no longer the leader.
async fn fleet_loop(parts: RunnerParts, cancel: CancellationToken) {
    use crate::fleet_loop::{FLEET_TICK_SECS, FleetCtx, FleetError};
    let ctx = FleetCtx {
        pool: parts.pool.clone(),
        store: mm_fleet::desired::DesiredStore::new(parts.pool.clone()),
        adapters: Arc::new(adapters::SealedAdapters::new(
            parts.pool.clone(),
            parts.kp.clone(),
        )),
        strategy: parts.strategy.clone(),
        leader: parts.leader.clone(),
        tfvars: parts
            .tfvars_path
            .as_deref()
            .and_then(Path::parent)
            .map(mm_fleet::tfvars::TfvarsWriter::new),
        backoff: mm_fleet::rent::BACKOFF.to_vec(),
    };
    let mut t = tokio::time::interval(Duration::from_secs(FLEET_TICK_SECS));
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tick: u64 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = t.tick() => {
                if let Err(FleetError::LostLeadership) = fleet_turn(&ctx, Utc::now(), tick).await {
                    tracing::error!("lost the leader lock; stopping every loop");
                    cancel.cancel();
                    break;
                }
                mm_core::metrics_global::heartbeat("fleet_loop");
                tick += 1;
            }
        }
    }
}

fn spawn_loop<F, Fut>(
    name: &'static str,
    pool: &PgPool,
    kp: &Arc<Keypair>,
    cancel: &CancellationToken,
    body: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(PgPool, Arc<Keypair>, CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (pool, kp, token) = (pool.clone(), kp.clone(), cancel.clone());
    supervise(name, cancel.clone(), move || {
        body(pool.clone(), kp.clone(), token.clone())
    })
}

/// Beats every [`HEARTBEAT_SECS`]. The heartbeat row also publishes the runner's PUBLIC KEY,
/// which the dashboard seals provider tokens to, so only the leader may write it: a runner that
/// lost the lock and wrote anyway would replace the new leader's key with its own. The lock is
/// asked before every write, and a runner that finds it lost stops every loop, as the fleet
/// loop does.
pub async fn heartbeat_loop(
    pool: PgPool,
    kp: Arc<Keypair>,
    cancel: CancellationToken,
    tfvars_file: Option<PathBuf>,
    leader: Arc<dyn crate::leader::LeaderCheck>,
) {
    let mut t = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = t.tick() => {
                if !leader.still_leader().await {
                    tracing::error!("lost the leader lock; stopping every loop");
                    cancel.cancel();
                    break;
                }
                match heartbeat_once_with(&pool, &kp, VERSION, tfvars_file.as_deref()).await {
                    Ok(()) => mm_fleet::metrics::RUNNER_HEARTBEAT_TIMESTAMP.set(Utc::now().timestamp()),
                    Err(e) => tracing::error!(error = %e, "heartbeat failed"),
                }
                mm_core::metrics_global::heartbeat("fleet_runner_heartbeat");
            }
        }
    }
}

async fn checks_loop(pool: PgPool, kp: Arc<Keypair>, cancel: CancellationToken) {
    let mut t = tokio::time::interval(Duration::from_secs(CHECK_SECS));
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick fires at once and covers startup, so the baseline is "as of now": only a
    // later change earns an extra pass.
    let mut seen = latest_provider_change(&pool).await.ok().flatten();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = t.tick() => {
                match checks_once(&pool, &kp, None).await {
                    Ok(n) => tracing::info!(providers = n, "checks done"),
                    Err(e) => tracing::error!(error = %e, "checks failed"),
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(CHANGE_POLL_SECS)) => {
                // A profile edit or a new/cleared token bumps `updated_at`: re-check at once
                // instead of showing the old verdict for up to CHECK_SECS.
                match latest_provider_change(&pool).await {
                    Ok(latest) if latest > seen => {
                        seen = latest;
                        t.reset_immediately();
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "provider change poll failed"),
                }
            }
        }
    }
}

async fn latest_provider_change(pool: &PgPool) -> sqlx::Result<Option<DateTime<Utc>>> {
    sqlx::query_scalar("SELECT max(updated_at) FROM mm_fleet_providers WHERE deleted_at IS NULL")
        .fetch_one(pool)
        .await
}

async fn requests_loop(pool: PgPool, kp: Arc<Keypair>, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(REQUEST_POLL_MS)) => {
                if let Err(e) = requests_once(&pool, &kp, None).await {
                    tracing::error!(error = %e, "request loop failed");
                }
            }
        }
    }
}

/// Like mm-server startup.rs `supervise` (private there): run the loop as its own task, so a
/// panic surfaces as a `JoinError`, and restart it with backoff unless shutting down.
fn supervise<F, Fut>(
    name: &'static str,
    cancel: CancellationToken,
    mut make: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);
        while !cancel.is_cancelled() {
            let res = tokio::spawn(make()).await;
            if cancel.is_cancelled() {
                break;
            }
            match res {
                Ok(()) => tracing::warn!(task = name, "loop returned unexpectedly; restarting"),
                Err(e) => tracing::error!(task = name, error = %e, "loop panicked; restarting"),
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = cancel.cancelled() => break,
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    })
}

/// One heartbeat that knows of no tfvars file: see [`heartbeat_once_with`].
pub async fn heartbeat_once(pool: &PgPool, kp: &Keypair, version: &str) -> sqlx::Result<()> {
    heartbeat_once_with(pool, kp, version, None).await
}

/// One heartbeat: mode and settings revision the runner is acting on, the live providers'
/// last verdicts, and how many rented nodes still bill. Soft-deleted providers are not
/// listed: the detail comes from `providers_db::list`, not from the status table.
///
/// `detail` also says what the runner resolved its settings to (the page shows what the runner
/// acts on, not what was typed): the region and the two backends as the text they are set in,
/// the GPU cap as a number; the zones on hold; and when the tfvars file was last written (its
/// modification time; `null` when no file is configured or none exists yet). Nothing in it is a
/// token, a key or a credential.
pub async fn heartbeat_once_with(
    pool: &PgPool,
    kp: &Keypair,
    version: &str,
    tfvars_file: Option<&Path>,
) -> sqlx::Result<()> {
    let snap = runner_settings::read(pool).await?;
    let providers = pdb::list(pool).await?;
    let rented: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mm_fleet_nodes WHERE ownership = 'rented' AND state <> 'gone'",
    )
    .fetch_one(pool)
    .await?;
    let cooldowns = placement_db::cooldowns(pool).await?;
    let tfvars_written_at: Option<DateTime<Utc>> = tfvars_file
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .map(DateTime::<Utc>::from);
    let detail = json!({
        "providers": providers.iter().map(|p| json!({
            "id": p.row.id,
            "state": p.status.as_ref().map(|s| s.state.as_str()),
            "checked_at": p.status.as_ref().map(|s| s.checked_at),
            "last_error_kind": p.status.as_ref().and_then(|s| s.last_error_kind.as_deref()),
        })).collect::<Vec<_>>(),
        "rented_nodes": rented,
        "settings": {
            "default_region": snap.default_region,
            "create_backend_transcode": snap.create_backend_transcode.as_str(),
            "create_backend_fanout": snap.create_backend_fanout.as_str(),
            "max_gpu_nodes": snap.max_gpu_nodes,
        },
        "cooldowns": cooldowns,
        "tfvars_written_at": tfvars_written_at,
    });
    control_db::heartbeat(
        pool,
        &Heartbeat {
            runner_version: version,
            public_key: &kp.public_bytes(),
            key_fingerprint: &kp.fingerprint(),
            fleet_mode_seen: &snap.mode.to_string(),
            settings_rev_seen: snap.rev,
            detail,
        },
    )
    .await
}

/// A status row with no measurements: a verdict that never reached the provider. `checked_at`
/// is when the evaluation began (see `evaluate`), not when the row was built.
fn status_row(
    p: &ProviderFull,
    state: &str,
    error: Option<(&str, &str)>,
    checked_at: DateTime<Utc>,
) -> StatusRow {
    StatusRow {
        provider_id: p.row.id.clone(),
        checked_at,
        state: state.into(),
        key_scope: None,
        quota: json!({}),
        stock: json!({}),
        prices: json!({}),
        balance_minor: None,
        last_error: error.map(|(_, msg)| msg.into()),
        last_error_kind: error.map(|(kind, _)| kind.into()),
        last_error_at: error.map(|_| checked_at),
    }
}

/// The (state, error kind) a refused endpoint is recorded as. A bad or private endpoint is
/// the operator's to fix; a name that would not resolve is a resolver hiccup, so the
/// provider is "unknown" for now and the next pass tries again.
fn endpoint_verdict(e: EndpointError) -> (&'static str, &'static str) {
    match e {
        EndpointError::Invalid | EndpointError::Forbidden => ("needs_you", "permanent"),
        EndpointError::Unresolved => ("unknown", "transient"),
    }
}

/// The database's own clock. Placement compares a verdict's `checked_at` with the token's
/// `entered_at`, which the database stamps with its `now()`: both must come from the one
/// clock, or a runner whose clock runs ahead would date a check of the old token after the
/// token that replaced it.
async fn db_clock(pool: &PgPool) -> sqlx::Result<DateTime<Utc>> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await
}

/// The verdict for one provider, without writing it; `None` when the verdict must not be
/// written.
///
/// Every row it returns is stamped with the database time at which the evaluation began,
/// taken before the token is read. Placement trusts a verdict only if `checked_at` is not
/// older than the token's `entered_at`, so a check of the old token that straddles a
/// replacement must read as older than the new token. Stamped after the network call, it
/// would read as newer, and the never-checked new token would pass as verified.
///
/// Dating alone is not enough: the database stamps `entered_at` with the start of the
/// writer's transaction, which can precede the check's start while the commit lands after
/// the token was read. So after the provider answers, the token's `entered_at` is read
/// again, and if it changed the verdict is about a token that is no longer stored: `None`.
/// The next pass (the change poll) checks the new token.
async fn evaluate(
    pool: &PgPool,
    kp: &Keypair,
    p: &ProviderFull,
    stand_in: Option<StandIn<'_>>,
) -> sqlx::Result<Option<StatusRow>> {
    let checked_from = db_clock(pool).await?;
    let Some((blob, entered_at)) = pdb::load_credential_entered(pool, &p.row.id).await? else {
        return Ok(Some(status_row(p, "waiting_for_token", None, checked_from)));
    };
    let pt = match adapters::open_credential(kp, p, &blob) {
        Ok(pt) => pt,
        Err((state, msg)) => {
            return Ok(Some(status_row(
                p,
                state,
                Some(("permanent", msg)),
                checked_from,
            )));
        }
    };
    // The sealed endpoint is vetted inside `checker_for`, before a checker exists, and only
    // for a kind that has one (no DNS lookup for a kind whose checks are not built). A test
    // points the checker at a stand-in on 127.0.0.1, which that vetting exists to refuse.
    let checker = match adapters::checker_for(&p.row.kind, &pt, &p.zones, stand_in).await {
        Ok(Some(checker)) => checker,
        Ok(None) => {
            return Ok(Some(status_row(
                p,
                "unknown",
                Some(("unsupported", "checks for this provider are not built yet")),
                checked_from,
            )));
        }
        Err(e) => {
            let (state, kind) = endpoint_verdict(e);
            return Ok(Some(status_row(
                p,
                state,
                Some((kind, &e.to_string())),
                checked_from,
            )));
        }
    };
    let report = checker.check().await;
    let still_stored = pdb::load_credential_entered(pool, &p.row.id)
        .await?
        .is_some_and(|(_, at)| at == entered_at);
    if !still_stored {
        return Ok(None);
    }
    Ok(Some(report.to_status_row(
        &p.row.id,
        p.row.max_gpu_nodes,
        checked_from,
    )))
}

/// A verdict that was judged about the stored token, and whether it reached the status table.
struct Checked {
    row: StatusRow,
    /// `false`: the provider was deleted while it was checked, so nothing was written.
    stored: bool,
}

/// Checks one provider and records the verdict. `None`: the token was replaced during the
/// check, so the verdict was dropped unwritten.
///
/// The check is timed (`mm_fleet_provider_check_seconds`) whatever it comes to, and a verdict
/// that reached the status table is published as the provider's state.
async fn check_provider(
    pool: &PgPool,
    kp: &Keypair,
    p: &ProviderFull,
    stand_in: Option<StandIn<'_>>,
) -> sqlx::Result<Option<Checked>> {
    let timer = mm_fleet::metrics::PROVIDER_CHECK_SECONDS.start_timer();
    let verdict = evaluate(pool, kp, p, stand_in).await;
    timer.observe_duration();
    let Some(row) = verdict? else {
        tracing::debug!(provider = %p.row.id, "token replaced during its check; verdict dropped");
        return Ok(None);
    };
    let stored = pdb::upsert_status(pool, &row).await?;
    if stored {
        publish_state(&p.row.id, &row.state);
    } else {
        tracing::debug!(provider = %p.row.id, "provider deleted during its check; verdict dropped");
    }
    tracing::info!(provider = %p.row.id, kind = %p.row.kind, state = %row.state, "provider checked");
    Ok(Some(Checked { row, stored }))
}

/// Every state a status row can hold (the CHECK on `mm_fleet_provider_status.state`; a test
/// compares the two).
pub const PROVIDER_STATES: [&str; 5] = [
    "ok",
    "needs_you",
    "waiting_for_token",
    "endpoint_mismatch",
    "unknown",
];

/// `mm_fleet_provider_state` for one provider: 1 on its current state and no series for any
/// other, so a provider whose verdict changed (a pass, or a Test connection between passes) is
/// never counted in two states at once.
fn publish_state(provider_id: &str, state: &str) {
    use mm_fleet::metrics::PROVIDER_STATE;
    for other in PROVIDER_STATES.iter().filter(|s| **s != state) {
        let _ = PROVIDER_STATE.remove_label_values(&[provider_id, other]);
    }
    PROVIDER_STATE
        .with_label_values(&[provider_id, state])
        .set(1);
}

/// Drops the state series of every provider that is no longer in `live`. Done once a pass has
/// published its verdicts, not before: the gauge is never empty while a pass is in progress (a
/// slow or failing provider is exactly when an alert on its state must keep seeing it).
fn forget_providers_not_in(live: &[ProviderFull]) {
    use mm_fleet::metrics::PROVIDER_STATE;
    use prometheus::core::Collector;
    let live: std::collections::HashSet<&str> = live.iter().map(|p| p.row.id.as_str()).collect();
    let label = |m: &prometheus::proto::Metric, name: &str| {
        m.get_label()
            .iter()
            .find(|l| l.get_name() == name)
            .map(|l| l.get_value().to_string())
    };
    for family in PROVIDER_STATE.collect() {
        for m in family.get_metric() {
            if let (Some(provider), Some(state)) = (label(m, "provider"), label(m, "state"))
                && !live.contains(provider.as_str())
            {
                let _ = PROVIDER_STATE.remove_label_values(&[&provider, &state]);
            }
        }
    }
}

/// Checks every live provider and records the verdicts. Returns how many were checked.
/// `stand_in` is always `None` outside a test: only the `test-support` feature can make one.
pub async fn checks_once(
    pool: &PgPool,
    kp: &Keypair,
    stand_in: Option<StandIn<'_>>,
) -> sqlx::Result<usize> {
    let providers = pdb::list(pool).await?;
    for p in &providers {
        check_provider(pool, kp, p, stand_in).await?;
    }
    forget_providers_not_in(&providers);
    Ok(providers.len())
}

/// Claims and answers the oldest queued Test connection, if any. Returns its id. Other kinds
/// of request have their own claim: this loop never takes one.
pub async fn requests_once(
    pool: &PgPool,
    kp: &Keypair,
    stand_in: Option<StandIn<'_>>,
) -> sqlx::Result<Option<String>> {
    let expired = rq::expire_stale(pool).await?;
    if expired > 0 {
        tracing::warn!(expired, "requests expired unanswered");
        mm_fleet::metrics::REQUESTS_EXPIRED.inc_by(expired);
    }
    let Some(req) = rq::claim_next(pool, "test_connection").await? else {
        return Ok(None);
    };
    match req.kind.as_str() {
        "test_connection" => match pdb::get(pool, &req.provider_id).await? {
            Some(p) => match check_provider(pool, kp, &p, stand_in).await? {
                Some(Checked { row, stored }) => {
                    // "unknown" means the check could not say: the operator's button press did
                    // not verify anything, so it is not a success.
                    let ok = row.state != "unknown";
                    // A successful Test connection lifts the provider's quota holds (C6): the
                    // operator is saying the account was fixed. Only a verdict that was
                    // written counts; one dropped because the provider was deleted meanwhile
                    // says nothing about an account that no longer exists here. Only the holds
                    // set before the check began go: the rent loop may have recorded a quota
                    // refusal while it ran, and the check says nothing about that one.
                    if stored && row.state == "ok" {
                        placement_db::clear_quota_holds(pool, &p.row.id, row.checked_at).await?;
                    }
                    let result = serde_json::to_value(&row).unwrap_or(json!({}));
                    rq::finish(pool, &req.id, ok, result).await?;
                }
                None => {
                    // The check judged a token that was replaced while it ran; it says nothing
                    // about the stored one.
                    let result = json!({"error": "the token was replaced during the check; run the test again"});
                    rq::finish(pool, &req.id, false, result).await?;
                }
            },
            None => {
                let result = json!({"error": "provider no longer exists"});
                rq::finish(pool, &req.id, false, result).await?;
            }
        },
        other => {
            let result = json!({"error": format!("{other} is not supported in P-A")});
            rq::finish(pool, &req.id, false, result).await?;
        }
    }
    Ok(Some(req.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_refused_endpoint_is_the_operators_problem_but_a_failed_lookup_is_not() {
        assert_eq!(
            endpoint_verdict(EndpointError::Forbidden),
            ("needs_you", "permanent")
        );
        assert_eq!(
            endpoint_verdict(EndpointError::Invalid),
            ("needs_you", "permanent")
        );
        assert_eq!(
            endpoint_verdict(EndpointError::Unresolved),
            ("unknown", "transient")
        );
    }

    #[tokio::test]
    async fn supervise_restarts_a_panicking_loop_and_stops_on_cancel() {
        let cancel = CancellationToken::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let (counter, token) = (runs.clone(), cancel.clone());
        let h = supervise("test", cancel.clone(), move || {
            let (counter, token) = (counter.clone(), token.clone());
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("first run dies");
                }
                token.cancelled().await;
            }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while runs.load(Ordering::SeqCst) < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the loop was not restarted"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .expect("supervise stops after cancel")
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }
}
