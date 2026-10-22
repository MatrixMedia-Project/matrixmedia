//! Relay-hour metering with viewer-share apportionment (spec §8.2 R4).
//!
//! A fan-out relay is rented whole, but it almost never serves one broadcast alone —
//! several broadcasts can share it, each with a different audience and a different
//! payer. The relay-hour meter bills the *node's* hour (or minute, on providers with
//! no hourly SKU — [`crate::providers_db::billing_clock`]) but splits that bill across
//! the broadcasts it served, in proportion to how many viewer-samples each one drew
//! during the period, via [`apportion`]. That is the "viewer-share" in the module name.
//!
//! [`apportion`] is pure and the one piece with a worked example in the spec (§8.2):
//! viewers 60/30/10 split a hundred-viewer period into exactly 600/300/100 milli (a
//! thousandth of the node's billed unit each). A broadcast with no wallet to charge —
//! no `mm_broadcaster_wallet` row, same signal the egress meter uses
//! (`PgMeteringDb::payer_for_broadcast`) — gets `Payer::House` instead of refusing to
//! split or silently dropping its share. A period with no samples at all is not
//! nothing: the relay existed and billed, nobody watched it, so the whole period is
//! one `Payer::House` slice and `apportion` warns — the zero-sample case is the one
//! shape a silent empty `Vec` would hide a real cost inside.
//!
//! [`sweep_relay_hours`] is the DB half: for every live fan-out node, meter the one
//! period that just completed (same "one sweep, one interval" shape as
//! [`crate::gpu_meter::sweep_gpu_minutes`], just periods instead of a running clock),
//! write one `relay_hour` usage row per slice under idempotency key
//! `lease:{node}:{period_start_unix}:{slice index}` — the node/period pair is exactly
//! what must never double-bill; the slice index exists only because one period can
//! carry more than one payer — and delete the samples that period consumed (V052's
//! retention note: a sample that has been metered has done its job).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use mm_core::fleet::funding::Payer;
use mm_db::metering_db::{MeteringDbError, PgMeteringDb, UsageRow};
use sqlx::PgPool;

/// One payer's piece of a period's bill. Shares across every slice of one period
/// sum to exactly 1000 (milli = a thousandth of the billed unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    pub payer: Payer,
    pub share_milli: i64,
}

/// Split a period's bill across whichever broadcasts drew viewer-samples on the
/// node, by largest-remainder rounding (spec §8.2 R4): shares sum to exactly 1000
/// milli, not "close to" — a billed node's hour has to land somewhere in full.
///
/// `samples` is `(broadcast_id, weight)` pairs — `weight` need only be proportional
/// to viewer-seconds (an equal-interval sample count works, because apportionment is
/// a ratio); a broadcast appearing twice has its weights summed first. `funding`
/// supplies each broadcast's payer; a broadcast with no entry — no wallet to bill —
/// gets [`Payer::House`] (R4's uncovered-broadcast rule). One [`Slice`] is returned
/// per distinct broadcast, in the order it first appears in `samples`.
///
/// Zero broadcasts, or every weight zero (nothing watched this period, but the node
/// still billed it): a single `Slice { payer: House, share_milli: 1000 }`, with a
/// warning — the period is not nothing, it is an unattributed cost, and that must be
/// visible rather than silently producing an empty `Vec`.
pub fn apportion(
    samples: &[(String, i64)],
    funding: &HashMap<String, Payer>,
) -> Vec<Slice> {
    // Aggregate duplicate broadcasts first, preserving first-seen order — the order
    // the slices come back in, so callers (and the worked example) see a stable shape.
    let mut order: Vec<String> = Vec::new();
    let mut weight: HashMap<String, i64> = HashMap::new();
    for (bc, w) in samples {
        if !weight.contains_key(bc) {
            order.push(bc.clone());
        }
        *weight.entry(bc.clone()).or_insert(0) += w;
    }

    let total: i64 = order.iter().map(|bc| weight[bc]).sum();
    if total <= 0 {
        tracing::warn!(
            "relay-hour meter: a period had zero viewer-samples — billing the whole \
             period to the house, because a relay existed and billed regardless of \
             whether anyone watched it"
        );
        return vec![Slice {
            payer: Payer::House,
            share_milli: 1000,
        }];
    }

    // Largest-remainder rounding: floor every share, then hand the leftover milli
    // (at most one per broadcast, since floor(x/total*1000) undershoots by less
    // than one unit) to the broadcasts with the largest remainders, ties broken by
    // first-seen order — deterministic, not "whichever the sort happened to put
    // first".
    let mut floors: Vec<i64> = Vec::with_capacity(order.len());
    let mut remainders: Vec<i64> = Vec::with_capacity(order.len());
    for bc in &order {
        let w = weight[bc];
        floors.push(w * 1000 / total);
        remainders.push(w * 1000 % total);
    }
    let mut deficit = 1000 - floors.iter().sum::<i64>();
    let mut by_remainder: Vec<usize> = (0..order.len()).collect();
    by_remainder.sort_by(|&a, &b| remainders[b].cmp(&remainders[a]));
    for idx in by_remainder {
        if deficit <= 0 {
            break;
        }
        floors[idx] += 1;
        deficit -= 1;
    }

    order
        .iter()
        .zip(floors)
        .map(|(bc, share_milli)| Slice {
            payer: funding.get(bc).copied().unwrap_or(Payer::House),
            share_milli,
        })
        .collect()
}

