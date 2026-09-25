//! How a provider charges, and what that means for when to destroy a node.
//!
//! This exists because of one measured fact: **Scaleway bills CPU Instances per
//! hour of uptime, rounded up, with a 60-minute minimum** — while GPU Instances
//! bill per minute. So the same fleet has two billing shapes at once, and a
//! reaper that treats them alike is wrong for one of them.
//!
//! The consequence that changes behaviour: with whole-hour billing, **destroying a
//! node five minutes into a paid hour saves nothing.** A node killed at 65 minutes
//! and one killed at 119 minutes both owe two hours; the second served 54 more
//! minutes of viewers for the same money. So teardown should wait for the
//! boundary — but only where the boundary exists.
//!
//! Everything here is pure and clock-free: `now` arrives as an argument, so "one
//! second before the boundary" and "one second after" are both testable.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// The granularity a provider actually charges at.
///
/// Measured values, not guesses: Scaleway's Instances FAQ states CPU Instances are
/// "billed per hour of uptime" and GPU Instances "per minute of uptime", and its
/// Specific Conditions Art. 6 makes the hour "due in full" once begun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingIncrement {
    /// Charged by the second past a stated minimum. Nothing to align to.
    PerSecond,
    /// Charged by the minute — Scaleway GPU Instances. The wasted remainder is at
    /// most 59 seconds, which is not worth deferring a teardown for.
    PerMinute,
    /// Charged by the hour, rounded up — Scaleway CPU Instances. **The only
    /// increment where teardown timing is worth money.**
    PerHour,
}

impl BillingIncrement {
    pub fn seconds(self) -> i64 {
        match self {
            BillingIncrement::PerSecond => 1,
            BillingIncrement::PerMinute => 60,
            BillingIncrement::PerHour => 3600,
        }
    }

    /// Is there enough wasted remainder in a partially-used period to be worth
    /// deferring a teardown for?
    ///
    /// Only for `PerHour`. Deferring to save 59 seconds would trade a
    /// cost-safety action for a rounding error, which is the wrong way round.
    pub fn worth_aligning_teardown(self) -> bool {
        matches!(self, BillingIncrement::PerHour)
    }

    /// Round a requested TTL up to something the provider can actually charge.
    ///
    /// A 90-minute TTL on hourly billing is a **lie about cost**: it bills two
    /// hours. Expressing it as two hours makes the bill and the config agree, and
    /// makes the operator's mental model match the invoice.
    pub fn round_ttl_secs(self, requested: u32) -> u32 {
        let unit = self.seconds() as u32;
        if requested == 0 {
            // Never zero: a rented node with a zero TTL would be destroyed the
            // instant it existed, having already been charged the minimum.
            return unit;
        }
        requested.div_ceil(unit) * unit
    }
}

/// Periods owed for a node that ran from `started_at` to `ended_at`.
///
/// Rounded up, minimum one — which is what "any hour begun is due in full" and a
/// "60-minute minimum" mean together. Returns 1 for a negative or zero interval:
/// a node that existed at all was charged for one period, and clocks disagree.
pub fn periods_owed(
    increment: BillingIncrement,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) -> u32 {
    let secs = (ended_at - started_at).num_seconds();
    if secs <= 0 {
        return 1;
    }
    let unit = increment.seconds();
    ((secs + unit - 1) / unit).max(1) as u32
}

/// The next instant at which a new period would begin — i.e. the last moment the
/// already-paid-for period is still usable.
///
/// Boundaries run from `started_at`, not from the wall clock: the provider's clock
/// starts when the instance does. A node started at 14:37 bills 14:37-15:37, so its
/// boundaries are at :37, not on the hour.
pub fn next_period_boundary(
    increment: BillingIncrement,
    started_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let unit = increment.seconds();
    let elapsed = (now - started_at).num_seconds();
    if elapsed < 0 {
        // `now` precedes the start: the clocks disagree, so the safest boundary is
        // the start itself — i.e. do not defer anything.
        return started_at;
    }
    let periods = elapsed / unit + 1;
    started_at + Duration::seconds(periods * unit)
}

