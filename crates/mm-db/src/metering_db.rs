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

#[derive(Debug, thiserror::Error)]
pub enum MeteringDbError {
    #[error("database error: {0}")]
    Db(String),
}

fn db(e: sqlx::Error) -> MeteringDbError {
    MeteringDbError::Db(e.to_string())
}

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
    ) -> Result<usize, MeteringDbError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let mut written = 0usize;

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
            written += res.rows_affected() as usize;
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