/// Thousandths of an hour in `ms` milliseconds, rounded half-up — the relay-hour
/// analogue of [`crate::gpu_meter::milli_minutes`], scaled to hours because the
/// billed unit here is always `relay_hour` even on a minute-quantized provider; the
/// quantization only changes how often a period closes, not what it is denominated
/// in.
fn milli_hours(ms: i64) -> i64 {
    if ms <= 0 {
        return 0;
    }
    ms.saturating_mul(1).saturating_add(1_800) / 3_600
}

/// How long one billing period is, per [`crate::providers_db::billing_clock`]:
/// an hour on providers with no per-minute SKU, a minute everywhere else.
fn period_seconds(kind: &str) -> i64 {
    if crate::providers_db::billing_clock(kind) == "hour" {
        3_600
    } else {
        60
    }
}

/// The most recently COMPLETED period as of `now`: `now` sits somewhere inside the
/// next period, which is still running and has nothing to meter yet.
fn last_completed_period(now: DateTime<Utc>, period_secs: i64) -> (DateTime<Utc>, DateTime<Utc>) {
    let now_unix = now.timestamp();
    let aligned = (now_unix.div_euclid(period_secs)) * period_secs;
    let start = aligned - period_secs;
    (
        DateTime::<Utc>::from_timestamp(start, 0).expect("in range"),
        DateTime::<Utc>::from_timestamp(aligned, 0).expect("in range"),
    )
}

/// What one relay-hour sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelaySweep {
    pub events_written: usize,
    pub errors: Vec<(String, String)>,
}

/// Meter the one period that just completed, for every live fan-out node.
///
/// Per node: read the node's `kind` to pick its period length, pull the samples
/// that landed in that period, apportion them, write one `relay_hour` usage row per
/// slice, and delete the consumed samples. Idempotent per (node, period): a replay
/// at the same `now` re-derives the same period and the same slices, and the usage
/// table's idempotency key makes the inserts no-ops.
///
/// One node's failure does not stop the others (FR-305f's rule, same as the egress
/// and GPU sweeps).
pub async fn sweep_relay_hours(pool: &PgPool, meter: &PgMeteringDb, now: DateTime<Utc>) -> RelaySweep {
    let mut sweep = RelaySweep::default();

    let nodes: Vec<(String, String)> = match sqlx::query_as(
        "SELECT mm_node_id, provider FROM mm_fleet_nodes
          WHERE flavor = 'fanout' AND state <> 'gone'
          ORDER BY mm_node_id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            sweep
                .errors
                .push(("*".into(), format!("listing fan-out nodes failed: {e}")));
            return sweep;
        }
    };

    for (node_id, kind) in nodes {
        if let Err(e) = meter_one_node(pool, meter, &node_id, &kind, now, &mut sweep).await {
            sweep.errors.push((node_id, e.to_string()));
        }
    }

    sweep
}

