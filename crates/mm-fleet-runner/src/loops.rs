//! The runner's loops (spec §6.2). Each is supervised: a panic or an unexpected return
//! restarts it with backoff, the same shape mm-server uses for the meter and ladder loops.
//!
//! In P-A the runner only reads from providers: it heartbeats, verifies each provider's
//! sealed token (read-only checks) and answers the dashboard's Test connection. It never
//! creates or destroys anything. The unsealed token lives only inside `evaluate`: a log
//! line carries a provider id, kind and state, and a status row carries the provider's own
//! status line, never the credential.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mm_fleet::adapters;
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

/// Spawns the three supervised loops and returns once `cancel` fires and they have stopped.
pub async fn run_forever(pool: PgPool, kp: Arc<Keypair>, cancel: CancellationToken) {
    let h = spawn_loop("heartbeat", &pool, &kp, &cancel, heartbeat_loop);
    let c = spawn_loop("checks", &pool, &kp, &cancel, checks_loop);
    let r = spawn_loop("requests", &pool, &kp, &cancel, requests_loop);
    cancel.cancelled().await;
    let _ = tokio::join!(h, c, r);
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

async fn heartbeat_loop(pool: PgPool, kp: Arc<Keypair>, cancel: CancellationToken) {
    let mut t = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = t.tick() => {
                if let Err(e) = heartbeat_once(&pool, &kp, VERSION).await {
                    tracing::error!(error = %e, "heartbeat failed");
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

/// One heartbeat: mode and settings revision the runner is acting on, the live providers'
/// last verdicts, and how many rented nodes still bill. Soft-deleted providers are not
/// listed: the detail comes from `providers_db::list`, not from the status table.
pub async fn heartbeat_once(pool: &PgPool, kp: &Keypair, version: &str) -> sqlx::Result<()> {
    let snap = runner_settings::read(pool).await?;
    let providers = pdb::list(pool).await?;
    let rented: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mm_fleet_nodes WHERE ownership = 'rented' AND state <> 'gone'",
    )
    .fetch_one(pool)
    .await?;
    let detail = json!({
        "providers": providers.iter().map(|p| json!({
            "id": p.row.id,
            "state": p.status.as_ref().map(|s| s.state.as_str()),
            "checked_at": p.status.as_ref().map(|s| s.checked_at),
            "last_error_kind": p.status.as_ref().and_then(|s| s.last_error_kind.as_deref()),
        })).collect::<Vec<_>>(),
        "rented_nodes": rented,
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
    base_override: Option<&str>,
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
    let checker = match adapters::checker_for(&p.row.kind, &pt, &p.zones, base_override).await {
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
async fn check_provider(
    pool: &PgPool,
    kp: &Keypair,
    p: &ProviderFull,
    base_override: Option<&str>,
) -> sqlx::Result<Option<Checked>> {
    let Some(row) = evaluate(pool, kp, p, base_override).await? else {
        tracing::debug!(provider = %p.row.id, "token replaced during its check; verdict dropped");
        return Ok(None);
    };
    let stored = pdb::upsert_status(pool, &row).await?;
    if !stored {
        tracing::debug!(provider = %p.row.id, "provider deleted during its check; verdict dropped");
    }
    tracing::info!(provider = %p.row.id, kind = %p.row.kind, state = %row.state, "provider checked");
    Ok(Some(Checked { row, stored }))
}

/// Checks every live provider and records the verdicts. Returns how many were checked.
pub async fn checks_once(
    pool: &PgPool,
    kp: &Keypair,
    base_override: Option<&str>,
) -> sqlx::Result<usize> {
    let providers = pdb::list(pool).await?;
    for p in &providers {
        check_provider(pool, kp, p, base_override).await?;
    }
    Ok(providers.len())
}

/// Claims and answers the oldest queued Test connection, if any. Returns its id. Other kinds
/// of request have their own claim: this loop never takes one.
pub async fn requests_once(
    pool: &PgPool,
    kp: &Keypair,
    base_override: Option<&str>,
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
            Some(p) => match check_provider(pool, kp, &p, base_override).await? {
                Some(Checked { row, stored }) => {
                    // "unknown" means the check could not say: the operator's button press did
                    // not verify anything, so it is not a success.
                    let ok = row.state != "unknown";
                    // A successful Test connection lifts the provider's quota holds (C6): the
                    // operator is saying the account was fixed. Only a verdict that was
                    // written counts; one dropped because the provider was deleted meanwhile
                    // says nothing about an account that no longer exists here.
                    if stored && row.state == "ok" {
                        placement_db::clear_quota_holds(pool, &p.row.id).await?;
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
