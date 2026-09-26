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
            .with_label_values(&[node.as_str()])
            .inc_by(bytes.max(&0).unsigned_abs());
    }
}

/// Log a tick at the right volume.
///
/// A meter on a one-minute timer logs 1,440 times a day, so a tick that found
/// nothing says nothing. `previous` is last tick's backlog: the "billing is off and
/// a bill is accumulating" warning fires when that number **changes**, not on every
/// tick, because otherwise the one line an operator needs to act on is buried in
/// identical copies of itself.
pub fn log_tick(tick: &MeterTick, billing_enabled: bool, previous: Option<i64>) {
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
            // Change-triggered, not per-tick. See the doc comment.
            if let Some(backlog) =
                tick.unrated_backlog.filter(|b| *b > 0 && Some(*b) != previous)
            {
                tracing::warn!(
                    unrated_events = backlog,
                    "billing is DISABLED and metered usage is accumulating — \
                     enabling MM_BILLING_ENABLED will charge this entire backlog \
                     in one pass"
                );
            }
        }
        // Billing on but no report: rating errored, already logged above.
        (None, true) => {}
    }
}