async fn meter_one_node(
    pool: &PgPool,
    meter: &PgMeteringDb,
    node_id: &str,
    kind: &str,
    now: DateTime<Utc>,
    sweep: &mut RelaySweep,
) -> Result<(), MeteringDbError> {
    let period_secs = period_seconds(kind);
    let (start, end) = last_completed_period(now, period_secs);
    let period_ms = (end - start).num_milliseconds();
    let period_milli_hours = milli_hours(period_ms);

    let rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT broadcast_id, viewers FROM mm_relay_usage_samples
          WHERE mm_node_id = $1 AND sampled_at >= $2 AND sampled_at < $3",
    )
    .bind(node_id)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .map_err(|e| MeteringDbError::Db(e.to_string()))?;

    let samples: Vec<(String, i64)> = rows
        .into_iter()
        .map(|(bc, viewers)| (bc, i64::from(viewers).max(0)))
        .collect();

    let mut broadcasts: Vec<&str> = samples.iter().map(|(bc, _)| bc.as_str()).collect();
    broadcasts.sort_unstable();
    broadcasts.dedup();

    let mut funding = HashMap::new();
    for bc in &broadcasts {
        let payer = match meter.payer_for_broadcast(bc).await? {
            Some(p) if p.wallet_currency.is_some() => Payer::Wallet,
            // No row, or a wallet-less broadcast (Open tier, or a lease with no
            // broadcast behind it at all): R4's uncovered-broadcast rule.
            _ => Payer::House,
        };
        funding.insert((*bc).to_string(), payer);
    }

    let slices = apportion(&samples, &funding);
    let period_start_unix = start.timestamp();

    for (idx, slice) in slices.iter().enumerate() {
        let quantity_milli = period_milli_hours * slice.share_milli / 1000;
        let idempotency_key = format!("lease:{node_id}:{period_start_unix}:{idx}");
        // A House slice from the zero-sample case has no broadcast to attribute to;
        // the node id keeps it from colliding with another node's house row.
        let broadcast_id = samples
            .get(idx)
            .map(|(bc, _)| bc.clone())
            .unwrap_or_else(|| format!("house-{node_id}"));
        let user_id = if slice.payer == Payer::House {
            "house".to_string()
        } else {
            broadcast_id.clone()
        };
        let mut tx = pool.begin().await.map_err(|e| MeteringDbError::Db(e.to_string()))?;
        let written = PgMeteringDb::insert_usage_in(
            &mut tx,
            &UsageRow {
                user_id: &user_id,
                broadcast_id: &broadcast_id,
                unit: "relay_hour",
                quantity_milli,
                mm_node_id: node_id,
                idempotency_key: &idempotency_key,
                occurred_at: end,
                payer: slice.payer,
                egress_epoch: None,
                cumulative_bytes: None,
            },
        )
        .await?;
        tx.commit().await.map_err(|e| MeteringDbError::Db(e.to_string()))?;
        if written {
            sweep.events_written += 1;
        }
    }

    // Retention (V052): a sample that was metered (or belonged to a zero-sample
    // period that is now closed) has done its job.
    sqlx::query(
        "DELETE FROM mm_relay_usage_samples
          WHERE mm_node_id = $1 AND sampled_at >= $2 AND sampled_at < $3",
    )
    .bind(node_id)
    .bind(start)
    .bind(end)
    .execute(pool)
    .await
    .map_err(|e| MeteringDbError::Db(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn funding(pairs: &[(&str, Payer)]) -> HashMap<String, Payer> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// The spec's worked example (§8.2 R4): 60/30/10 viewers split a hundred-viewer
    /// period into EXACT 600/300/100 milli — no rounding needed, since 100 divides
    /// 1000 cleanly. C has no wallet, so its share is House, not dropped.
    #[test]
    fn apportion_60_30_10_example() {
        let samples = vec![
            ("a".to_string(), 60),
            ("b".to_string(), 30),
            ("c".to_string(), 10),
        ];
        let funding = funding(&[("a", Payer::Wallet), ("b", Payer::Wallet)]);
        let slices = apportion(&samples, &funding);
        assert_eq!(
            slices,
            vec![
                Slice {
                    payer: Payer::Wallet,
                    share_milli: 600
                },
                Slice {
                    payer: Payer::Wallet,
                    share_milli: 300
                },
                Slice {
                    payer: Payer::House,
                    share_milli: 100
                },
            ]
        );
    }

    /// A split that does NOT divide evenly must still sum to exactly 1000 — the
    /// largest-remainder rounding's whole point.
    #[test]
    fn shares_sum_to_1000_with_rounding() {
        let samples = vec![
            ("a".to_string(), 1),
            ("b".to_string(), 1),
            ("c".to_string(), 1),
        ];
        let funding = funding(&[("a", Payer::Wallet), ("b", Payer::Wallet), ("c", Payer::Wallet)]);
        let slices = apportion(&samples, &funding);
        let sum: i64 = slices.iter().map(|s| s.share_milli).sum();
        assert_eq!(sum, 1000);
        // 1000/3 = 333.33..., so two land on 333 and one (the first, by the
        // deterministic tie-break) picks up the extra milli.
        let mut shares: Vec<i64> = slices.iter().map(|s| s.share_milli).collect();
        shares.sort_unstable();
        assert_eq!(shares, vec![333, 333, 334]);

        // A larger, uneven split too — not just the all-tied case above.
        let samples2 = vec![
            ("a".to_string(), 7),
            ("b".to_string(), 5),
            ("c".to_string(), 3),
        ];
        let slices2 = apportion(&samples2, &HashMap::new());
        let sum2: i64 = slices2.iter().map(|s| s.share_milli).sum();
        assert_eq!(sum2, 1000);
    }

    /// Zero samples for a whole period is not nothing: the relay existed and
    /// billed, so the whole period goes to the house as one slice, not an empty
    /// `Vec` that would make the cost disappear.
    #[test]
    fn zero_samples_whole_hour_house() {
        let slices = apportion(&[], &HashMap::new());
        assert_eq!(
            slices,
            vec![Slice {
                payer: Payer::House,
                share_milli: 1000
            }]
        );
    }

    #[test]
    fn a_broadcast_appearing_twice_has_its_weight_summed() {
        let samples = vec![
            ("a".to_string(), 5),
            ("b".to_string(), 5),
            ("a".to_string(), 5),
        ];
        let funding = funding(&[("a", Payer::Wallet), ("b", Payer::Wallet)]);
        let slices = apportion(&samples, &funding);
        // "a" totals 10 against "b"'s 5: a 2:1 split, in first-seen order.
        assert_eq!(
            slices,
            vec![
                Slice {
                    payer: Payer::Wallet,
                    share_milli: 667
                },
                Slice {
                    payer: Payer::Wallet,
                    share_milli: 333
                },
            ]
        );
    }

    #[test]
    fn hour_and_minute_periods_quantize_as_billing_clock_says() {
        assert_eq!(period_seconds("hetzner"), 3_600);
        assert_eq!(period_seconds("akamai"), 3_600);
        assert_eq!(period_seconds("scaleway"), 60);
    }

    #[test]
    fn milli_hours_rounds_half_up() {
        assert_eq!(milli_hours(3_600_000), 1000, "one hour");
        assert_eq!(milli_hours(60_000), 17, "one minute = 16.67 milli-hours");
        assert_eq!(milli_hours(0), 0);
        assert_eq!(milli_hours(-1), 0);
    }

    #[test]
    fn the_last_completed_period_is_the_one_before_now() {
        let t = |s: i64| DateTime::<Utc>::from_timestamp(s, 0).unwrap();
        // now = 12:17:00 (unix 44220), hour periods -> last completed is 11:00-12:00.
        let now = t(3_600 * 12 + 17 * 60);
        let (start, end) = last_completed_period(now, 3_600);
        assert_eq!(start, t(3_600 * 11));
        assert_eq!(end, t(3_600 * 12));
    }
}
