//! `BillingSource` backed by the prepaid wallet (WS-D, design §17).
//!
//! This is what replaces [`crate::runner::NoBillingYet`] and makes `fleet=on`
//! capable of provisioning. Until it existed the runner reached the balance gate and
//! stopped, by construction.
//!
//! ## Two things the schema cannot yet tell us, stated rather than invented
//!
//! **There is no scheduled end.** `mm_streams` has `started_at` and `ended_at` and
//! nothing in between, so "projected cost **to scheduled end**" (§17.4) is not
//! computable today. This projects over a **configured horizon** instead, and
//! `HORIZON_NOTE` says so. A real scheduled end needs a column *and* a product
//! decision about how a broadcaster sets one — inventing either here would bake a
//! guess into every billing decision.
//!
//! The horizon being short is the safe direction: it under-states the bill, so the
//! gate lets a broadcast continue that a longer horizon would have stopped. Getting
//! that backwards would stop broadcasts that could afford to run.
//!
//! **There is no broadcaster tier.** Design §20's Open / Verified / Funded ladder is
//! not in the schema, so `transcode_enabled` uses the only signal that exists: a
//! funded wallet. That matches the owner's decision — *"transcode only for payed
//! users; for free, a single rate"* — but it is a proxy, not the model.

use async_trait::async_trait;
use mm_core::fleet::billing::BillingIncrement;
use mm_db::wallet_db::PgWalletDb;
use sqlx::PgPool;

use crate::runner::{BillingSource, BroadcastBilling};

/// Per-unit prices for one currency, as read from `mm_rate_card`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateCard {
    pub version: i32,
    pub node_minute_minor: i64,
    pub gpu_minute_minor: i64,
    pub egress_gb_minor: i64,
}

pub struct WalletBillingSource {
    pool: PgPool,
    wallet: PgWalletDb,
    /// How far ahead to project, in seconds. See `HORIZON_NOTE`.
    horizon_secs: u32,
    /// The currency the rate card is read in. One per deployment for now; a wallet in
    /// another currency is a mismatch the store refuses loudly rather than converting
    /// at a rate nobody agreed.
    currency: String,
}

impl WalletBillingSource {
    /// ⚠️ The projection horizon is a **stand-in for a scheduled end that does not
    /// exist in the schema**. One hour, chosen because it is one Scaleway billing
    /// period for a CPU instance — so the projection covers at least the period we
    /// are already committed to paying for.
    pub const HORIZON_NOTE: &'static str =
        "projection horizon stands in for a scheduled end; mm_streams has no such column (design §17.4 invariant 2)";

    pub const DEFAULT_HORIZON_SECS: u32 = 3600;

    pub fn new(pool: PgPool, currency: impl Into<String>) -> Self {
        Self {
            wallet: PgWalletDb::new(pool.clone()),
            pool,
            horizon_secs: Self::DEFAULT_HORIZON_SECS,
            currency: currency.into(),
        }
    }

    pub fn with_horizon_secs(mut self, secs: u32) -> Self {
        self.horizon_secs = secs;
        self
    }

    /// The newest rate card for this currency.
    ///
    /// Newest, because a *new* charge is rated against the current card; historical
    /// usage keeps the version it was rated with (§17.5), which is recorded on the
    /// usage event and never re-derived from here.
    pub async fn current_rate_card(&self) -> Result<RateCard, String> {
        let card = read_rate_card(&self.pool, &self.currency).await?;

        // A card that exists but omits `node_minute` projects every fan-out node at
        // zero, and a zero projection authorises an empty wallet to spend. That is
        // reachable, not theoretical: the first realistic rate card prices egress,
        // and `RateCard` defaults every unmentioned unit to 0. Refusing is the same
        // reasoning as refusing an absent card — a price of zero is a giveaway, not
        // a cautious default.
        if card.node_minute_minor <= 0 {
            return Err(format!(
                "rate card {} for {} has no price for node_minute — every projection \
                 would be zero, which authorises an empty wallet to provision",
                card.version, self.currency
            ));
        }

        Ok(card)
    }
}

