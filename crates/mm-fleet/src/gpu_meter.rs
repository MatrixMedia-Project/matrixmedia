//! GPU-minute metering (GPU funding design §4.2).
//!
//! A transcoder bills from the moment the provider returned a handle
//! (`billing_started_at`) until the teardown completed (`gone_at`). Each meter sweep
//! writes one `gpu_minute` usage row per node for the time since the last sweep,
//! carrying the node's payer. The quantity is thousandths of a minute, rounded
//! half-up like every other charge (FR-307c). Non-wallet payers are written already
//! rated (see `PgMeteringDb::insert_usage_in`).
//!
//! Why `billing_started_at` and not `requested_at`: a create that fails and retries,
//! or a node parked in a quota cooldown, has a row and no machine. Billing it would
//! charge a wallet — or the ad budget — for minutes the provider never billed us.

use chrono::{DateTime, Utc};
use mm_core::fleet::funding::{Funding, Payer};
use mm_db::metering_db::{GpuUsage, PgMeteringDb, TranscodeNodeToMeter};

/// One node's billable span since the last sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInterval {
    pub mm_node_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Thousandths of a GPU-minute, half-up.
    pub quantity_milli: i64,
    pub funding: Funding,
    pub idempotency_key: String,
}

/// Thousandths of a minute in `secs` seconds, rounded half-up.
pub fn milli_minutes(secs: i64) -> i64 {
    if secs <= 0 {
        return 0;
    }
    (secs.saturating_mul(1000).saturating_add(30)) / 60
}

/// The span to bill for `node` at `now`, or `None` when there is nothing to bill:
/// no `billing_started_at` (no machine), or an end at or before the start (already
/// metered to its end, or a `gone_at` behind the last sweep — RF-1).
pub fn gpu_interval(node: &TranscodeNodeToMeter, now: DateTime<Utc>) -> Option<GpuInterval> {
    let started = node.billing_started_at?;
    let start = node.last_metered_at.map_or(started, |l| l.max(started));
    let end = node.gone_at.map_or(now, |g| g.min(now));
    if end <= start {
        return None;
    }
    let secs = (end - start).num_seconds();
    Some(GpuInterval {
        mm_node_id: node.mm_node_id.clone(),
        start,
        end,
        quantity_milli: milli_minutes(secs),
        funding: node.funding,
        idempotency_key: format!("gpu:{}:{}", node.mm_node_id, start.timestamp()),
    })
}

/// What one GPU sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GpuSweep {
    pub events_written: usize,
    /// (payer, thousandths of a minute) newly written, for the metric.
    pub metered_milli: Vec<(Payer, i64)>,
    pub errors: Vec<(String, String)>,
}

