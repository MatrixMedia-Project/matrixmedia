//! The egress meter's persistence (FR-302a/b, WS-D).
//!
//! Two things, and the relationship between them is the whole correctness of the
//! meter:
//!
//!  * `mm_egress_baseline` — the last counter reading per (node, source).
//!  * `mm_usage_events` — the billable deltas derived from consecutive readings.
//!
//! [`PgMeteringDb::record_interval`] writes both **in one transaction**. If the
//! usage events landed and the baseline did not, the next poll would re-derive the
//! same delta — harmless, because the idempotency key makes it a no-op — but if the
//! baseline advanced and the events did not, that interval's revenue is **gone**, and
//! nothing would ever notice. One transaction removes the question.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// One (node, source) counter position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressBaseline {
    pub source: String,
    pub epoch: String,
    pub cumulative_bytes: i64,
    pub observed_at: DateTime<Utc>,
}

/// What a `record_interval` call actually wrote.
///
/// Both numbers, because they answer different questions and only one of them can
/// be derived from the other's inputs: `events` is how many rows are now on the
/// invoice, and `bytes` is what the provider should be billing us for the same
/// traffic. The gap between `bytes` and the provider's figure is the per-packet
/// overhead estimate's error.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub events: usize,
    /// `i64` like every other byte count here, because that is what the column is.
    pub bytes: i64,
}

/// A currency's current rate card: its version, and a price per unit.
///
/// The version travels with the prices because an event records which card priced it,
/// and that is what stops history being re-rated against a newer card (FR-306).
pub type RateCard = (i32, Vec<(String, i64)>);

/// A billable interval for one source, ready to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressInterval {
    /// `stream-{broadcast_id}` as the node reported it.
    pub source: String,
    /// Whose wallet pays.
    pub user_id: String,
    pub broadcast_id: String,
    /// Bytes attributable to this interval.
    pub bytes: i64,
    /// Thousandths of a gigabyte, already converted — so the conversion is tested
    /// once, in `mm_fleet::metering`, rather than re-derived here.
    pub quantity_milli: i64,
    /// The counter value this interval ends at. Both the idempotency key and the
    /// new baseline derive from it.
    pub cumulative_bytes: i64,
    pub epoch: String,
    pub idempotency_key: String,
    pub occurred_at: DateTime<Utc>,
}

/// An unrated usage event, with the facts the rater needs to price it.
///
/// Mapped by **column name** (`FromRow`), not by position. The positional form was an
/// eight-element tuple in which `user_id` and `broadcast_id` are adjacent `String`s:
/// transposing them compiles, passes every type check, and bills the right amount to
/// the wrong person.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PendingUsage {
    pub id: i64,
    pub user_id: String,
    pub broadcast_id: String,
    pub unit: String,
    /// Thousandths of a unit.
    pub quantity_milli: i64,
    /// The METER's key for this event. The charge's key derives from it, so a
    /// re-rated event charges once.
    pub idempotency_key: String,
    pub occurred_at: DateTime<Utc>,
    /// The wallet's currency, joined in: a unit's price is per-currency, and rating
    /// against the wrong card is not a rounding error but a different price.
    pub currency: String,
}

#[derive(Debug, thiserror::Error)]
pub enum MeteringDbError {
    #[error("database error: {0}")]
    Db(String),
}

fn db(e: sqlx::Error) -> MeteringDbError {
    MeteringDbError::Db(e.to_string())
}

#[derive(Clone)]
pub struct PgMeteringDb {
    pool: PgPool,
}

