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
//! [`sweep_relay_hours`] is the DB half, built on gpu_meter's period logic: windows
//! are anchored to each node's own `billing_started_at` and advanced by its
//! `last_metered_at` meter clock ([`relay_windows`], mirroring
//! [`crate::gpu_meter::gpu_interval`]'s clamps), never wall-clock aligned; gone
//! nodes are caught up to `gone_at` — final partial window included — exactly like
//! `transcode_nodes_to_meter`'s gone clause. Per window it writes one `relay_hour`
//! usage row per slice under idempotency key
//! `lease:{node}:{period_start_unix}:{slice index}` — the node/period pair is exactly
//! what must never double-bill; the slice index exists only because one period can
//! carry more than one payer — and deletes the samples the window consumed (V052's
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

/// A fan-out node as the relay-hour meter reads it — the same clock fields
/// [`crate::gpu_meter::gpu_interval`] reads off a transcoder, because the two meters
/// share `mm_fleet_nodes`' `billing_started_at` / `last_metered_at` / `gone_at`
/// columns (a node is one flavor for life, so the clocks never collide).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutNodeToMeter {
    pub mm_node_id: String,
    pub kind: String,
    pub billing_started_at: Option<DateTime<Utc>>,
    pub last_metered_at: Option<DateTime<Utc>>,
    pub gone_at: Option<DateTime<Utc>>,
}

/// One billable window for a node: `[start, end)`, where `end - start` is a whole
/// period except for a gone node's final partial window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// The windows to bill for `node` at `now` — gpu_meter's period logic, per the
/// brief: periods are anchored to the node's OWN `billing_started_at` (a node
/// created 12:17 on an hourly provider bills 12:17–13:17 windows), never to
/// wall-clock epoch alignment, and the meter clock (`last_metered_at`, clamped up
/// to `billing_started_at` like `gpu_interval` does) says where the next window
/// starts. Several windows come back when the sweep was down for longer than one
/// period — each is its own usage row under its own `period_start_unix`.
///
/// A gone node is metered to `gone_at` (RF-1's clamp: a `gone_at` behind the clock
/// is nothing, never a negative window), including a final PARTIAL window — the
/// machine billed until it was destroyed, not until the period boundary it never
/// reached. `None`-`billing_started_at` nodes (no machine ever) bill nothing.
pub fn relay_windows(node: &FanoutNodeToMeter, now: DateTime<Utc>) -> Vec<RelayWindow> {
    let Some(started) = node.billing_started_at else {
        return Vec::new();
    };
    let period = chrono::Duration::seconds(period_seconds(&node.kind));
    let mut cursor = node.last_metered_at.map_or(started, |l| l.max(started));
    let hard_end = node.gone_at.map_or(now, |g| g.min(now));

    let mut windows = Vec::new();
    loop {
        let full_end = cursor + period;
        if full_end <= hard_end {
            windows.push(RelayWindow {
                start: cursor,
                end: full_end,
            });
            cursor = full_end;
        } else if node.gone_at.is_some() && cursor < hard_end {
            // The final partial window of a gone node: billed to the instant the
            // machine stopped billing us, then never again (the clock advances to
            // gone_at, and gone_at <= last_metered_at yields nothing next sweep).
            windows.push(RelayWindow {
                start: cursor,
                end: hard_end,
            });
            break;
        } else {
            // The current window is still running (or there is nothing left): wait.
            break;
        }
    }
    windows
}

/// What one relay-hour sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelaySweep {
    pub events_written: usize,
    pub errors: Vec<(String, String)>,
}