/// Meter every transcode node that has billed since the last sweep.
///
/// Per node: compute the interval, write the usage row and advance
/// `last_metered_at` in one transaction. One node's failure does not stop the others
/// (the same rule as the egress sweep, FR-305f).
pub async fn sweep_gpu_minutes(db: &PgMeteringDb, now: DateTime<Utc>) -> GpuSweep {
    let mut sweep = GpuSweep::default();
    let nodes = match db.transcode_nodes_to_meter().await {
        Ok(n) => n,
        Err(e) => {
            sweep
                .errors
                .push(("*".into(), format!("listing transcode nodes failed: {e}")));
            return sweep;
        }
    };
    for node in nodes {
        let Some(iv) = gpu_interval(&node, now) else {
            continue;
        };
        let (user_id, broadcast_id) = match (&node.host_user_id, &node.broadcast_id) {
            (Some(host), Some(bc)) => (host.clone(), bc.clone()),
            // A test boot or operator node serves no broadcast: the operator who
            // asked for it is the "user", or a fixed word when nobody is recorded.
            _ => (
                node.created_by
                    .clone()
                    .unwrap_or_else(|| "operator".to_string()),
                node.broadcast_id
                    .clone()
                    .unwrap_or_else(|| node.mm_node_id.clone()),
            ),
        };
        let usage = GpuUsage {
            mm_node_id: &iv.mm_node_id,
            user_id: &user_id,
            broadcast_id: &broadcast_id,
            quantity_milli: iv.quantity_milli,
            payer: Payer::from(iv.funding),
            idempotency_key: &iv.idempotency_key,
            occurred_at: iv.end,
            metered_to: iv.end,
        };
        match db.record_gpu_interval(&usage).await {
            Ok(true) => {
                sweep.events_written += 1;
                sweep.metered_milli.push((usage.payer, iv.quantity_milli));
            }
            Ok(false) => {} // replay: the row existed; last_metered_at still advanced
            Err(e) => sweep.errors.push((iv.mm_node_id.clone(), e.to_string())),
        }
    }
    sweep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(started: Option<i64>, last: Option<i64>, gone: Option<i64>) -> TranscodeNodeToMeter {
        let t = |s: i64| DateTime::<Utc>::from_timestamp(1_700_000_000 + s, 0).unwrap();
        TranscodeNodeToMeter {
            mm_node_id: "bc-b1-transcode-0".into(),
            funding: Funding::Wallet,
            broadcast_id: Some("b1".into()),
            host_user_id: Some("@h:hs".into()),
            created_by: None,
            billing_started_at: started.map(t),
            last_metered_at: last.map(t),
            gone_at: gone.map(t),
        }
    }
    fn at(s: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000 + s, 0).unwrap()
    }

    #[test]
    fn half_up_thousandths_of_a_minute() {
        assert_eq!(milli_minutes(60), 1000);
        assert_eq!(milli_minutes(90), 1500);
        assert_eq!(milli_minutes(1), 17, "1 s = 16.67 milli-minutes, half-up");
        assert_eq!(milli_minutes(0), 0);
        assert_eq!(milli_minutes(-5), 0);
    }

    #[test]
    fn the_first_interval_runs_from_billing_start() {
        let iv = gpu_interval(&node(Some(0), None, None), at(600)).unwrap();
        assert_eq!((iv.start, iv.end), (at(0), at(600)));
        assert_eq!(iv.quantity_milli, 10_000);
        assert_eq!(
            iv.idempotency_key,
            format!("gpu:bc-b1-transcode-0:{}", at(0).timestamp())
        );
    }

    #[test]
    fn the_next_interval_runs_from_the_last_sweep() {
        let iv = gpu_interval(&node(Some(0), Some(600), None), at(660)).unwrap();
        assert_eq!((iv.start, iv.end), (at(600), at(660)));
        assert_eq!(iv.quantity_milli, 1000);
    }

    #[test]
    fn a_node_with_no_machine_is_not_metered() {
        assert_eq!(gpu_interval(&node(None, None, None), at(600)), None);
    }

    #[test]
    fn a_gone_node_is_metered_to_gone_at_and_then_never_again() {
        let last = gpu_interval(&node(Some(0), Some(600), Some(630)), at(900)).unwrap();
        assert_eq!((last.start, last.end), (at(600), at(630)));
        assert_eq!(
            gpu_interval(&node(Some(0), Some(630), Some(630)), at(900)),
            None
        );
    }

    /// RF-1: gone_at behind the last sweep (skew, or a late teardown stamp) is nothing,
    /// never a negative quantity.
    #[test]
    fn gone_at_before_the_last_sweep_yields_nothing() {
        assert_eq!(
            gpu_interval(&node(Some(0), Some(700), Some(630)), at(900)),
            None
        );
    }

    #[test]
    fn a_last_sweep_before_billing_start_is_clamped_to_the_start() {
        let iv = gpu_interval(&node(Some(100), Some(50), None), at(160)).unwrap();
        assert_eq!(iv.start, at(100));
        assert_eq!(iv.quantity_milli, 1000);
    }
}
