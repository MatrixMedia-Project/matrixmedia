//! Turning a node's egress counters into billable usage (FR-302a/b, design §17.2).
//!
//! mm-switch counts bytes per stream in memory and exposes cumulative totals. This
//! converts two readings into the usage between them — and its whole job is the
//! handful of ways that subtraction goes wrong.
//!
//! §17.2 sets the bar: **a lost usage event is unbilled revenue; a double-counted one
//! is an angry customer.** So every ambiguous case here resolves toward "report it and
//! bill nothing" rather than toward a number that might be invented.
//!
//! ## The four ways a naive delta is wrong
//!
//! | | |
//! |---|---|
//! | **The node restarted.** In-memory counters went to zero, so `current - previous` is negative | The reading carries an `epoch`. A different epoch means the whole current reading is new usage, and an unknown amount from the old epoch is **lost** — reported, not guessed at |
//! | **The first reading.** There is nothing to subtract from | It establishes a baseline and bills nothing. Billing a first cumulative reading would charge for everything since the node booted, which may already have been billed by a previous poller |
//! | **A counter went backwards within one epoch.** Impossible if the node is correct | So it means the node is not. Reported and **not billed**, because the alternative is a negative charge or a wildly wrong positive one |
//! | **A source vanished within one epoch.** Also impossible — departed viewers' bytes are folded into a running total | Same treatment |

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use mm_core::switch_client::SwitchEgress;

/// A reading reduced to what the delta needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressSnapshot {
    pub epoch: String,
    pub since: DateTime<Utc>,
    /// Cumulative bytes per source. `BTreeMap` so the derived usage list is ordered
    /// and an idempotency key built from it is stable.
    pub bytes: BTreeMap<String, i64>,
}

impl From<SwitchEgress> for EgressSnapshot {
    fn from(e: SwitchEgress) -> Self {
        let mut bytes = BTreeMap::new();
        for r in e.sources {
            // Sum rather than insert: a node that ever reported a source twice would
            // otherwise have one of them silently discarded.
            *bytes.entry(r.source).or_insert(0) += r.bytes;
        }
        Self {
            epoch: e.epoch,
            since: e.since,
            bytes,
        }
    }
}

/// Usage attributable to one source between two readings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub source: String,
    pub bytes: i64,
}

/// Something that happened which must reach a human or a counter, rather than being
/// resolved by guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeteringAnomaly {
    /// The node restarted between polls. Everything it delivered in the old epoch
    /// after the previous poll is **unrecoverable** — the counters were in memory.
    CountersReset {
        previous_epoch: String,
        current_epoch: String,
        /// When the new epoch began, so the size of the gap is at least bounded.
        new_epoch_since: DateTime<Utc>,
    },
    /// A cumulative counter decreased within one epoch, which the node should make
    /// impossible. Not billed.
    CounterWentBackwards {
        source: String,
        previous: i64,
        current: i64,
    },
    /// A source present in the previous reading is absent from this one, within the
    /// same epoch. Departed viewers' bytes are folded into a running total, so this
    /// should not happen. Not billed.
    SourceVanished { source: String, previous: i64 },
}

/// What one poll produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeteringResult {
    /// Billable usage since the previous reading.
    pub usage: Vec<Usage>,
    /// Anomalies to alert on. **Non-empty does not mean `usage` is unusable** — a
    /// backwards counter on one source does not invalidate the others.
    pub anomalies: Vec<MeteringAnomaly>,
    /// Whether this reading should replace the stored baseline. False only when the
    /// reading itself could not be trusted at all.
    pub advance_baseline: bool,
}

