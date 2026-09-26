//! Turning metered usage into wallet charges (WS-D, design §17.5).
//!
//! `mm_usage_events` with `rated_at IS NULL` is the queue; V035's partial index is
//! the work list. This prices each event against the current rate card, charges the
//! wallet, and records which card version did it — so history is never re-priced.
//!
//! ## Why the charge and the mark are separate, and why that is safe
//!
//! Ideally both would be one transaction. They are not, because the charge goes
//! through `PgWalletDb`, whose whole design is that the *database* maintains the
//! balance through a trigger — and threading a caller's transaction through that
//! would let a caller hold the wallet row across arbitrary other work.
//!
//! So: **charge first, then mark.** The charge is idempotent on a key derived from
//! the event, so if the mark fails the next pass re-charges, gets `AlreadyApplied`,
//! recovers the transaction id, and marks. The reverse order — mark then charge —
//! would leave an event recorded as billed that was never charged, which is
//! unrecoverable because nothing would ever look at it again.
//!
//! ## Rounding, and why not truncation
//!
//! A price is per whole unit; a quantity is in thousandths. The exact charge is
//! `quantity_milli × price ÷ 1000`, and the remainder is a fraction of a minor unit.
//!
//! Truncating would bias **every** charge downward — the same systematic error as
//! advancing a baseline past sub-unit bytes, and at one event per node per poll it
//! accumulates. Here it is cheap to avoid: rounding half-up leaves the error bounded
//! by one minor unit and **centred on zero**, so it neither favours us nor the
//! customer over any number of events.

use chrono::{DateTime, Utc};
use mm_db::metering_db::{PendingUsage, PgMeteringDb, RateCard};
use mm_db::wallet_db::{treat_replay_as_success, ChargeOutcome, PgWalletDb, WalletError};

/// What one rating pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RatingReport {
    /// Events priced, charged and marked.
    pub rated: usize,
    /// Total charged, in minor units.
    pub charged_minor: i64,
    /// Events already charged by an earlier pass whose mark had not landed. A
    /// success — it means the idempotent charge did its job.
    pub recovered: usize,
    /// Broadcasts whose wallet could not cover the charge. **Left unrated on
    /// purpose**: the debt is real, and the demotion ladder is what acts on it. They
    /// will rate when the wallet is topped up.
    pub insufficient_funds: Vec<String>,
    /// Events that could not be priced at all, with why. Left unrated.
    pub unpriceable: Vec<(String, String)>,
    /// Users with unrated usage and **no wallet**. Invisible to the main query,
    /// because it joins the wallet — so they are asked about separately.
    pub without_wallet: Vec<String>,
    /// Events not attempted this pass because an EARLIER event of the same wallet
    /// could not be afforded. Charging a newer event while older usage is unpaid
    /// would break oldest-first for that broadcaster.
    pub deferred: usize,
}

/// Charge in minor units for `quantity_milli` thousandths of a unit at
/// `price_minor` per whole unit.
///
/// Rounds **half-up**. See the module docs: truncation biases every charge downward,
/// and here the fix is free.
///
/// Saturating rather than wrapping. A quantity large enough to overflow is a metering
/// bug, and a wrapped charge would be a nonsensical number — possibly negative, which
/// V035's sign constraint would reject, leaving the event stuck with no explanation.
pub fn charge_minor(quantity_milli: i64, price_minor: i64) -> i64 {
    if quantity_milli <= 0 || price_minor <= 0 {
        return 0;
    }
    let scaled = quantity_milli.saturating_mul(price_minor);
    scaled.saturating_add(500) / 1000
}

/// The charge's idempotency key, derived from the event's own.
///
/// Prefixed rather than reused verbatim so a ledger row can never collide with some
/// future non-charge movement that happens to quote a usage key.
pub fn charge_idempotency_key(usage_key: &str) -> String {
    format!("rate:{usage_key}")
}