/// When a node past its `destroy_deadline` should actually be destroyed.
///
/// `None` means "now". `Some(t)` means "at `t`, because the period up to `t` is
/// already paid for and the node can keep serving viewers until then for free".
///
/// **The boundary is anchored on the DEADLINE, not on `now`.** That distinction is
/// the whole correctness of this function, and the first version got it wrong.
///
/// Anchoring on `now` looks equivalent and is not: once a period has begun it is
/// already owed, so at *any* instant inside one there is always paid time left.
/// "Wait for the next boundary after now" therefore always waits — and on the next
/// tick it waits again, and the node is **never destroyed**. Each individual
/// deferral is under an hour while the total is unbounded, which is precisely how a
/// cost backstop gets switched off by an optimisation.
///
/// Anchored on the deadline, the answer is a fixed instant: the end of the period
/// the deadline fell in. Before it, defer; after it, destroy. The deferral can never
/// exceed one period past the deadline no matter how many times the sweeper runs.
///
/// Returns `None` whenever alignment cannot be justified:
///
/// * the increment has no meaningful remainder (`PerSecond`, `PerMinute`);
/// * the node's start time is unknown, so no boundary can be computed;
/// * that boundary has already passed.
pub fn aligned_teardown_at(
    increment: BillingIncrement,
    billing_started_at: Option<DateTime<Utc>>,
    destroy_deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if !increment.worth_aligning_teardown() {
        return None;
    }
    let started_at = billing_started_at?;
    let boundary = next_period_boundary(increment, started_at, destroy_deadline);
    (boundary > now).then_some(boundary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap().to_utc()
    }

    // ── rounding a TTL to what the provider can charge ───────────────────────

    #[test]
    fn an_hourly_ttl_rounds_up_to_whole_hours() {
        let h = BillingIncrement::PerHour;
        assert_eq!(h.round_ttl_secs(3600), 3600);
        assert_eq!(
            h.round_ttl_secs(5400),
            7200,
            "90 minutes bills two hours, so calling it 90 minutes is a lie about cost"
        );
        assert_eq!(h.round_ttl_secs(1), 3600, "the 60-minute minimum");
    }

    #[test]
    fn a_zero_ttl_becomes_one_period_not_zero() {
        // A rented node with a zero TTL would be destroyed the instant it
        // existed, having already been charged the minimum.
        assert_eq!(BillingIncrement::PerHour.round_ttl_secs(0), 3600);
        assert_eq!(BillingIncrement::PerMinute.round_ttl_secs(0), 60);
    }

    #[test]
    fn per_minute_and_per_second_round_to_their_own_units() {
        assert_eq!(BillingIncrement::PerMinute.round_ttl_secs(90), 120);
        assert_eq!(BillingIncrement::PerSecond.round_ttl_secs(90), 90);
    }

    // ── periods owed ─────────────────────────────────────────────────────────

    #[test]
    fn a_node_that_ran_65_minutes_owes_two_hours() {
        assert_eq!(
            periods_owed(BillingIncrement::PerHour, t(0), t(65 * 60)),
            2,
            "any hour begun is due in full"
        );
    }

    #[test]
    fn a_node_that_ran_119_minutes_also_owes_two_hours() {
        // The point of the whole module: these two cost the same, and the second
        // served 54 more minutes of viewers.
        assert_eq!(periods_owed(BillingIncrement::PerHour, t(0), t(119 * 60)), 2);
    }

    #[test]
    fn a_node_that_ran_five_minutes_still_owes_one_hour() {
        assert_eq!(periods_owed(BillingIncrement::PerHour, t(0), t(300)), 1);
    }

    #[test]
    fn a_zero_or_negative_interval_still_owes_one_period() {
        assert_eq!(periods_owed(BillingIncrement::PerHour, t(0), t(0)), 1);
        assert_eq!(
            periods_owed(BillingIncrement::PerHour, t(100), t(0)),
            1,
            "a node that existed at all was charged; a negative interval means the \
             clocks disagree, not that it was free"
        );
    }

    // ── boundaries run from the node's own start, not the wall clock ──────────

    #[test]
    fn boundaries_run_from_the_instance_start_not_the_top_of_the_hour() {
        // Started at :37 past. The provider's clock starts when the instance does,
        // so the boundary is at :37, not on the hour.
        let started = t(37 * 60);
        let now = t(37 * 60 + 100);
        assert_eq!(
            next_period_boundary(BillingIncrement::PerHour, started, now),
            t(37 * 60 + 3600)
        );
    }

    #[test]
    fn the_boundary_after_exactly_one_period_is_the_next_one() {
        let started = t(0);
        assert_eq!(
            next_period_boundary(BillingIncrement::PerHour, started, t(3600)),
            t(7200),
            "at exactly 3600s a second hour has begun and is already owed"
        );
    }

    #[test]
    fn a_now_before_the_start_yields_the_start_so_nothing_is_deferred() {
        let started = t(1000);
        assert_eq!(
            next_period_boundary(BillingIncrement::PerHour, started, t(0)),
            started
        );
    }

    // ── the decision the sweeper makes ───────────────────────────────────────

    /// THE ONE THAT SAVES MONEY. A deadline 5 minutes into a paid hour leaves 55
    /// minutes of already-purchased service.
    #[test]
    fn an_hourly_node_whose_deadline_fell_early_in_a_paid_hour_defers_to_that_boundary() {
        let started = t(0);
        let deadline = t(3600 + 300); // 5 minutes into hour two
        let now = deadline + Duration::seconds(30);
        let at = aligned_teardown_at(BillingIncrement::PerHour, Some(started), deadline, now)
            .expect("there is paid time left in the deadline's own hour");
        assert_eq!(at, t(7200));
        assert_eq!(
            (at - deadline).num_minutes(),
            55,
            "55 minutes of already-paid service, free to the broadcast"
        );
    }

    /// THE BUG THE FIRST VERSION HAD. Anchored on `now`, a node would defer on every
    /// tick forever: a period once begun is already owed, so there is always paid
    /// time left. Each wait under an hour, the total unbounded — a cost backstop
    /// switched off by an optimisation.
    #[test]
    fn deferral_expires_rather_than_renewing_on_every_tick() {
        let started = t(0);
        let deadline = t(3600 + 300);
        let boundary = t(7200);

        // Just before: still deferred.
        assert_eq!(
            aligned_teardown_at(
                BillingIncrement::PerHour,
                Some(started),
                deadline,
                boundary - Duration::seconds(1)
            ),
            Some(boundary)
        );

        // At and after the boundary: destroy now, however many ticks have passed.
        for later in [0, 1, 60, 3600, 86_400] {
            assert!(
                aligned_teardown_at(
                    BillingIncrement::PerHour,
                    Some(started),
                    deadline,
                    boundary + Duration::seconds(later)
                )
                .is_none(),
                "at {later}s past the boundary the node must go, not get another hour"
            );
        }
    }

    #[test]
    fn per_minute_billing_never_defers() {
        // Deferring to save 59 seconds would trade a cost-safety action for a
        // rounding error.
        assert!(
            aligned_teardown_at(BillingIncrement::PerMinute, Some(t(0)), t(30), t(31)).is_none()
        );
        assert!(
            aligned_teardown_at(BillingIncrement::PerSecond, Some(t(0)), t(30), t(31)).is_none()
        );
    }

    /// Without a start time there is no boundary to compute, so destroy now. The
    /// safe direction for a cost-safety mechanism is to act, not to wait.
    #[test]
    fn an_unknown_start_time_destroys_immediately() {
        assert!(aligned_teardown_at(BillingIncrement::PerHour, None, t(9000), t(9999)).is_none());
    }

    /// Bounded by construction, and now bounded relative to the DEADLINE rather
    /// than to `now` — which is what makes the bound hold across repeated sweeps.
    #[test]
    fn deferral_never_exceeds_one_period_past_the_deadline() {
        let started = t(0);
        for deadline_at in [1, 59, 3599, 3600, 7199, 86_400] {
            let deadline = t(deadline_at);
            for tick in [0, 1, 300, 3599, 3600, 7200] {
                let now = deadline + Duration::seconds(tick);
                if let Some(at) =
                    aligned_teardown_at(BillingIncrement::PerHour, Some(started), deadline, now)
                {
                    let past_deadline = (at - deadline).num_seconds();
                    assert!(
                        (0..=3600).contains(&past_deadline),
                        "teardown {past_deadline}s past a deadline at {deadline_at}s is \
                         outside one billing period"
                    );
                }
            }
        }
    }
}
