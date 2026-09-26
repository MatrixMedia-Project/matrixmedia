//! Persistence for the demotion ladder (WS-D, design §17.4, V037).
//!
//! The ladder function in `mm-core` is pure and knows nothing between evaluations.
//! This is its memory: which rung each broadcast is targeted for, which rung's
//! effects have actually been applied, how long a recovery has been building, and the
//! append-only record of every decision and every action with the statement of
//! reasons CR-604 requires.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// A broadcast's persisted ladder state.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DemotionState {
    pub broadcast_id: String,
    /// What the ladder has decided.
    pub target_step: String,
    /// What has actually been done. See V037 for why these are separate.
    pub applied_step: String,
    pub entered_at: DateTime<Utc>,
    pub milder_streak: i32,
}

/// A live broadcast the ladder should evaluate.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LiveBroadcast {
    pub broadcast_id: String,
    pub user_id: String,
}

/// Which fact an event row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// The target rung changed. Internal; in observe mode it is a forecast.
    Decision,
    /// The applied rung changed: a restriction took effect or was lifted. The only
    /// kind that is a statement of reasons to the broadcaster (CR-604).
    Applied,
}

impl EventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Applied => "applied",
        }
    }
}

/// One row for the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemotionEvent {
    pub kind: EventKind,
    pub from_step: String,
    pub to_step: String,
    pub statement: String,
}

/// Everything one evaluation of one broadcast writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub broadcast_id: String,
    pub user_id: String,
    pub target_step: String,
    pub applied_step: String,
    /// Did the TARGET change? Only then does `entered_at` move.
    pub target_changed: bool,
    pub milder_streak: i32,
    pub balance_minor: i64,
    pub projected_cost_minor: i64,
    pub events: Vec<DemotionEvent>,
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

    /// One page of live broadcasts, strictly after `after` in id order.
    ///
    /// **Keyset pagination, and the caller walks every page.** The first version took
    /// `ORDER BY id LIMIT n` once per tick, which evaluates the same first `n`
    /// broadcasts forever and never reaches the rest. Pagination lets a tick cover
    /// every live broadcast while bounding each query.
    pub async fn live_broadcasts_after(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<LiveBroadcast>, LadderDbError> {
        sqlx::query_as::<_, LiveBroadcast>(
            "SELECT id AS broadcast_id, host_user_id AS user_id
               FROM mm_streams
              WHERE status = 'active'
                AND ($1::TEXT IS NULL OR id > $1)
              ORDER BY id
              LIMIT $2",
        )
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    pub async fn state(&self, broadcast_id: &str) -> Result<Option<DemotionState>, LadderDbError> {
        sqlx::query_as::<_, DemotionState>(
            "SELECT broadcast_id, target_step, applied_step, entered_at, milder_streak
               FROM mm_broadcast_demotion
              WHERE broadcast_id = $1",
        )
        .bind(broadcast_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    /// Write one evaluation: the state row and its events, in **one transaction**.
    ///
    /// One transaction because CR-604 requires the statement by the time the
    /// restriction takes effect. Two statements could leave the applied rung moved
    /// with no record of why — the failure CR-604 exists to prevent, and an
    /// invisible one.
    ///
    /// `entered_at` moves only when the TARGET changed. Touching it on every tick
    /// would turn "degraded for forty minutes" into "degraded for one minute" forever.
    pub async fn record(&self, e: &Evaluation, now: DateTime<Utc>) -> Result<(), LadderDbError> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        sqlx::query(
            "INSERT INTO mm_broadcast_demotion
                 (broadcast_id, target_step, applied_step, entered_at, evaluated_at,
                  balance_minor, projected_cost_minor, milder_streak)
             VALUES ($1, $2, $3, $4, $4, $5, $6, $7)
             ON CONFLICT (broadcast_id) DO UPDATE SET
                 target_step          = EXCLUDED.target_step,
                 applied_step         = EXCLUDED.applied_step,
                 entered_at           = CASE WHEN $8 THEN EXCLUDED.entered_at
                                             ELSE mm_broadcast_demotion.entered_at END,
                 evaluated_at         = EXCLUDED.evaluated_at,
                 balance_minor        = EXCLUDED.balance_minor,
                 projected_cost_minor = EXCLUDED.projected_cost_minor,
                 milder_streak        = EXCLUDED.milder_streak",
        )
        .bind(&e.broadcast_id)
        .bind(&e.target_step)
        .bind(&e.applied_step)
        .bind(now)
        .bind(e.balance_minor)
        .bind(e.projected_cost_minor)
        .bind(e.milder_streak)
        .bind(e.target_changed)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        for ev in &e.events {
            sqlx::query(
                "INSERT INTO mm_demotion_events
                     (broadcast_id, user_id, from_step, to_step, balance_minor,
                      projected_cost_minor, statement, kind, occurred_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(&e.broadcast_id)
            .bind(&e.user_id)
            .bind(&ev.from_step)
            .bind(&ev.to_step)
            .bind(e.balance_minor)
            .bind(e.projected_cost_minor)
            .bind(&ev.statement)
            .bind(ev.kind.as_str())
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }

        tx.commit().await.map_err(db)?;
        Ok(())
    }

    /// **Live** broadcasts by rung, for the gauge, as `(which, step, count)` where
    /// `which` is `target` or `applied`.
    ///
    /// Joined to `mm_streams` on purpose. Every evaluated broadcast gets a row and
    /// rows are never deleted, so counting the table counts every broadcast that was
    /// ever live — the first version's gauge grew forever and reported ended streams
    /// as healthy live ones.
    ///
    /// Both rungs, because in observe mode they are the difference between the
    /// forecast and what was done, and that difference is the whole reason to run in
    /// observe mode.
    pub async fn live_step_counts(&self) -> Result<Vec<(String, String, i64)>, LadderDbError> {
        sqlx::query_as(
            "SELECT 'target', d.target_step, count(*)::BIGINT
               FROM mm_broadcast_demotion d
               JOIN mm_streams s ON s.id = d.broadcast_id AND s.status = 'active'
              GROUP BY d.target_step
             UNION ALL
             SELECT 'applied', d.applied_step, count(*)::BIGINT
               FROM mm_broadcast_demotion d
               JOIN mm_streams s ON s.id = d.broadcast_id AND s.status = 'active'
              GROUP BY d.applied_step",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }

    /// Statements of reasons nobody has delivered yet (CR-604).
    ///
    /// Only `applied` rows: a decision is internal and is never sent. The delivery
    /// channel is not built; this exists so that gap is a **number** rather than an
    /// assumption.
    pub async fn undelivered_statements(&self) -> Result<i64, LadderDbError> {
        sqlx::query_scalar(
            "SELECT count(*) FROM mm_demotion_events
              WHERE delivered_at IS NULL AND kind = 'applied'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)
    }
}