/// Price and charge up to `batch` pending events.
///
/// Each event is independent: one that cannot be priced, or whose wallet is empty,
/// does not stop the others. A broadcaster with funds still gets billed while
/// another's balance is being argued about.
pub async fn rate_pending(
    meter: &PgMeteringDb,
    wallet: &PgWalletDb,
    batch: i64,
    now: DateTime<Utc>,
) -> Result<RatingReport, String> {
    // Asked separately because the main query joins the wallet, so usage belonging
    // to a user without one would sit in the queue invisibly.
    let mut report = RatingReport {
        without_wallet: meter
            .unrated_without_wallet()
            .await
            .map_err(|e| format!("checking for wallet-less usage failed: {e}"))?,
        ..Default::default()
    };

    let pending = meter
        .unrated_events(batch)
        .await
        .map_err(|e| format!("reading the rating queue failed: {e}"))?;

    // One rate-card read per currency, not per event.
    let mut cards: std::collections::HashMap<String, Option<RateCard>> =
        std::collections::HashMap::new();

    // Wallets that could not cover a charge this pass, and wallets already
    // unblocked this pass (so a successful user costs one UPDATE, not one per event).
    let mut stalled: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unblocked: std::collections::HashSet<String> = std::collections::HashSet::new();

    for ev in pending {
        if stalled.contains(&ev.user_id) {
            report.deferred += 1;
            continue;
        }

        let card = match cards.entry(ev.currency.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let loaded = meter
                    .current_prices(&ev.currency)
                    .await
                    .map_err(|err| format!("reading the rate card failed: {err}"))?;
                e.insert(loaded)
            }
        };

        let Some((version, prices)) = card.as_ref() else {
            // No card means no price, and a price of zero would silently forgive the
            // charge. Left unrated so it bills once a card exists.
            report.unpriceable.push((
                ev.idempotency_key.clone(),
                format!("no rate card for currency {}", ev.currency),
            ));
            block(meter, &ev.user_id, now, &mut report).await;
            continue;
        };

        let Some((_, price)) = prices.iter().find(|(unit, _)| unit == &ev.unit) else {
            report.unpriceable.push((
                ev.idempotency_key.clone(),
                format!("rate card {version} has no price for unit {}", ev.unit),
            ));
            // Rotate the wallet behind billable ones, but do NOT stall it: an
            // unpriceable unit is a configuration gap, not an inability to pay, and
            // stalling would stop this broadcaster's priceable usage being billed.
            block(meter, &ev.user_id, now, &mut report).await;
            continue;
        };

        let amount = charge_minor(ev.quantity_milli, *price);
        if amount == 0 {
            // Priced at nothing. Marked rated with no transaction, which V035 permits
            // — rated_at and the version without a transaction id is the "priced at
            // zero" shape — so it leaves the queue instead of being re-read forever.
            if let Err(e) = mark_zero_rated(meter, &ev, *version, now).await {
                report.unpriceable.push((ev.idempotency_key.clone(), e));
            } else {
                report.rated += 1;
            }
            continue;
        }

        let key = charge_idempotency_key(&ev.idempotency_key);
        let outcome = treat_replay_as_success(
            wallet
                .charge(
                    &ev.user_id,
                    &ev.currency,
                    amount,
                    &key,
                    Some(&ev.broadcast_id),
                )
                .await,
        );

        let tx_id = match outcome {
            Ok(ChargeOutcome::Applied) => {
                report.charged_minor += amount;
                meter
                    .transaction_id_for_key(&key)
                    .await
                    .map_err(|e| format!("recovering the transaction id failed: {e}"))?
            }
            Ok(ChargeOutcome::AlreadyApplied) => {
                // An earlier pass charged this and did not get as far as marking it.
                // Recovering the id here is what stops the event being stuck unrated
                // while its money has already moved.
                report.recovered += 1;
                meter
                    .transaction_id_for_key(&key)
                    .await
                    .map_err(|e| format!("recovering the transaction id failed: {e}"))?
            }
            Err(WalletError::InsufficientFunds { .. }) => {
                // Left UNRATED deliberately. The debt is real; the demotion ladder is
                // what acts on an empty wallet, and this event will rate when the
                // wallet is topped up. Marking it rated would forgive the charge.
                //
                // And the wallet moves to the back of the queue (V038), or its
                // unaffordable events — which stay the oldest — fill every batch and
                // nobody else is ever billed.
                report.insufficient_funds.push(ev.broadcast_id.clone());
                block(meter, &ev.user_id, now, &mut report).await;
                stalled.insert(ev.user_id.clone());
                continue;
            }
            Err(e) => {
                report
                    .unpriceable
                    .push((ev.idempotency_key.clone(), e.to_string()));
                continue;
            }
        };

        let Some(tx_id) = tx_id else {
            // The charge succeeded but its ledger row cannot be found, which should be
            // impossible. Left unrated rather than marked without a transaction id,
            // because a charge nobody can trace is worse than one that retries.
            report.unpriceable.push((
                ev.idempotency_key.clone(),
                "charged but the ledger row could not be found".into(),
            ));
            continue;
        };

        if let Err(e) = meter.mark_rated(ev.id, *version, tx_id, now).await {
            // The money moved. The next pass re-charges idempotently, recovers the id
            // and marks — so this is a retry, not a lost charge.
            report.unpriceable.push((
                ev.idempotency_key.clone(),
                format!("charged but marking rated failed (will retry): {e}"),
            ));
            continue;
        }
        report.rated += 1;

        // A successful charge means this wallet can be billed again; bring it back
        // to the front of the queue.
        if unblocked.insert(ev.user_id.clone())
            && let Err(e) = meter.unblock_rating(&ev.user_id).await
        {
            report
                .unpriceable
                .push((ev.idempotency_key.clone(), format!("unblocking the wallet failed: {e}")));
        }
    }

    Ok(report)
}

