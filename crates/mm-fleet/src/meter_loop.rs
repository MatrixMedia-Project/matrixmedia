//! One turn of the meter: poll, record, then charge (WS-D, §17.5, FR-302a/b).
//!
//! `sweep_egress` polls and writes usage; `rate_pending` turns usage into wallet
//! charges. This is the thing that runs both on a timer, and it is a plain function
//! taking `now` so a test can drive two ticks without waiting sixty seconds.
//!
//! ## Metering and charging are separate switches
//!
//! Metering writes rows and moves no money, so it runs by default: the usage data is
//! how the rate card gets set from real numbers rather than guesses. Charging is off
//! until an operator sets `billing_enabled`, and it is deliberately **not** inferred
//! from "a rate card row exists" — a row can arrive from a seed file or a deployment
//! template, and money moving should require someone to say so.
//!
//! The consequence is a backlog: the rating queue is every unrated event ever
//! metered, so the tick that enables billing charges all of it at once. That is why
//! the tick counts the queue even when billing is off, logs it, and publishes
//! `mm_billing_unrated_events` — the size of that first bill should be a number on a
//! dashboard before it is a charge on someone's wallet.
//!
//! ## What this does NOT do
//!
//! Poll before a node is destroyed. The meter runs on its own interval and the
//! reaper runs on its own; usage delivered since the last poll dies with the node
//! (FR-305, §17.5's buffer-and-acknowledge — the epoch makes the loss visible and
//! bounded, not prevented). At a one-minute interval the exposure is under a minute
//! of one node's egress, which is why this is a known gap and not a blocker.

use chrono::{DateTime, Utc};
use mm_db::metering_db::PgMeteringDb;
use mm_db::wallet_db::PgWalletDb;

use crate::metering::{sweep_egress, MeteredNode, MeteringSweep};
use crate::rating::{rate_pending, RatingReport};

/// What one tick did.
#[derive(Debug, Default)]
pub struct MeterTick {
    pub sweep: MeteringSweep,
    /// `None` when billing is disabled — which is not the same as a pass that
    /// charged nothing, and the log says which.
    pub rating: Option<RatingReport>,
    /// Unrated events after the tick, or `None` if the count could not be read.
    pub unrated_backlog: Option<i64>,
    /// Non-fatal problems with the tick itself, as opposed to with a node.
    pub errors: Vec<String>,
}

/// Poll every node, record usage, and — if billing is enabled — charge for it.
///
/// Sweep first, then rate. The other order would rate a queue that does not yet
/// contain this interval's usage, delaying every charge by one tick for no benefit.
/// Nothing here aborts on one failure: an unreachable node, an unpriceable event and
/// an empty wallet are all per-item outcomes, because a fleet-wide abort means one
/// bad node costs the revenue of every other one (FR-305f).
pub async fn meter_tick(
    meter: &PgMeteringDb,
    wallet: &PgWalletDb,
    nodes: &[MeteredNode],
    billing_enabled: bool,
    rating_batch: i64,
    now: DateTime<Utc>,
) -> MeterTick {
    let mut tick = MeterTick {
        sweep: sweep_egress(meter, nodes, now).await,
        ..Default::default()
    };

    if billing_enabled {
        match rate_pending(meter, wallet, rating_batch, now).await {
            Ok(report) => tick.rating = Some(report),
            // Rating failing must not lose the sweep's result: the usage is already
            // durably written, and it rates on a later tick.
            Err(e) => tick.errors.push(format!("rating pass failed: {e}")),
        }
    }

    match meter.unrated_count().await {
        Ok(n) => tick.unrated_backlog = Some(n),
        Err(e) => tick.errors.push(format!("counting the rating queue failed: {e}")),
    }

    tick
}

