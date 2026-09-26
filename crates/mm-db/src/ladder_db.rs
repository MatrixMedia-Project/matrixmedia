//! Persistence for the demotion ladder (WS-D, design §17.4, V037).
//!
//! The ladder function in `mm-core` is pure and knows nothing between evaluations.
//! This is its memory: which rung each broadcast is on, how long a recovery has been
//! building, and the append-only record of every transition with the statement of
//! reasons CR-604 requires.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// A broadcast's persisted rung.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DemotionState {
    pub broadcast_id: String,
    pub step: String,
    pub entered_at: DateTime<Utc>,
    pub milder_streak: i32,
    pub actuated: bool,
}

/// A live broadcast the ladder should evaluate.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LiveBroadcast {
    pub broadcast_id: String,
    pub user_id: String,
}

/// One transition, as written to the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemotionTransition {
    pub broadcast_id: String,
    pub user_id: String,
    pub from_step: String,
    pub to_step: String,
    pub balance_minor: i64,
    pub projected_cost_minor: i64,
    pub statement: String,
    pub actuated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LadderDbError {
    #[error("ladder db: {0}")]
    Db(String),
}

fn db(e: sqlx::Error) -> LadderDbError {
    LadderDbError::Db(e.to_string())
}

#[derive(Clone)]
pub struct PgLadderDb {
    pool: PgPool,
}

impl PgLadderDb {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Broadcasts the ladder should evaluate: live, with a host.
    ///
    /// Ordered by id so a run is reproducible and a partial run (one that hit an
    /// error part-way) covers the same prefix next time rather than a random one.
    pub async fn live_broadcasts(&self, limit: i64) -> Result<Vec<LiveBroadcast>, LadderDbError> {
        sqlx::query_as::<_, LiveBroadcast>(
            "SELECT id AS broadcast_id, host_user_id AS user_id
               FROM mm_streams
              WHERE status = 'active'
              ORDER BY id
              LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    pub async fn state(&self, broadcast_id: &str) -> Result<Option<DemotionState>, LadderDbError> {
        sqlx::query_as::<_, DemotionState>(
            "SELECT broadcast_id, step, entered_at, milder_streak, actuated
               FROM mm_broadcast_demotion
              WHERE broadcast_id = $1",
        )
        .bind(broadcast_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Record an evaluation that did **not** change the rung.
    ///
    /// Only `evaluated_at`, the streak and the observed numbers move. `entered_at`
    /// deliberately does not: the difference between the two is what says "degraded
    /// for forty minutes" rather than merely "degraded", and touching it on every
    /// tick would erase exactly that.
    pub async fn touch(
        &self,
        broadcast_id: &str,
        step: &str,
        milder_streak: i32,
        balance_minor: i64,
        projected_cost_minor: i64,
        now: DateTime<Utc>,
    ) -> Result<(), LadderDbError> {
        sqlx::query(
            "INSERT INTO mm_broadcast_demotion
                 (broadcast_id, step, entered_at, evaluated_at, balance_minor,
                  projected_cost_minor, milder_streak, actuated)
             VALUES ($1, $2, $6, $6, $4, $5, $3, FALSE)
             ON CONFLICT (broadcast_id) DO UPDATE SET
                 evaluated_at         = EXCLUDED.evaluated_at,
                 milder_streak        = EXCLUDED.milder_streak,
                 balance_minor        = EXCLUDED.balance_minor,
                 projected_cost_minor = EXCLUDED.projected_cost_minor",
        )
        .bind(broadcast_id)
        .bind(step)
        .bind(milder_streak)
        .bind(balance_minor)
        .bind(projected_cost_minor)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// Move a broadcast to a new rung **and** write its statement of reasons, in one
    /// transaction.
    ///
    /// One transaction because CR-604 requires the statement by the time the
    /// restriction takes effect. Two statements could leave a restriction applied
    /// with no record of why — which is the failure CR-604 exists to prevent, and it
    /// would be invisible.
    pub async fn apply_transition(
        &self,
        t: &DemotionTransition,
        milder_streak: i32,
        now: DateTime<Utc>,
    ) -> Result<(), LadderDbError> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        sqlx::query(
            "INSERT INTO mm_broadcast_demotion
                 (broadcast_id, step, entered_at, evaluated_at, balance_minor,
                  projected_cost_minor, milder_streak, actuated)
             VALUES ($1, $2, $7, $7, $4, $5, $3, $6)
             ON CONFLICT (broadcast_id) DO UPDATE SET
                 step                 = EXCLUDED.step,
                 entered_at           = EXCLUDED.entered_at,
                 evaluated_at         = EXCLUDED.evaluated_at,
                 balance_minor        = EXCLUDED.balance_minor,
                 projected_cost_minor = EXCLUDED.projected_cost_minor,
                 milder_streak        = EXCLUDED.milder_streak,
                 actuated             = EXCLUDED.actuated",
        )
        .bind(&t.broadcast_id)
        .bind(&t.to_step)
        .bind(milder_streak)
        .bind(t.balance_minor)
        .bind(t.projected_cost_minor)
        .bind(t.actuated)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        sqlx::query(
            "INSERT INTO mm_demotion_events
                 (broadcast_id, user_id, from_step, to_step, balance_minor,
                  projected_cost_minor, statement, actuated, occurred_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&t.broadcast_id)
        .bind(&t.user_id)
        .bind(&t.from_step)
        .bind(&t.to_step)
        .bind(t.balance_minor)
        .bind(t.projected_cost_minor)
        .bind(&t.statement)
        .bind(t.actuated)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        tx.commit().await.map_err(db)?;
        Ok(())
    }

    /// How many broadcasts sit on each rung, for the gauge.
    pub async fn step_counts(&self) -> Result<Vec<(String, i64)>, LadderDbError> {
        sqlx::query_as(
            "SELECT step, count(*)::BIGINT FROM mm_broadcast_demotion GROUP BY step",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Statements of reasons nobody has delivered yet (CR-604).
    ///
    /// The delivery channel is not built. This exists so that gap is a **number**
    /// rather than an assumption — an undelivered statement is a compliance debt,
    /// and one that cannot be counted is one nobody will pay.
    pub async fn undelivered_statements(&self) -> Result<i64, LadderDbError> {
        sqlx::query_scalar(
            "SELECT count(*) FROM mm_demotion_events WHERE delivered_at IS NULL AND actuated",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)
    }
}
