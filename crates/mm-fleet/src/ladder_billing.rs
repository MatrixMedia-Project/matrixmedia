//! What the demotion ladder is told a broadcast costs (WS-D, design §17.4).
//!
//! ## Why this is not the planner's quote
//!
//! The first version reused `WalletBillingSource::quote`, which answers the
//! **planner's** question — "can this wallet afford one more node for an hour?". It
//! projects at least one fan-out node even when there are none, and it leaves egress
//! out. The ladder asks something else: "can this wallet afford what is running?". On
//! the origin-only fleet (`frozen`, the default) every broadcast runs no nodes and is
//! charged only for egress, so the reused quote judged every broadcast against a
//! node-hour it was not using while ignoring the thing it was actually billed for —
//! and observe mode's forecast, the evidence for setting the watermarks, would have
//! measured nothing real.
//!
//! So this quote counts:
//!
//! - the nodes that **exist** for the broadcast, and no phantom one;
//! - egress at its **measured** recent rate, projected over the horizon;
//! - and it subtracts usage **already metered but not yet charged** from the balance.
//!   Without that, with billing off the forecast says "healthy" while an unbilled
//!   backlog grows — and the moment billing is enabled the balance drops by all of it.
//!
//! Every unit actually in use must have a price. One that does not makes the
//! broadcast unpriceable, and the ladder skips an unpriceable broadcast rather than
//! demoting it.

use async_trait::async_trait;
use mm_core::fleet::billing::BillingIncrement;
use mm_db::wallet_db::PgWalletDb;
use sqlx::PgPool;

use crate::rating::charge_minor;
use crate::runner::{BillingSource, BroadcastBilling};
use crate::wallet_billing::{read_rate_card, RateCard};

/// How far back egress is measured to estimate its rate.
///
/// Ten minutes: long enough that one quiet poll does not read as a stopped broadcast,
/// short enough that an audience that has just doubled shows up within a few ladder
/// ticks (§17.4 invariant 2 — the burn rate must be recomputed continuously, because
/// audience growth changes it mid-broadcast).
pub const EGRESS_RATE_WINDOW_SECS: i64 = 600;

/// What is running and what has been measured, for one broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunUsage {
    pub fanout_nodes: u32,
    pub transcode_nodes: u32,
    /// Egress metered within the rate window, in thousandths of a GB.
    pub egress_milli_in_window: i64,
    /// The window actually covered — shorter than [`EGRESS_RATE_WINDOW_SECS`] for a
    /// broadcast younger than that, or the rate would be understated by an empty
    /// start.
    pub window_secs: i64,
}

/// Cost of keeping the current run going for `horizon_secs`, in minor units.
///
/// Errors when a unit **in use** has no price: a zero price for something being
/// consumed would read as "costs nothing" and keep the broadcast healthy on money it
/// does not have. A unit not in use needs no price — an egress-only card is complete
/// for an origin-only broadcast.
pub fn project_run_cost_minor(
    card: &RateCard,
    usage: &RunUsage,
    horizon_secs: u32,
    fanout_increment: BillingIncrement,
) -> Result<i64, String> {
    if usage.fanout_nodes > 0 && card.node_minute_minor <= 0 {
        return Err(format!(
            "rate card {} has no node_minute price, and {} fan-out node(s) are running",
            card.version, usage.fanout_nodes
        ));
    }
    if usage.transcode_nodes > 0 && card.gpu_minute_minor <= 0 {
        return Err(format!(
            "rate card {} has no gpu_minute price, and a transcode node is running",
            card.version
        ));
    }
    if usage.egress_milli_in_window > 0 && card.egress_gb_minor <= 0 {
        return Err(format!(
            "rate card {} has no egress_gb price, and the broadcast is delivering",
            card.version
        ));
    }

    // Nodes: exactly the ones running. `project_cost_minor` rounds fan-out up to whole
    // billing periods, which is what the invoice does.
    let nodes = crate::wallet_billing::project_cost_minor(
        card,
        usage.fanout_nodes,
        usage.transcode_nodes,
        horizon_secs,
        fanout_increment,
    );

    // Egress: the measured rate, scaled to the horizon.
    let window = usage.window_secs.max(1);
    let projected_milli = usage
        .egress_milli_in_window
        .saturating_mul(i64::from(horizon_secs))
        / window;
    let egress = charge_minor(projected_milli, card.egress_gb_minor);

    Ok(nodes.saturating_add(egress))
}