/// Move a wallet behind billable ones. A failure here is reported, not swallowed:
/// it is the difference between this wallet rotating and it leading every batch.
async fn block(
    meter: &PgMeteringDb,
    user_id: &str,
    now: DateTime<Utc>,
    report: &mut RatingReport,
) {
    if let Err(e) = meter.block_rating(user_id, now).await {
        report
            .unpriceable
            .push((user_id.to_string(), format!("blocking the wallet failed: {e}")));
    }
}

/// Mark an event that priced to zero.
///
/// V035's CHECK allows `rated_at` and a version without a transaction, which is
/// exactly the "priced at nothing" shape — and letting it leave the queue is the
/// point, because otherwise every pass re-reads it forever.
async fn mark_zero_rated(
    meter: &PgMeteringDb,
    ev: &PendingUsage,
    version: i32,
    now: DateTime<Utc>,
) -> Result<(), String> {
    meter
        .mark_rated_without_charge(ev.id, version, now)
        .await
        .map_err(|e| format!("marking a zero-priced event failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The money arithmetic ─────────────────────────────────────────────────

    #[test]
    fn a_whole_unit_at_a_whole_price_is_exact() {
        // 1.000 GB at 9 minor units per GB.
        assert_eq!(charge_minor(1_000, 9), 9);
        assert_eq!(charge_minor(2_000, 9), 18);
    }

    /// THE ROUNDING DECISION. Truncation would bias every charge downward — the same
    /// systematic error as advancing a baseline past sub-unit bytes — and here it is
    /// free to avoid.
    #[test]
    fn a_fractional_charge_rounds_half_up_rather_than_truncating() {
        // 2.500 GB at 9 = 22.5 exactly.
        assert_eq!(
            charge_minor(2_500, 9),
            23,
            "truncation would give 22 and under-bill on every single event"
        );
        // 2.499 GB at 9 = 22.491 -> 22.
        assert_eq!(charge_minor(2_499, 9), 22);
        // 1.111 GB at 9 = 9.999 -> 10.
        assert_eq!(charge_minor(1_111, 9), 10);
    }

    /// Bounded by one minor unit and centred on zero, so over many events it favours
    /// neither side. That is the property that makes half-up defensible rather than
    /// merely convenient.
    #[test]
    fn the_rounding_error_is_bounded_and_unbiased() {
        let price = 7;
        let mut total_error_thousandths: i64 = 0;
        for q in 1..=2000 {
            let exact_thousandths = q * price;
            let charged = charge_minor(q, price);
            let err = charged * 1000 - exact_thousandths;
            assert!(
                err.abs() <= 500,
                "quantity {q} charged {charged}, error {err} thousandths exceeds half a \
                 minor unit"
            );
            total_error_thousandths += err;
        }
        // Symmetric rounding: the accumulated bias is a rounding artefact of the
        // half-up tie, not a systematic lean.
        assert!(
            total_error_thousandths.abs() <= 2000,
            "accumulated bias over 2000 events is {total_error_thousandths} thousandths \
             of a minor unit — that is a systematic lean, not rounding"
        );
    }

    #[test]
    fn nothing_used_or_nothing_charged_is_zero() {
        assert_eq!(charge_minor(0, 9), 0);
        assert_eq!(charge_minor(1_000, 0), 0, "a free unit charges nothing");
        assert_eq!(charge_minor(-5, 9), 0, "a negative quantity can never become a charge");
        assert_eq!(charge_minor(1_000, -9), 0);
    }

    /// A quantity large enough to overflow is a metering bug; a wrapped charge would be
    /// nonsense, possibly negative, and V035's sign constraint would then reject it —
    /// leaving the event stuck with no explanation.
    #[test]
    fn an_absurd_quantity_saturates_rather_than_wrapping() {
        let c = charge_minor(i64::MAX, 1_000);
        assert!(c > 0, "a wrapped charge would be negative and silently rejected");
    }

    // ── The key ──────────────────────────────────────────────────────────────

    /// Prefixed rather than reused verbatim, so a ledger row cannot collide with some
    /// future non-charge movement that happens to quote a usage key.
    #[test]
    fn the_charge_key_derives_from_the_usage_key_but_is_not_it() {
        let usage = "egress:n1:e1:stream-b1:3000";
        let charge = charge_idempotency_key(usage);
        assert_ne!(charge, usage);
        assert!(charge.contains(usage), "and it stays traceable back to the event");
    }

    #[test]
    fn the_same_event_always_yields_the_same_charge_key() {
        assert_eq!(
            charge_idempotency_key("egress:n1:e1:s:1"),
            charge_idempotency_key("egress:n1:e1:s:1")
        );
    }
}