/// Publish a tick's numbers to Prometheus.
///
/// Separate from `meter_tick` so the tick stays testable without a metrics registry,
/// and because the two gauges are reset here: a gauge vector keeps every label
/// combination it has ever been given, so leaving `billing_disabled` published after
/// billing is switched on would show a backlog that no longer has that cause.
pub fn publish(tick: &MeterTick, billing_enabled: bool) {
    if let Some(backlog) = tick.unrated_backlog {
        let g = &mm_core::metrics_global::BILLING_UNRATED_EVENTS;
        g.reset();
        // Exactly one of the two carries the count. They are separate labels because
        // they want separate alerts: rising while disabled is an unsent bill growing,
        // rising while enabled is rating stalled.
        let reason = if billing_enabled {
            "billing_enabled"
        } else {
            "billing_disabled"
        };
        g.with_label_values(&[reason]).set(backlog);
    }

    for (node, bytes) in &tick.sweep.metered_bytes {
        // A counter cannot go backwards, so a negative would panic prometheus. The
        // column is constrained non-negative; clamping is the belt to that braces.
        mm_core::metrics_global::EGRESS_METERED_BYTES
            .with_label_values(&[node_kind(node)])
            .inc_by(bytes.max(&0).unsigned_abs());
    }
}

/// The metric label for a node: one of two values, whatever the id.
///
/// Bounded on purpose (FR-302c). Fleet node ids embed the broadcast id, so labelling
/// by id leaves a permanent series per broadcast.
pub fn node_kind(mm_node_id: &str) -> &'static str {
    if mm_node_id == crate::metering::ORIGIN_NODE_ID {
        "origin"
    } else {
        "fleet"
    }
}

/// Log a tick at the right volume.
///
/// A meter on a one-minute timer logs 1,440 times a day, so a tick that found
/// nothing says nothing. The "billing is off and a bill is accumulating" warning is
/// rate-limited by [`backlog_warning_due`] — see there for why "on change" was wrong.
///
/// `last_warned` is the backlog at the last warning; the return value is the one to
/// pass next tick.
pub fn log_tick(tick: &MeterTick, billing_enabled: bool, last_warned: Option<i64>) -> Option<i64> {
    let mut last_warned = last_warned;
    let s = &tick.sweep;

    for e in &tick.errors {
        tracing::error!("egress meter: {e}");
    }
    for (node, why) in &s.unreachable {
        tracing::warn!(node = %node, reason = %why, "egress meter: node not metered");
    }
    for a in &s.anomalies {
        tracing::warn!(anomaly = ?a, "egress meter: metering anomaly");
    }
    if !s.unbillable.is_empty() {
        // Bytes were delivered that nobody can be charged for. A product problem,
        // not a rounding error, so it is warned about rather than counted quietly.
        tracing::warn!(
            sources = ?s.unbillable,
            "egress meter: delivered bytes with no payer"
        );
    }

    if s.events_written > 0 {
        tracing::info!(
            nodes = s.polled.len(),
            events = s.events_written,
            "egress meter: usage recorded"
        );
    }

    match (&tick.rating, billing_enabled) {
        (Some(r), _) => {
            if r.rated > 0 || r.recovered > 0 {
                tracing::info!(
                    rated = r.rated,
                    charged_minor = r.charged_minor,
                    recovered = r.recovered,
                    "billing: usage charged"
                );
            }
            if !r.insufficient_funds.is_empty() {
                tracing::warn!(
                    broadcasts = ?r.insufficient_funds,
                    "billing: wallet cannot cover the charge — usage left unrated"
                );
            }
            if !r.unpriceable.is_empty() {
                tracing::warn!(
                    count = r.unpriceable.len(),
                    first = ?r.unpriceable.first(),
                    "billing: usage could not be priced — left unrated"
                );
            }
            if !r.without_wallet.is_empty() {
                tracing::warn!(
                    users = ?r.without_wallet,
                    "billing: metered usage from users with no wallet"
                );
            }
        }
        (None, false) => {
            if let Some(backlog) = tick.unrated_backlog {
                if backlog_warning_due(backlog, last_warned) {
                    tracing::warn!(
                        unrated_events = backlog,
                        "billing is DISABLED and metered usage is accumulating — \
                         enabling MM_BILLING_ENABLED will charge this entire backlog \
                         in one pass"
                    );
                    last_warned = Some(backlog);
                } else if backlog == 0 {
                    // Drained (retention, or billing was on and off again): the next
                    // accumulation is news again.
                    last_warned = None;
                }
            }
        }
        // Billing on but no report: rating errored, already logged above.
        (None, true) => {}
    }
    last_warned
}