/// The newest rate card for `currency`, **without** the planner's `node_minute`
/// requirement.
///
/// Split out because the two callers need different rules. The planner must refuse a
/// card with no `node_minute` price: it projects fan-out nodes, and a zero projection
/// authorises an empty wallet to provision. The ladder must not: on the origin-only
/// fleet (`frozen`, the default) a broadcast runs no nodes, and an egress-only card
/// prices everything it actually uses. The ladder's own rule — every unit IN USE must
/// have a price — lives in [`project_run_cost_minor`].
pub async fn read_rate_card(pool: &PgPool, currency: &str) -> Result<RateCard, String> {
    let rows: Vec<(i32, String, i64)> = sqlx::query_as(
        "SELECT version, unit, price_minor
           FROM mm_rate_card
          WHERE currency = $1
            AND version = (SELECT MAX(version) FROM mm_rate_card WHERE currency = $1)",
    )
    .bind(currency)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("reading the rate card failed: {e}"))?;

    if rows.is_empty() {
        // No card means no price, and a price of zero would let the gate authorise
        // unlimited spending. Refusing is the only safe reading.
        return Err(format!(
            "no rate card for currency {} — nothing may be provisioned without a price",
            currency
        ));
    }

    let mut card = RateCard {
        version: rows[0].0,
        ..Default::default()
    };
    for (_, unit, price) in &rows {
        match unit.as_str() {
            "node_minute" => card.node_minute_minor = *price,
            "gpu_minute" => card.gpu_minute_minor = *price,
            "egress_gb" => card.egress_gb_minor = *price,
            // storage_gb_month is a recurring charge on the recording, not part
            // of a live broadcast's burn rate (§17.2).
            _ => {}
        }
    }
    Ok(card)
}

/// Cost of running `fanout_nodes` fan-out and `transcode_nodes` transcode nodes for
/// `horizon_secs`, rounded up to whole billing periods.
///
/// Pure, so the arithmetic that decides whether a broadcast may continue is testable
/// without a database — and rounded **up**, because that is what the invoice does:
/// a projection that under-counts partial periods authorises spending the provider
/// will bill for anyway.
pub fn project_cost_minor(
    card: &RateCard,
    fanout_nodes: u32,
    transcode_nodes: u32,
    horizon_secs: u32,
    fanout_increment: BillingIncrement,
) -> i64 {
    // Fan-out is CPU and bills per hour on Scaleway; transcode is GPU and bills per
    // minute. Charging both by the minute would under-state the fan-out bill by up
    // to 59 minutes per node.
    let fanout_periods = i64::from(
        fanout_increment.round_ttl_secs(horizon_secs) / (fanout_increment.seconds() as u32).max(1),
    );
    let fanout_minutes_per_period = (fanout_increment.seconds() / 60).max(1);

    let fanout = i64::from(fanout_nodes)
        * fanout_periods
        * fanout_minutes_per_period
        * card.node_minute_minor;

    // Transcode per minute, rounded up.
    let gpu_minutes = i64::from(horizon_secs.div_ceil(60));
    let gpu = i64::from(transcode_nodes) * gpu_minutes * card.gpu_minute_minor;

    fanout + gpu
}