impl PgMeteringDb {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Every stored counter position for one node.
    pub async fn baselines_for_node(
        &self,
        mm_node_id: &str,
    ) -> Result<Vec<EgressBaseline>, MeteringDbError> {
        let rows: Vec<(String, String, i64, DateTime<Utc>)> = sqlx::query_as(
            "SELECT source, epoch, cumulative_bytes, observed_at
               FROM mm_egress_baseline
              WHERE mm_node_id = $1
              ORDER BY source",
        )
        .bind(mm_node_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        Ok(rows
            .into_iter()
            .map(
                |(source, epoch, cumulative_bytes, observed_at)| EgressBaseline {
                    source,
                    epoch,
                    cumulative_bytes,
                    observed_at,
                },
            )
            .collect())
    }

    /// Write one poll's worth of usage and advance the baseline, atomically.
    ///
    /// `intervals` may be empty — a node that delivered nothing since the last poll
    /// still advances its baseline's `observed_at`, which is what distinguishes
    /// "quiet" from "the meter has stopped reporting".
    ///
    /// Returns how many usage events were newly written. A number lower than
    /// `intervals.len()` means some were replays, which is a success: the
    /// idempotency constraint did its job.
    ///
    /// `positions` is **every** source in the reading with its cumulative value,
    /// including those whose delta was zero. Passed separately from `intervals`
    /// because the baseline must advance for a source even when it produced no usage
    /// — otherwise a source that goes quiet keeps re-deriving the same zero delta
    /// forever and its `observed_at` never moves, so it looks like a stalled meter.
    pub async fn record_interval(
        &self,
        mm_node_id: &str,
        epoch: &str,
        observed_at: DateTime<Utc>,
        intervals: &[EgressInterval],
        positions: &[(String, i64)],
    ) -> Result<Recorded, MeteringDbError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let mut written = Recorded::default();

        for iv in intervals {
            // ON CONFLICT DO NOTHING, not an error: a replayed poll must be a no-op,
            // and distinguishing "already billed" from "failed to bill" by catching a
            // constraint violation is exactly the fragile path V035 exists to avoid.
            let res = sqlx::query(
                "INSERT INTO mm_usage_events
                     (user_id, broadcast_id, unit, quantity_milli, mm_node_id,
                      idempotency_key, occurred_at, egress_epoch, cumulative_bytes)
                 VALUES ($1, $2, 'egress_gb', $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (idempotency_key) DO NOTHING",
            )
            .bind(&iv.user_id)
            .bind(&iv.broadcast_id)
            .bind(iv.quantity_milli)
            .bind(mm_node_id)
            .bind(&iv.idempotency_key)
            .bind(iv.occurred_at)
            .bind(&iv.epoch)
            .bind(iv.cumulative_bytes)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            // Per interval, not a count taken afterwards. A replayed poll has some
            // of its intervals de-duplicated and some not, in no particular order,
            // so the accepted bytes can only be attributed here — summing the first
            // `events` intervals would charge the wrong ones to the meter.
            if res.rows_affected() > 0 {
                written.events += 1;
                written.bytes += iv.bytes;
            }
        }

        for (source, cumulative) in positions {
            sqlx::query(
                "INSERT INTO mm_egress_baseline
                     (mm_node_id, source, epoch, cumulative_bytes, observed_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (mm_node_id, source) DO UPDATE SET
                     epoch            = EXCLUDED.epoch,
                     cumulative_bytes = EXCLUDED.cumulative_bytes,
                     observed_at      = EXCLUDED.observed_at",
            )
            .bind(mm_node_id)
            .bind(source)
            .bind(epoch)
            .bind(cumulative)
            .bind(observed_at)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }

        tx.commit().await.map_err(db)?;
        Ok(written)
    }

    /// Count unrated usage events.
    ///
    /// The size of the bill that enabling billing would send. Cheap enough to run
    /// every tick: V035's partial index on `rated_at IS NULL` is exactly this
    /// predicate, so it is an index-only count of the queue, not a table scan.
    pub async fn unrated_count(&self) -> Result<i64, MeteringDbError> {
        sqlx::query_scalar("SELECT count(*) FROM mm_usage_events WHERE rated_at IS NULL")
            .fetch_one(&self.pool)
            .await
            .map_err(db)
    }

    /// Whose wallet pays for a broadcast, and in which currency.
    ///
    /// `None` when the broadcast or the wallet is absent — either way there is nobody
    /// to bill, and inventing a payer would charge the wrong person.
    pub async fn payer_for_broadcast(
        &self,
        broadcast_id: &str,
    ) -> Result<Option<(String, String)>, MeteringDbError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT s.host_user_id, w.currency
               FROM mm_streams s
               JOIN mm_broadcaster_wallet w ON w.user_id = s.host_user_id
              WHERE s.id = $1",
        )
        .bind(broadcast_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row)
    }