/// The balance the ladder should judge against: spendable, minus usage already
/// metered and not yet charged.
pub fn available_after_owed(spendable_minor: i64, owed_minor: i64) -> i64 {
    spendable_minor.saturating_sub(owed_minor.max(0))
}

pub struct LadderBillingSource {
    pool: PgPool,
    wallet: PgWalletDb,
    currency: String,
    horizon_secs: u32,
}

impl LadderBillingSource {
    pub fn new(pool: PgPool, currency: impl Into<String>) -> Self {
        Self {
            wallet: PgWalletDb::new(pool.clone()),
            pool,
            currency: currency.into(),
            horizon_secs: crate::wallet_billing::WalletBillingSource::DEFAULT_HORIZON_SECS,
        }
    }
}

#[async_trait]
impl BillingSource for LadderBillingSource {
    async fn quote(&self, broadcast_id: &str) -> Result<BroadcastBilling, String> {
        let host: Option<String> =
            sqlx::query_scalar("SELECT host_user_id FROM mm_streams WHERE id = $1")
                .bind(broadcast_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| format!("reading the broadcast host failed: {e}"))?;
        let host = host.ok_or_else(|| format!("no host for broadcast {broadcast_id}"))?;

        let wallet = self
            .wallet
            .get(&host)
            .await
            .map_err(|e| format!("reading the wallet failed: {e}"))?
            .ok_or_else(|| format!("no wallet for {host}"))?;
        if wallet.currency != self.currency {
            return Err(format!(
                "wallet for {host} is in {} but the rate card is in {} — refusing to \
                 convert at a rate nobody agreed",
                wallet.currency, self.currency
            ));
        }

        let card = read_rate_card(&self.pool, &self.currency).await?;

        // Nodes that exist for this broadcast — no phantom one.
        let counts: Vec<(String, i64)> = sqlx::query_as(
            "SELECT flavor, count(*)::BIGINT
               FROM mm_fleet_nodes
              WHERE mm_node_id LIKE $1 AND state <> 'gone'
              GROUP BY flavor",
        )
        .bind(format!("bc-{broadcast_id}-%"))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("counting fleet nodes failed: {e}"))?;
        let mut usage = RunUsage::default();
        for (flavor, n) in counts {
            match flavor.as_str() {
                "fanout" | "edge" => usage.fanout_nodes += n as u32,
                "transcode" => usage.transcode_nodes += n as u32,
                _ => {}
            }
        }