/// Meter every completed window per fan-out node — gpu_meter's sweep shape, with
/// windows instead of a single open interval.
///
/// Per node: compute the windows past its meter clock ([`relay_windows`]), and for
/// each one pull the samples that landed in it, apportion them, write one
/// `relay_hour` usage row per slice, delete the consumed samples, and advance
/// `last_metered_at` to the window's end. Idempotent per (node, period): the key is
/// `lease:{node}:{period_start_unix}` (plus a slice index, since one period can
/// carry several payers), so a replayed sweep's inserts are no-ops.
///
/// Gone nodes are included while their `gone_at` is past the meter clock — the same
/// catch-up clause as `transcode_nodes_to_meter` — so a node destroyed mid-period
/// still has its final partial window metered and its samples cleaned up, rather
/// than leaking both forever.
///
/// One node's failure does not stop the others (FR-305f's rule, same as the egress
/// and GPU sweeps).
pub async fn sweep_relay_hours(pool: &PgPool, meter: &PgMeteringDb, now: DateTime<Utc>) -> RelaySweep {
    let mut sweep = RelaySweep::default();

    type NodeRow = (
        String,
        String,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    );
    let rows: Vec<NodeRow> = match sqlx::query_as(
        // The gone-node clause mirrors `transcode_nodes_to_meter` (RF-2 included: a
        // gone row with no gone_at is left alone — metering it to now() would bill
        // a machine that stopped at an unknown time).
        "SELECT mm_node_id, provider, billing_started_at, last_metered_at, gone_at
           FROM mm_fleet_nodes
          WHERE flavor = 'fanout'
            AND billing_started_at IS NOT NULL
            AND (state <> 'gone'
                 OR (gone_at IS NOT NULL
                     AND (last_metered_at IS NULL OR gone_at > last_metered_at)))
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

    for (mm_node_id, kind, billing_started_at, last_metered_at, gone_at) in rows {
        let node = FanoutNodeToMeter {
            mm_node_id,
            kind,
            billing_started_at,
            last_metered_at,
            gone_at,
        };
        if let Err(e) = meter_one_node(pool, meter, &node, now, &mut sweep).await {
            sweep.errors.push((node.mm_node_id.clone(), e.to_string()));
        }
    }

    sweep
}

async fn meter_one_node(
    pool: &PgPool,
    meter: &PgMeteringDb,
    node: &FanoutNodeToMeter,
    now: DateTime<Utc>,
    sweep: &mut RelaySweep,
) -> Result<(), MeteringDbError> {
    let node_id = node.mm_node_id.as_str();
    for window in relay_windows(node, now) {
        meter_one_window(pool, meter, node_id, window, sweep).await?;
        // Advance the meter clock only after the window's rows and sample delete
        // landed, so a failure re-derives the same window next sweep (and the
        // idempotency keys de-duplicate whatever did land).
        sqlx::query("UPDATE mm_fleet_nodes SET last_metered_at = $2 WHERE mm_node_id = $1")
            .bind(node_id)
            .bind(window.end)
            .execute(pool)
            .await
            .map_err(|e| MeteringDbError::Db(e.to_string()))?;
    }
    Ok(())
}

async fn meter_one_window(
    pool: &PgPool,
    meter: &PgMeteringDb,
    node_id: &str,
    window: RelayWindow,
    sweep: &mut RelaySweep,
) -> Result<(), MeteringDbError> {
    let window_milli_hours = milli_hours((window.end - window.start).num_milliseconds());

    let rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT broadcast_id, viewers FROM mm_relay_usage_samples
          WHERE mm_node_id = $1 AND sampled_at >= $2 AND sampled_at < $3",
    )
    .bind(node_id)
    .bind(window.start)
    .bind(window.end)
    .fetch_all(pool)
    .await
    .map_err(|e| MeteringDbError::Db(e.to_string()))?;

    let samples: Vec<(String, i64)> = rows
        .into_iter()
        .map(|(bc, viewers)| (bc, i64::from(viewers).max(0)))
        .collect();

    // The distinct broadcasts in FIRST-SEEN order — the exact order `apportion`
    // returns its slices in, so `slices[i]` belongs to `distinct[i]`. (Indexing the
    // raw sample rows here would mis-attribute slices the moment one broadcast has
    // two sample rows in the window.)
    let mut distinct: Vec<String> = Vec::new();
    for (bc, _) in &samples {
        if !distinct.contains(bc) {
            distinct.push(bc.clone());
        }
    }

    // Per broadcast: who pays, and — for wallet slices — whose wallet (the host).
    let mut funding: HashMap<String, Payer> = HashMap::new();
    let mut hosts: HashMap<String, String> = HashMap::new();
    for bc in &distinct {
        match meter.payer_for_broadcast(bc).await? {
            Some(p) if p.wallet_currency.is_some() => {
                funding.insert(bc.clone(), Payer::Wallet);
                hosts.insert(bc.clone(), p.host_user_id);
            }
            // No stream row, or a wallet-less host (Open tier, or a lease serving no
            // broadcast of ours at all): R4's uncovered-broadcast rule.
            _ => {
                funding.insert(bc.clone(), Payer::House);
            }
        }
    }

    let slices = apportion(&samples, &funding);
    let period_start_unix = window.start.timestamp();

    for (idx, slice) in slices.iter().enumerate() {
        let quantity_milli = window_milli_hours * slice.share_milli / 1000;
        let idempotency_key = format!("lease:{node_id}:{period_start_unix}:{idx}");
        // House-slice identity convention (one shape, both House cases): `user_id`
        // is always the literal "house" — House rows are nobody's bill, and a fixed
        // word keeps them countable as one line in any per-user report. The
        // `broadcast_id` stays REAL whenever one exists (an uncovered broadcast's
        // slice names the broadcast whose viewers drew it, for disputes); only the
        // zero-sample whole-period slice has no broadcast at all, and there it is
        // the node id — also real, also unambiguous, and it cannot collide with a
        // broadcast id because node ids carry the `bc-…-fanout-…` / lease shape.
        let broadcast_id = distinct
            .get(idx)
            .cloned()
            .unwrap_or_else(|| node_id.to_string());
        let user_id = match slice.payer {
            Payer::House => "house".to_string(),
            _ => hosts.get(&broadcast_id).cloned().unwrap_or_else(|| "house".to_string()),
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
                occurred_at: window.end,
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
    // window that is now closed) has done its job.
    sqlx::query(
        "DELETE FROM mm_relay_usage_samples
          WHERE mm_node_id = $1 AND sampled_at >= $2 AND sampled_at < $3",
    )
    .bind(node_id)
    .bind(window.start)
    .bind(window.end)
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

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(s, 0).unwrap()
    }

    fn node(
        kind: &str,
        started: Option<i64>,
        last: Option<i64>,
        gone: Option<i64>,
    ) -> FanoutNodeToMeter {
        FanoutNodeToMeter {
            mm_node_id: "bc-b1-fanout-0".into(),
            kind: kind.into(),
            billing_started_at: started.map(t),
            last_metered_at: last.map(t),
            gone_at: gone.map(t),
        }
    }

    /// Windows anchor to the node's own billing start, not wall-clock alignment: a
    /// node created at 12:17 on an hourly provider bills 12:17–13:17 windows.
    #[test]
    fn windows_anchor_to_billing_start_not_the_wall_clock() {
        let started = 3_600 * 12 + 17 * 60; // 12:17
        let n = node("hetzner", Some(started), None, None);
        // 13:20: the 12:17–13:17 window is complete, 13:17–14:17 is still running.
        let w = relay_windows(&n, t(3_600 * 13 + 20 * 60));
        assert_eq!(
            w,
            vec![RelayWindow {
                start: t(started),
                end: t(started + 3_600)
            }]
        );
        // 13:10: the first window has not completed yet — nothing to bill.
        assert_eq!(relay_windows(&n, t(3_600 * 13 + 10 * 60)), Vec::new());
    }

    #[test]
    fn the_next_window_starts_at_the_meter_clock() {
        let n = node("scaleway", Some(0), Some(120), None);
        let w = relay_windows(&n, t(185));
        assert_eq!(
            w,
            vec![RelayWindow {
                start: t(120),
                end: t(180)
            }]
        );
    }

    /// A stalled sweep catches up: every completed window since the clock, each its
    /// own period.
    #[test]
    fn a_stalled_sweep_yields_every_completed_window() {
        let n = node("scaleway", Some(0), None, None);
        let w = relay_windows(&n, t(185));
        assert_eq!(w.len(), 3);
        assert_eq!((w[0].start, w[0].end), (t(0), t(60)));
        assert_eq!((w[2].start, w[2].end), (t(120), t(180)));
    }

    /// A gone node's final PARTIAL window is billed to gone_at — and once the clock
    /// has advanced there, the node yields nothing ever again.
    #[test]
    fn a_gone_node_gets_a_final_partial_window_and_then_nothing() {
        let n = node("hetzner", Some(0), Some(3_600), Some(3_600 + 600));
        let w = relay_windows(&n, t(7_200));
        assert_eq!(
            w,
            vec![RelayWindow {
                start: t(3_600),
                end: t(3_600 + 600)
            }]
        );
        let done = node("hetzner", Some(0), Some(3_600 + 600), Some(3_600 + 600));
        assert_eq!(relay_windows(&done, t(7_200)), Vec::new());
    }

    /// RF-1's clamp: gone_at behind the clock is nothing, never a negative window.
    #[test]
    fn gone_at_behind_the_clock_yields_nothing() {
        let n = node("hetzner", Some(0), Some(7_200), Some(3_600));
        assert_eq!(relay_windows(&n, t(10_000)), Vec::new());
    }

    #[test]
    fn a_node_with_no_machine_bills_nothing() {
        assert_eq!(relay_windows(&node("hetzner", None, None, None), t(7_200)), Vec::new());
    }
}