#[async_trait]
impl BillingSource for WalletBillingSource {
    async fn quote(&self, broadcast_id: &str) -> Result<BroadcastBilling, String> {
        // Whose wallet pays. A broadcast with no host row cannot be billed to anyone,
        // and provisioning for it would be spending nobody agreed to.
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
            .ok_or_else(|| {
                format!("no wallet for {host} — nothing may be provisioned on their behalf")
            })?;

        if wallet.currency != self.currency {
            // Refused rather than converted: an FX rate applied here would be a
            // price nobody agreed, and a silent one.
            return Err(format!(
                "wallet for {host} is in {} but the rate card is in {} — refusing to \
                 convert at a rate nobody agreed",
                wallet.currency, self.currency
            ));
        }

        let card = self.current_rate_card().await?;

        // Nodes already serving this broadcast. Node ids are `bc-{broadcast}-…`
        // (planner::DesiredNode), which is the link — and it survives teardown
        // deleting the desired row.
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

        let mut fanout = 0u32;
        let mut transcode = 0u32;
        for (flavor, n) in counts {
            match flavor.as_str() {
                "fanout" | "edge" => fanout += n as u32,
                "transcode" => transcode += n as u32,
                _ => {}
            }
        }

        // A broadcast with no nodes yet still projects for one: the gate is being
        // asked whether it may provision, and quoting zero would answer "yes, for
        // free" to a wallet that cannot afford the first node.
        let fanout_for_projection = fanout.max(1);

        let projected = project_cost_minor(
            &card,
            fanout_for_projection,
            transcode,
            self.horizon_secs,
            BillingIncrement::PerHour,
        );

        Ok(BroadcastBilling {
            // spendable, not balance: a wallet at zero with a credit limit can pay.
            available_balance_minor: wallet.spendable_minor(),
            projected_cost_minor: projected,
            // Proxy for design §20's tier ladder, which is not in the schema. Matches
            // the owner's decision that transcode is for paying broadcasters only.
            transcode_enabled: wallet.spendable_minor() > 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> RateCard {
        // 1 minor unit per node-minute, 4 per GPU-minute — round numbers so the
        // arithmetic is checkable by eye rather than by rerunning the function.
        RateCard {
            version: 1,
            node_minute_minor: 1,
            gpu_minute_minor: 4,
            egress_gb_minor: 9,
        }
    }

    /// Fan-out bills per hour, so an hour's horizon is 60 node-minutes per node.
    #[test]
    fn an_hour_of_one_fanout_node_is_sixty_node_minutes() {
        let cost = project_cost_minor(&card(), 1, 0, 3600, BillingIncrement::PerHour);
        assert_eq!(cost, 60);
    }

    /// THE ROUNDING THAT MATTERS. A 30-minute horizon on hourly billing still bills a
    /// whole hour, so a projection that counted 30 minutes would authorise spending
    /// the provider bills 60 for.
    #[test]
    fn a_partial_hour_projects_as_a_whole_hour_for_hourly_billing() {
        let half = project_cost_minor(&card(), 1, 0, 1800, BillingIncrement::PerHour);
        let full = project_cost_minor(&card(), 1, 0, 3600, BillingIncrement::PerHour);
        assert_eq!(
            half, full,
            "a projection that under-counts a partial period authorises spending the \
             invoice will charge for anyway"
        );
        assert_eq!(half, 60);
    }

    #[test]
    fn nodes_and_hours_both_multiply() {
        assert_eq!(project_cost_minor(&card(), 3, 0, 3600, BillingIncrement::PerHour), 180);
        assert_eq!(project_cost_minor(&card(), 1, 0, 7200, BillingIncrement::PerHour), 120);
        assert_eq!(project_cost_minor(&card(), 2, 0, 7200, BillingIncrement::PerHour), 240);
    }

    /// GPU bills per minute, so it is NOT rounded to the hour — charging it by the
    /// hour would over-state a short transcode job by up to 59 minutes.
    #[test]
    fn transcode_is_projected_per_minute_not_per_hour() {
        // 30 minutes of one transcode node at 4/minute = 120.
        assert_eq!(project_cost_minor(&card(), 0, 1, 1800, BillingIncrement::PerHour), 120);
        // And a partial minute rounds up, because the provider does.
        assert_eq!(project_cost_minor(&card(), 0, 1, 90, BillingIncrement::PerHour), 8);
    }

    #[test]
    fn fanout_and_transcode_add() {
        let cost = project_cost_minor(&card(), 2, 1, 3600, BillingIncrement::PerHour);
        assert_eq!(cost, 120 + 240, "2 nodes × 60 min × 1, plus 60 min × 4");
    }

    #[test]
    fn no_nodes_costs_nothing() {
        assert_eq!(project_cost_minor(&card(), 0, 0, 3600, BillingIncrement::PerHour), 0);
    }

    /// Per-minute fan-out (a provider that bills that way) must not be rounded to the
    /// hour — the increment is a parameter precisely so this stays right when the
    /// provider changes.
    #[test]
    fn per_minute_fanout_is_not_rounded_up_to_an_hour() {
        let hourly = project_cost_minor(&card(), 1, 0, 1800, BillingIncrement::PerHour);
        let per_min = project_cost_minor(&card(), 1, 0, 1800, BillingIncrement::PerMinute);
        assert_eq!(hourly, 60);
        assert_eq!(per_min, 30, "30 minutes billed per minute is 30 node-minutes");
    }

    #[test]
    fn the_horizon_is_documented_as_a_stand_in() {
        assert!(WalletBillingSource::HORIZON_NOTE.contains("scheduled end"));
        assert_eq!(
            WalletBillingSource::DEFAULT_HORIZON_SECS, 3600,
            "one Scaleway CPU billing period, so the projection covers at least what \
             we are already committed to paying"
        );
    }
}