/// Should the "billing is off and a bill is accumulating" warning fire?
///
/// **On doubling, not on change.** The first version fired whenever the backlog
/// differed from last tick's, which was described as "not per tick" — but the backlog
/// grows on every tick that anything is live, so it fired every minute in exactly the
/// situation the warning exists for, burying the one line an operator needs in a
/// thousand copies of itself.
///
/// Doubling bounds it at about log₂(backlog) lines over the life of the backlog —
/// around twenty for a million events — while still saying "this is getting bigger"
/// each time it meaningfully does.
pub fn backlog_warning_due(backlog: i64, last_warned: Option<i64>) -> bool {
    match last_warned {
        _ if backlog <= 0 => false,
        None => true,
        Some(prev) => backlog >= prev.saturating_mul(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case the first version got wrong: a backlog that grows by a little every
    /// tick. It must NOT warn every tick.
    #[test]
    fn a_steadily_growing_backlog_warns_only_on_doubling() {
        let mut last = None;
        let mut warnings = 0;
        for backlog in 1..=1_000_000i64 {
            if backlog_warning_due(backlog, last) {
                warnings += 1;
                last = Some(backlog);
            }
        }
        assert_eq!(
            warnings, 20,
            "one warning per doubling from 1 to a million — not one per tick"
        );
    }

    /// REGRESSION (review 2026-09-25). The label must be bounded however many
    /// broadcasts there are — FR-302c.
    #[test]
    fn the_egress_metric_label_is_bounded_whatever_the_node_id() {
        assert_eq!(node_kind(crate::metering::ORIGIN_NODE_ID), "origin");
        for id in ["bc-b1-fanout-0", "bc-some-broadcast-fanout-7", "bc-x-transcode"] {
            assert_eq!(node_kind(id), "fleet", "{id}");
        }

        // And end to end through `publish`: whatever ids it is fed, only the two
        // label values may exist afterwards.
        let tick = MeterTick {
            sweep: MeteringSweep {
                metered_bytes: (0..50)
                    .map(|i| (format!("bc-b{i}-fanout-0"), 1_000))
                    .chain(std::iter::once((crate::metering::ORIGIN_NODE_ID.to_string(), 1)))
                    .collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        publish(&tick, false);
        use prometheus::core::Collector;
        let families = mm_core::metrics_global::EGRESS_METERED_BYTES.collect();
        let mut values: Vec<String> = families[0]
            .get_metric()
            .iter()
            .flat_map(|m: &prometheus::proto::Metric| {
                m.get_label().iter().map(|l| l.get_value().to_string()).collect::<Vec<_>>()
            })
            .collect();
        values.sort();
        values.dedup();
        assert!(
            values.iter().all(|v| v == "origin" || v == "fleet"),
            "fifty broadcasts must not produce fifty series: {values:?}"
        );
    }

    #[test]
    fn the_first_unrated_event_warns() {
        assert!(backlog_warning_due(1, None));
    }

    #[test]
    fn an_empty_backlog_never_warns() {
        assert!(!backlog_warning_due(0, None));
        assert!(!backlog_warning_due(0, Some(100)));
    }

    #[test]
    fn short_of_doubling_is_quiet() {
        assert!(!backlog_warning_due(199, Some(100)));
        assert!(backlog_warning_due(200, Some(100)));
    }
}