/// Usage between `previous` and `current`.
///
/// `previous` is `None` on the first poll of a node, which establishes a baseline and
/// bills nothing — billing a first cumulative reading would charge for everything
/// since the node booted, which a previous poller may already have billed.
pub fn egress_delta(previous: Option<&EgressSnapshot>, current: &EgressSnapshot) -> MeteringResult {
    let Some(previous) = previous else {
        return MeteringResult {
            usage: Vec::new(),
            anomalies: Vec::new(),
            advance_baseline: true,
        };
    };

    if previous.epoch != current.epoch {
        // The node restarted. Everything in the current reading accrued inside the
        // NEW epoch, so all of it is new usage and billing it is correct — what is
        // lost is whatever the old epoch accrued after the previous poll, which was
        // only ever in that process's memory.
        return MeteringResult {
            usage: current
                .bytes
                .iter()
                .filter(|(_, b)| **b > 0)
                .map(|(s, b)| Usage {
                    source: s.clone(),
                    bytes: *b,
                })
                .collect(),
            anomalies: vec![MeteringAnomaly::CountersReset {
                previous_epoch: previous.epoch.clone(),
                current_epoch: current.epoch.clone(),
                new_epoch_since: current.since,
            }],
            advance_baseline: true,
        };
    }

    let mut usage = Vec::new();
    let mut anomalies = Vec::new();

    for (source, &now) in &current.bytes {
        let before = previous.bytes.get(source).copied().unwrap_or(0);
        if now < before {
            anomalies.push(MeteringAnomaly::CounterWentBackwards {
                source: source.clone(),
                previous: before,
                current: now,
            });
            continue;
        }
        let delta = now - before;
        if delta > 0 {
            usage.push(Usage {
                source: source.clone(),
                bytes: delta,
            });
        }
    }

    // A source that was there and is not any more, inside one epoch.
    for (source, &before) in &previous.bytes {
        if !current.bytes.contains_key(source) {
            anomalies.push(MeteringAnomaly::SourceVanished {
                source: source.clone(),
                previous: before,
            });
        }
    }

    MeteringResult {
        usage,
        anomalies,
        advance_baseline: true,
    }
}

/// Bytes to thousandths of a gigabyte, which is what `mm_usage_events.quantity_milli`
/// holds for the `egress_gb` unit.
///
/// **Truncates**, and that is deliberate: rounding up would bill a fraction of a
/// gigabyte as a whole one, and at poll frequency that fraction recurs on every
/// interval. Truncation loses at most one thousandth of a gigabyte — a megabyte — per
/// poll, which is the side to be wrong on.
///
/// 1 GB = 1_000_000_000 bytes, the decimal definition, because that is what every
/// provider's egress invoice uses.
pub fn bytes_to_milli_gb(bytes: i64) -> i64 {
    if bytes <= 0 {
        return 0;
    }
    bytes / 1_000_000
}

/// A stable idempotency key for one source's usage in one poll.
///
/// Built from the node, the epoch and the **cumulative counter value at the end of
/// the interval** — not from a timestamp, and not from the delta. A retried poll of
/// the same reading produces the same key and is therefore a no-op at the database's
/// UNIQUE constraint (§17.2), which is what makes the whole meter retry-safe.
pub fn usage_idempotency_key(node_id: &str, epoch: &str, source: &str, cumulative: i64) -> String {
    format!("egress:{node_id}:{epoch}:{source}:{cumulative}")
}