        // Measured egress over the window, which is clipped to the broadcast's age
        // (floor one minute) so a young broadcast's rate is not diluted by the time
        // before it started.
        let (milli, window): (i64, i64) = sqlx::query_as(
            "SELECT
                 COALESCE((SELECT SUM(e.quantity_milli)::BIGINT
                             FROM mm_usage_events e
                            WHERE e.broadcast_id = s.id AND e.unit = 'egress_gb'
                              AND e.occurred_at > now() - make_interval(secs => $2)), 0),
                 GREATEST(60, LEAST($2, EXTRACT(EPOCH FROM now() - s.started_at)))::BIGINT
               FROM mm_streams s WHERE s.id = $1",
        )
        .bind(broadcast_id)
        .bind(EGRESS_RATE_WINDOW_SECS as f64)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("measuring egress failed: {e}"))?;
        usage.egress_milli_in_window = milli;
        usage.window_secs = window;

        let projected = project_run_cost_minor(
            &card,
            &usage,
            self.horizon_secs,
            BillingIncrement::PerHour,
        )?;

        // Usage metered and not yet charged, across this payer's broadcasts: it is
        // owed from the same balance. Priced at the current card, per unit.
        let unrated: Vec<(String, i64)> = sqlx::query_as(
            "SELECT unit, SUM(quantity_milli)::BIGINT
               FROM mm_usage_events
              WHERE user_id = $1 AND rated_at IS NULL
              GROUP BY unit",
        )
        .bind(&host)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("reading unrated usage failed: {e}"))?;
        let mut owed = 0i64;
        for (unit, milli) in unrated {
            let price = match unit.as_str() {
                "egress_gb" => card.egress_gb_minor,
                "gpu_minute" => card.gpu_minute_minor,
                "node_minute" => card.node_minute_minor,
                _ => 0,
            };
            owed = owed.saturating_add(charge_minor(milli, price));
        }

        Ok(BroadcastBilling {
            available_balance_minor: available_after_owed(wallet.spendable_minor(), owed),
            projected_cost_minor: projected,
            transcode_enabled: wallet.spendable_minor() > 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(node: i64, gpu: i64, egress: i64) -> RateCard {
        RateCard {
            version: 1,
            node_minute_minor: node,
            gpu_minute_minor: gpu,
            egress_gb_minor: egress,
        }
    }

    const HOUR: u32 = 3600;

    /// REGRESSION (review 2026-09-25). The planner's quote projects one fan-out node
    /// even when none is running. An origin-only broadcast with no egress yet costs
    /// nothing to keep going, and must be quoted as nothing.
    #[test]
    fn an_origin_only_broadcast_is_not_charged_for_a_phantom_node() {
        let cost = project_run_cost_minor(
            &card(10, 0, 9),
            &RunUsage { window_secs: 600, ..Default::default() },
            HOUR,
            BillingIncrement::PerHour,
        )
        .unwrap();
        assert_eq!(cost, 0);
    }

    /// REGRESSION. Egress is what an origin-only broadcast is billed for, so it is
    /// what its projection must contain: 1 GB in the last 10 minutes is 6 GB/hour.
    #[test]
    fn measured_egress_is_projected_over_the_horizon() {
        let cost = project_run_cost_minor(
            &card(0, 0, 9),
            &RunUsage { egress_milli_in_window: 1_000, window_secs: 600, ..Default::default() },
            HOUR,
            BillingIncrement::PerHour,
        )
        .unwrap();
        assert_eq!(cost, 6 * 9);
    }

    /// A young broadcast's window is its age, so a minute of egress is not read as a
    /// tenth of the real rate.
    #[test]
    fn a_short_window_is_not_diluted() {
        let young = RunUsage { egress_milli_in_window: 100, window_secs: 60, ..Default::default() };
        let cost = project_run_cost_minor(&card(0, 0, 9), &young, HOUR, BillingIncrement::PerHour).unwrap();
        // 0.1 GB/min -> 6 GB/hour.
        assert_eq!(cost, 54);
    }

    #[test]
    fn running_nodes_are_counted_at_whole_billing_periods() {
        let usage = RunUsage { fanout_nodes: 2, window_secs: 600, ..Default::default() };
        let cost = project_run_cost_minor(&card(10, 0, 9), &usage, HOUR, BillingIncrement::PerHour).unwrap();
        assert_eq!(cost, 2 * 60 * 10);
    }

    /// An egress-only card is complete for an origin-only broadcast — the planner's
    /// `node_minute` requirement must not leak into the ladder, or every broadcast in
    /// `frozen` is unpriceable and the ladder does nothing.
    #[test]
    fn an_egress_only_card_prices_an_origin_only_broadcast() {
        let usage = RunUsage { egress_milli_in_window: 1_000, window_secs: 600, ..Default::default() };
        assert!(project_run_cost_minor(&card(0, 0, 9), &usage, HOUR, BillingIncrement::PerHour).is_ok());
    }

    /// But a unit IN USE with no price is unpriceable, never "free".
    #[test]
    fn a_unit_in_use_without_a_price_is_refused() {
        let nodes = RunUsage { fanout_nodes: 1, window_secs: 600, ..Default::default() };
        assert!(project_run_cost_minor(&card(0, 0, 9), &nodes, HOUR, BillingIncrement::PerHour).is_err());

        let egress = RunUsage { egress_milli_in_window: 1, window_secs: 600, ..Default::default() };
        assert!(project_run_cost_minor(&card(10, 0, 0), &egress, HOUR, BillingIncrement::PerHour).is_err());

        let gpu = RunUsage { transcode_nodes: 1, window_secs: 600, ..Default::default() };
        assert!(project_run_cost_minor(&card(10, 0, 9), &gpu, HOUR, BillingIncrement::PerHour).is_err());
    }

    /// Usage metered and not yet charged is owed from the same balance. With billing
    /// off, ignoring it forecasts "healthy" while the backlog grows.
    #[test]
    fn unrated_usage_reduces_the_balance_judged() {
        assert_eq!(available_after_owed(1_000, 300), 700);
        assert_eq!(available_after_owed(100, 300), -200, "and can take it below zero");
        assert_eq!(available_after_owed(100, -5), 100, "a negative owed figure is ignored");
    }
}
