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
use mm_core::fleet::funding::Payer;
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
    /// The broadcast's host: whose wallet pays, or whom the free tier served.
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
    /// Who carries the cost. Anything but `Wallet` is written already rated.
    pub payer: Payer,
}

/// Whose broadcast, and whether they can be billed for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastPayer {
    pub host_user_id: String,
    /// `None` = the host has no wallet: Open tier, so the platform carries the cost.
    pub wallet_currency: Option<String>,
}

/// One usage row to insert. Shared by the egress and GPU-minute meters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow<'a> {
    pub user_id: &'a str,
    pub broadcast_id: &'a str,
    pub unit: &'a str,
    pub quantity_milli: i64,
    pub mm_node_id: &'a str,
    pub idempotency_key: &'a str,
    pub occurred_at: DateTime<Utc>,
    pub payer: Payer,
    /// Egress-only provenance; `None` for GPU-minutes.
    pub egress_epoch: Option<&'a str>,
    pub cumulative_bytes: Option<i64>,
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
    /// `record_gpu_interval`'s compare-and-swap lost: another sweep moved the node's
    /// meter clock after this one read it. Nothing was recorded.
    #[error("meter clock moved under us; interval not recorded")]
    ClockMoved,
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

    /// The underlying pool, for callers (the relay-hour meter) that need plain
    /// queries this type does not wrap.
    pub fn pool(&self) -> &PgPool {
        &self.pool
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
    ///
    /// Every **other** row of the node is moved into `epoch` at zero. A restart
    /// zeroes every counter, so a source the new process has not served — or one
    /// whose position is held back this poll — is at zero in the new epoch. Left on
    /// the old epoch, those rows make the node's baseline mixed, the meter discards
    /// it, and every later poll becomes a first reading that bills nothing.
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
            let written_row = Self::insert_usage_in(
                &mut tx,
                &UsageRow {
                    user_id: &iv.user_id,
                    broadcast_id: &iv.broadcast_id,
                    unit: "egress_gb",
                    quantity_milli: iv.quantity_milli,
                    mm_node_id,
                    idempotency_key: &iv.idempotency_key,
                    occurred_at: iv.occurred_at,
                    payer: iv.payer,
                    egress_epoch: Some(&iv.epoch),
                    cumulative_bytes: Some(iv.cumulative_bytes),
                },
            )
            .await?;
            // Per interval, not a count taken afterwards. A replayed poll has some
            // of its intervals de-duplicated and some not, in no particular order,
            // so the accepted bytes can only be attributed here — summing the first
            // `events` intervals would charge the wrong ones to the meter.
            if written_row {
                written.events += 1;
                written.bytes += iv.bytes;
            }
        }

        // Before the positions below, which then overwrite the rows they name.
        // Within one epoch this matches nothing.
        sqlx::query(
            "UPDATE mm_egress_baseline
                SET epoch = $2, cumulative_bytes = 0, observed_at = $3
              WHERE mm_node_id = $1 AND epoch <> $2",
        )
        .bind(mm_node_id)
        .bind(epoch)
        .bind(observed_at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

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

    /// Whose broadcast this is, and the currency of their wallet if they have one.
    ///
    /// `None` only when the broadcast row is absent — there is nobody to attribute the
    /// usage to, and inventing one would charge the wrong person (RF-5). A host with
    /// no wallet is `Some` with `wallet_currency: None`: an Open-tier broadcaster
    /// whose egress the platform carries.
    pub async fn payer_for_broadcast(
        &self,
        broadcast_id: &str,
    ) -> Result<Option<BroadcastPayer>, MeteringDbError> {
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT s.host_user_id, w.currency
               FROM mm_streams s
               LEFT JOIN mm_broadcaster_wallet w ON w.user_id = s.host_user_id
              WHERE s.id = $1",
        )
        .bind(broadcast_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row.map(|(host_user_id, wallet_currency)| BroadcastPayer {
            host_user_id,
            wallet_currency,
        }))
    }

    /// Insert one usage row. Non-wallet payers are written ALREADY RATED — rated_at,
    /// a card version for provenance (0 when no card exists), no transaction — which
    /// V035's `usage_rating_is_all_or_nothing` permits. They are nobody's bill, and a
    /// row left unrated would sit in the queue and the backlog gauge forever.
    ///
    /// Returns whether a row was written: `false` is a replay de-duplicated by the
    /// idempotency key.
    pub async fn insert_usage_in(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        row: &UsageRow<'_>,
    ) -> Result<bool, MeteringDbError> {
        let res = sqlx::query(
            "INSERT INTO mm_usage_events
                 (user_id, broadcast_id, unit, quantity_milli, mm_node_id, idempotency_key,
                  occurred_at, egress_epoch, cumulative_bytes, payer,
                  rated_at, rate_card_version, transaction_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                     CASE WHEN $11 THEN $7 ELSE NULL END,
                     CASE WHEN $11 THEN COALESCE((SELECT MAX(version) FROM mm_rate_card), 0) ELSE NULL END,
                     NULL)
             ON CONFLICT (idempotency_key) DO NOTHING",
        )
        .bind(row.user_id)
        .bind(row.broadcast_id)
        .bind(row.unit)
        .bind(row.quantity_milli)
        .bind(row.mm_node_id)
        .bind(row.idempotency_key)
        .bind(row.occurred_at)
        .bind(row.egress_epoch)
        .bind(row.cumulative_bytes)
        .bind(row.payer.as_str())
        .bind(row.payer.is_rated_at_write())
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        Ok(res.rows_affected() > 0)
    }

    /// One unrated usage event, as the rater needs it.
    pub async fn unrated_events(
        &self,
        limit: i64,
    ) -> Result<Vec<PendingUsage>, MeteringDbError> {
        // Wallets that can be billed first, then blocked ones longest-blocked first;
        // within a wallet, oldest first — so a broadcaster whose balance runs out is
        // charged for what they used earliest.
        //
        // The blocked-last ordering is what stops unrateable usage from starving
        // everyone else (V038). Plain oldest-first let one broke broadcaster's events
        // fill every batch forever.
        //
        // The wallet's currency is joined in because a unit's price is per-currency
        // and rating against the wrong card is not a rounding error, it is a
        // different price.
        sqlx::query_as::<_, PendingUsage>(
            "SELECT e.id, e.user_id, e.broadcast_id, e.unit, e.quantity_milli,
                    e.idempotency_key, e.occurred_at, w.currency
               FROM mm_usage_events e
               JOIN mm_broadcaster_wallet w ON w.user_id = e.user_id
              WHERE e.rated_at IS NULL AND e.payer = 'wallet'
              ORDER BY w.rating_blocked_at NULLS FIRST, e.occurred_at, e.id
              LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Mark a wallet as unable to have its usage rated right now, moving its events
    /// behind every other wallet's (V038). Re-stamped on each failure, so blocked
    /// wallets rotate among themselves rather than one always leading.
    pub async fn block_rating(&self, user_id: &str, now: DateTime<Utc>) -> Result<(), MeteringDbError> {
        sqlx::query("UPDATE mm_broadcaster_wallet SET rating_blocked_at = $2 WHERE user_id = $1")
            .bind(user_id)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    /// Clear the mark once a charge for this wallet succeeds.
    pub async fn unblock_rating(&self, user_id: &str) -> Result<(), MeteringDbError> {
        sqlx::query(
            "UPDATE mm_broadcaster_wallet SET rating_blocked_at = NULL
              WHERE user_id = $1 AND rating_blocked_at IS NOT NULL",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
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
              WHERE e.rated_at IS NULL AND e.payer = 'wallet' AND w.user_id IS NULL",
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

/// `transcode_nodes_to_meter`'s raw row: id, funding, created_by, billing_started_at,
/// last_metered_at, gone_at.
type TranscodeNodeRow = (
    String,
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

/// A transcode node the GPU meter may bill, with what it needs to attribute the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeNodeToMeter {
    pub mm_node_id: String,
    pub funding: mm_core::fleet::funding::Funding,
    /// From the node id (`bc-{broadcast}-transcode-{n}`); `None` for test boots.
    pub broadcast_id: Option<String>,
    pub host_user_id: Option<String>,
    pub created_by: Option<String>,
    pub billing_started_at: Option<DateTime<Utc>>,
    pub last_metered_at: Option<DateTime<Utc>>,
    pub gone_at: Option<DateTime<Utc>>,
}

/// One GPU-minute usage row plus the clock advance that goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuUsage<'a> {
    pub mm_node_id: &'a str,
    pub user_id: &'a str,
    pub broadcast_id: &'a str,
    pub quantity_milli: i64,
    pub payer: Payer,
    pub idempotency_key: &'a str,
    pub occurred_at: DateTime<Utc>,
    /// The node's `last_metered_at` as the sweep read it — the compare half of the
    /// clock's compare-and-swap.
    pub metered_from: Option<DateTime<Utc>>,
    pub metered_to: DateTime<Utc>,
}

impl PgMeteringDb {
    /// Transcode nodes with something to bill: a machine exists (`billing_started_at`),
    /// and either it is not gone, or it went gone after the last sweep. A `gone` row
    /// with no `gone_at` (pre-V043) is left alone (RF-2): metering it to `now()` would
    /// bill a machine that stopped at an unknown time, forever.
    pub async fn transcode_nodes_to_meter(
        &self,
    ) -> Result<Vec<TranscodeNodeToMeter>, MeteringDbError> {
        let rows: Vec<TranscodeNodeRow> = sqlx::query_as(
            "SELECT n.mm_node_id, n.funding, n.created_by,
                        n.billing_started_at, n.last_metered_at, n.gone_at
                   FROM mm_fleet_nodes n
                  WHERE n.flavor = 'transcode'
                    AND n.billing_started_at IS NOT NULL
                    AND (n.state <> 'gone'
                         OR (n.gone_at IS NOT NULL
                             AND (n.last_metered_at IS NULL OR n.gone_at > n.last_metered_at)))
                  ORDER BY n.mm_node_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, funding, created_by, started, last, gone) in rows {
            let Some(funding) = mm_core::fleet::funding::Funding::parse(&funding) else {
                continue;
            };
            let broadcast_id = mm_core::fleet::planner::broadcast_of(&id).map(str::to_string);
            let host_user_id = match &broadcast_id {
                Some(bc) => sqlx::query_scalar::<_, String>(
                    "SELECT host_user_id FROM mm_streams WHERE id = $1",
                )
                .bind(bc)
                .fetch_optional(&self.pool)
                .await
                .map_err(db)?,
                None => None,
            };
            out.push(TranscodeNodeToMeter {
                mm_node_id: id,
                funding,
                broadcast_id,
                host_user_id,
                created_by,
                billing_started_at: started,
                last_metered_at: last,
                gone_at: gone,
            });
        }
        Ok(out)
    }

    /// Write one GPU-minute row and advance the node's meter clock, atomically.
    /// Returns whether the row was new (a replay advances the clock and writes nothing).
    ///
    /// The clock advance is compare-and-swap on `metered_from` (FR-305n): if another
    /// sweep moved `last_metered_at` since this one read it, the whole transaction —
    /// usage row included — is rolled back and `ClockMoved` returned. Without it two
    /// racing sweeps could move the clock backwards (double charge) or skip an interval.
    pub async fn record_gpu_interval(&self, u: &GpuUsage<'_>) -> Result<bool, MeteringDbError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let written = Self::insert_usage_in(
            &mut tx,
            &UsageRow {
                user_id: u.user_id,
                broadcast_id: u.broadcast_id,
                unit: "gpu_minute",
                quantity_milli: u.quantity_milli,
                mm_node_id: u.mm_node_id,
                idempotency_key: u.idempotency_key,
                occurred_at: u.occurred_at,
                payer: u.payer,
                egress_epoch: None,
                cumulative_bytes: None,
            },
        )
        .await?;
        let advanced = sqlx::query(
            "UPDATE mm_fleet_nodes SET last_metered_at = $2
              WHERE mm_node_id = $1 AND last_metered_at IS NOT DISTINCT FROM $3",
        )
        .bind(u.mm_node_id)
        .bind(u.metered_to)
        .bind(u.metered_from)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();
        if advanced == 0 {
            tx.rollback().await.map_err(db)?;
            return Err(MeteringDbError::ClockMoved);
        }
        tx.commit().await.map_err(db)?;
        Ok(written)
    }
}