/// Convenience: the cumulative values a caller needs for the keys above.
pub fn cumulative_for(snapshot: &EgressSnapshot) -> HashMap<&str, i64> {
    snapshot
        .bytes
        .iter()
        .map(|(s, b)| (s.as_str(), *b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(epoch: &str, pairs: &[(&str, i64)]) -> EgressSnapshot {
        EgressSnapshot {
            epoch: epoch.to_string(),
            since: DateTime::from_timestamp(1_700_000_000, 0).unwrap().to_utc(),
            bytes: pairs
                .iter()
                .map(|(s, b)| ((*s).to_string(), *b))
                .collect(),
        }
    }

    // ── The ordinary case ────────────────────────────────────────────────────

    #[test]
    fn usage_is_the_difference_within_one_epoch() {
        let before = snap("e1", &[("stream-b1", 1_000), ("stream-b2", 500)]);
        let after = snap("e1", &[("stream-b1", 3_000), ("stream-b2", 500)]);

        let r = egress_delta(Some(&before), &after);
        assert_eq!(
            r.usage,
            vec![Usage {
                source: "stream-b1".into(),
                bytes: 2_000
            }],
            "a source that did not move must not produce a zero-byte usage event"
        );
        assert!(r.anomalies.is_empty());
        assert!(r.advance_baseline);
    }

    #[test]
    fn a_source_seen_for_the_first_time_bills_its_whole_counter() {
        let before = snap("e1", &[("stream-b1", 1_000)]);
        let after = snap("e1", &[("stream-b1", 1_000), ("stream-new", 700)]);

        let r = egress_delta(Some(&before), &after);
        assert_eq!(
            r.usage,
            vec![Usage {
                source: "stream-new".into(),
                bytes: 700
            }],
            "a new source starts from zero, so its whole counter is new usage"
        );
    }

    // ── The first poll ───────────────────────────────────────────────────────

    /// Billing a first cumulative reading would charge for everything since the node
    /// booted — which a previous poller may already have billed.
    #[test]
    fn the_first_reading_establishes_a_baseline_and_bills_nothing() {
        let r = egress_delta(None, &snap("e1", &[("stream-b1", 9_999_999)]));
        assert!(r.usage.is_empty(), "the first reading must not be billed");
        assert!(r.anomalies.is_empty(), "and it is not an anomaly either");
        assert!(r.advance_baseline);
    }

    // ── The restart, which is the normal end of an ephemeral node's life ──────

    /// THE ONE THAT MAKES THE METER SAFE. A naive `current - previous` across a
    /// restart is negative, and code that clamps it to zero silently bills nothing
    /// for a node's entire second life.
    #[test]
    fn an_epoch_change_bills_the_new_reading_in_full_and_reports_the_loss() {
        let before = snap("e1", &[("stream-b1", 10_000)]);
        let after = snap("e2", &[("stream-b1", 300)]);

        let r = egress_delta(Some(&before), &after);
        assert_eq!(
            r.usage,
            vec![Usage {
                source: "stream-b1".into(),
                bytes: 300
            }],
            "everything in the new epoch accrued after the restart, so all of it is \
             new usage — clamping a negative delta to zero would bill none of it"
        );
        assert_eq!(r.anomalies.len(), 1);
        assert!(matches!(
            r.anomalies[0],
            MeteringAnomaly::CountersReset { .. }
        ));
        assert!(r.advance_baseline);
    }

    /// The anomaly must name both epochs and when the new one began, or nobody can
    /// bound how much revenue was lost.
    #[test]
    fn the_reset_anomaly_bounds_the_loss() {
        let before = snap("e1", &[("stream-b1", 10_000)]);
        let after = snap("e2", &[("stream-b1", 300)]);
        let r = egress_delta(Some(&before), &after);
        match &r.anomalies[0] {
            MeteringAnomaly::CountersReset {
                previous_epoch,
                current_epoch,
                new_epoch_since,
            } => {
                assert_eq!(previous_epoch, "e1");
                assert_eq!(current_epoch, "e2");
                assert_eq!(*new_epoch_since, after.since);
            }
            other => panic!("expected CountersReset, got {other:?}"),
        }
    }

    // ── Things that should be impossible, treated as such ────────────────────

    /// A cumulative counter cannot decrease within one epoch. If it does, the node is
    /// wrong — and billing a negative number, or the absolute value, would be worse
    /// than billing nothing and saying so.
    #[test]
    fn a_counter_going_backwards_is_reported_and_not_billed() {
        let before = snap("e1", &[("stream-b1", 5_000)]);
        let after = snap("e1", &[("stream-b1", 4_000)]);

        let r = egress_delta(Some(&before), &after);
        assert!(r.usage.is_empty(), "a negative delta must not become a charge");
        assert_eq!(
            r.anomalies,
            vec![MeteringAnomaly::CounterWentBackwards {
                source: "stream-b1".into(),
                previous: 5_000,
                current: 4_000,
            }]
        );
    }

    /// One bad source must not discard the others: a broadcaster whose counters are
    /// fine still owes for what they used.
    #[test]
    fn one_anomalous_source_does_not_invalidate_the_others() {
        let before = snap("e1", &[("bad", 5_000), ("good", 1_000)]);
        let after = snap("e1", &[("bad", 4_000), ("good", 2_500)]);

        let r = egress_delta(Some(&before), &after);
        assert_eq!(
            r.usage,
            vec![Usage {
                source: "good".into(),
                bytes: 1_500
            }]
        );
        assert_eq!(r.anomalies.len(), 1);
    }

    /// Departed viewers' bytes are folded into a running total, so a source cannot
    /// disappear inside one epoch. If one does, say so rather than quietly forgetting
    /// that a broadcast existed.
    #[test]
    fn a_vanished_source_is_reported() {
        let before = snap("e1", &[("stream-b1", 5_000), ("stream-b2", 100)]);
        let after = snap("e1", &[("stream-b1", 5_000)]);

        let r = egress_delta(Some(&before), &after);
        assert_eq!(
            r.anomalies,
            vec![MeteringAnomaly::SourceVanished {
                source: "stream-b2".into(),
                previous: 100,
            }]
        );
    }

    // ── Units ────────────────────────────────────────────────────────────────

    /// Truncates, because rounding up would bill a fraction of a gigabyte as a whole
    /// one — and at poll frequency that fraction recurs on every interval.
    #[test]
    fn bytes_convert_to_thousandths_of_a_gigabyte_by_truncation() {
        assert_eq!(bytes_to_milli_gb(1_000_000_000), 1_000, "1 GB = 1000 milli-GB");
        assert_eq!(bytes_to_milli_gb(1_000_000), 1, "1 MB = 1 milli-GB");
        assert_eq!(bytes_to_milli_gb(999_999), 0, "just under a megabyte rounds DOWN");
        assert_eq!(bytes_to_milli_gb(1_999_999), 1);
        assert_eq!(bytes_to_milli_gb(0), 0);
        assert_eq!(bytes_to_milli_gb(-5), 0, "a negative can never become a charge");
    }

    /// Decimal gigabytes, because that is what an egress invoice uses. Using 2^30
    /// here would under-bill by 7.4% against every provider.
    #[test]
    fn a_gigabyte_is_the_decimal_one_that_invoices_use() {
        assert_eq!(bytes_to_milli_gb(1_073_741_824), 1_073, "2^30 bytes is 1.073 GB");
    }

    // ── Idempotency ──────────────────────────────────────────────────────────

    /// The key is built from the CUMULATIVE value, not a timestamp and not the delta.
    /// A retried poll of the same reading therefore produces the same key and is a
    /// no-op at the database's UNIQUE constraint — which is what makes the meter
    /// retry-safe rather than merely retried.
    #[test]
    fn the_same_reading_always_yields_the_same_idempotency_key() {
        let a = usage_idempotency_key("bc-b1-fanout-0", "e1", "stream-b1", 3_000);
        let b = usage_idempotency_key("bc-b1-fanout-0", "e1", "stream-b1", 3_000);
        assert_eq!(a, b);
    }

    #[test]
    fn the_key_changes_with_every_field_that_should_change_it() {
        let base = usage_idempotency_key("n1", "e1", "s1", 100);
        assert_ne!(base, usage_idempotency_key("n2", "e1", "s1", 100), "node");
        assert_ne!(base, usage_idempotency_key("n1", "e2", "s1", 100), "epoch");
        assert_ne!(base, usage_idempotency_key("n1", "e1", "s2", 100), "source");
        assert_ne!(base, usage_idempotency_key("n1", "e1", "s1", 200), "cumulative");
    }

    /// Two nodes serving the same broadcast must produce different keys, or one of
    /// their usage events would be silently dropped as a duplicate and that node's
    /// delivery would go unbilled.
    #[test]
    fn two_nodes_serving_one_broadcast_do_not_collide() {
        assert_ne!(
            usage_idempotency_key("bc-b1-fanout-0", "e1", "stream-b1", 500),
            usage_idempotency_key("bc-b1-fanout-1", "e1", "stream-b1", 500)
        );
    }

    // ── Parsing the node's response ──────────────────────────────────────────

    #[test]
    fn a_duplicated_source_in_one_response_is_summed_not_dropped() {
        use mm_core::switch_client::{SwitchEgress, SwitchEgressReading};
        let raw = SwitchEgress {
            epoch: "e1".into(),
            since: DateTime::from_timestamp(1_700_000_000, 0).unwrap().to_utc(),
            overhead_bytes_per_packet: 38,
            sources: vec![
                SwitchEgressReading {
                    source: "stream-b1".into(),
                    bytes: 100,
                },
                SwitchEgressReading {
                    source: "stream-b1".into(),
                    bytes: 50,
                },
            ],
        };
        let snapshot = EgressSnapshot::from(raw);
        assert_eq!(
            snapshot.bytes.get("stream-b1"),
            Some(&150),
            "inserting rather than summing would silently discard one of them"
        );
    }
}