    /// One unrated usage event, as the rater needs it.
    pub async fn unrated_events(
        &self,
        limit: i64,
    ) -> Result<Vec<PendingUsage>, MeteringDbError> {
        // Oldest first: a broadcaster whose balance runs out should be charged for
        // what they used earliest, not for whichever event the planner happened to
        // read first.
        //
        // The wallet's currency is joined in because a unit's price is per-currency
        // and rating against the wrong card is not a rounding error, it is a
        // different price.
        sqlx::query_as::<_, PendingUsage>(
            "SELECT e.id, e.user_id, e.broadcast_id, e.unit, e.quantity_milli,
                    e.idempotency_key, e.occurred_at, w.currency
               FROM mm_usage_events e
               JOIN mm_broadcaster_wallet w ON w.user_id = e.user_id
              WHERE e.rated_at IS NULL
              ORDER BY e.occurred_at
              LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Unrated events whose owner has **no wallet**.
    ///
    /// `unrated_events` joins the wallet, so these would otherwise be invisible —
    /// sitting in the queue forever while the join silently skipped them. Unbilled
    /// revenue that no counter shows is exactly what §17.2 warns about.
    pub async fn unrated_without_wallet(&self) -> Result<Vec<String>, MeteringDbError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT e.user_id
               FROM mm_usage_events e
          LEFT JOIN mm_broadcaster_wallet w ON w.user_id = e.user_id
              WHERE e.rated_at IS NULL AND w.user_id IS NULL",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows.into_iter().map(|(u,)| u).collect())
    }

    /// Prices for one currency, at its newest version.
    ///
    /// Newest, because a NEW charge is rated against the current card; a rated event
    /// keeps the version it was rated with, which is what stops history being
    /// re-priced (§17.5).
    pub async fn current_prices(
        &self,
        currency: &str,
    ) -> Result<Option<RateCard>, MeteringDbError> {
        let rows: Vec<(i32, String, i64)> = sqlx::query_as(
            "SELECT version, unit, price_minor
               FROM mm_rate_card
              WHERE currency = $1
                AND version = (SELECT MAX(version) FROM mm_rate_card WHERE currency = $1)",
        )
        .bind(currency)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        if rows.is_empty() {
            return Ok(None);
        }
        let version = rows[0].0;
        Ok(Some((
            version,
            rows.into_iter().map(|(_, u, p)| (u, p)).collect(),
        )))
    }

    /// The ledger row for an idempotency key, if one exists.
    ///
    /// Needed because the charge is idempotent but marking the event rated is a
    /// separate statement: if the charge landed and the mark did not, the next pass
    /// re-charges, gets `AlreadyApplied`, and has to recover the transaction id from
    /// here rather than leaving the event unrated forever.
    pub async fn transaction_id_for_key(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<i64>, MeteringDbError> {
        sqlx::query_scalar("SELECT id FROM mm_wallet_transactions WHERE idempotency_key = $1")
            .bind(idempotency_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    /// Mark an event rated. All three fields together — V035's CHECK refuses a
    /// half-rated row, because one would either bill twice or never bill depending on
    /// which half the rater trusted.
    pub async fn mark_rated(
        &self,
        event_id: i64,
        rate_card_version: i32,
        transaction_id: i64,
        rated_at: DateTime<Utc>,
    ) -> Result<(), MeteringDbError> {
        sqlx::query(
            "UPDATE mm_usage_events
                SET rated_at = $2, rate_card_version = $3, transaction_id = $4
              WHERE id = $1 AND rated_at IS NULL",
        )
        .bind(event_id)
        .bind(rated_at)
        .bind(rate_card_version)
        .bind(transaction_id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// Mark an event rated with **no** transaction, for usage that priced to zero.
    ///
    /// V035's CHECK allows `rated_at` plus a version without a transaction id — that
    /// shape IS "priced at nothing". Letting such an event leave the queue is the
    /// point: otherwise every pass re-reads it forever.
    pub async fn mark_rated_without_charge(
        &self,
        event_id: i64,
        rate_card_version: i32,
        rated_at: DateTime<Utc>,
    ) -> Result<(), MeteringDbError> {
        sqlx::query(
            "UPDATE mm_usage_events
                SET rated_at = $2, rate_card_version = $3
              WHERE id = $1 AND rated_at IS NULL",
        )
        .bind(event_id)
        .bind(rated_at)
        .bind(rate_card_version)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// Nodes whose meter has not reported since `cutoff`.
    ///
    /// A stalled meter is unbilled revenue and looks identical to a quiet broadcast,
    /// so it has to be asked about rather than waited for.
    pub async fn stale_meters(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<(String, DateTime<Utc>)>, MeteringDbError> {
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT mm_node_id, MAX(observed_at) AS last_seen
               FROM mm_egress_baseline
              GROUP BY mm_node_id
             HAVING MAX(observed_at) < $1
              ORDER BY last_seen",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows)
    }
}
