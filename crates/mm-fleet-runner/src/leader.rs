//! One runner at a time. A session-level advisory lock on a standalone connection (the
//! same pattern mm-db uses for migrations): a pooled connection could be handed to another
//! task and the lock would silently travel with it.

use sqlx::postgres::PgConnection;
use sqlx::{ConnectOptions, Connection, PgPool};

pub const LEADER_LOCK_KEY: i64 = 0x6d6d_7275_6e6e_6572; // "mmrunner"

/// Holds the session on which the advisory lock was taken. Dropping it closes the socket,
/// and Postgres releases the lock with the session (a moment later, once it notices the
/// socket is gone); `release` lets go immediately.
pub struct LeaderLock {
    conn: PgConnection,
}

impl LeaderLock {
    /// Unlocks and closes now. Once this returns the lock is free for the next caller. The
    /// connection is standalone (never pooled), so closing it afterwards is what frees the
    /// session even if the unlock query failed.
    pub async fn release(mut self) {
        if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(LEADER_LOCK_KEY)
            .execute(&mut self.conn)
            .await
        {
            tracing::warn!(error = %e, "leader unlock failed; freed by closing the connection");
        }
        let _ = self.conn.close().await;
    }
}

pub async fn try_acquire(pool: &PgPool) -> Result<Option<LeaderLock>, sqlx::Error> {
    let mut conn = (*pool.connect_options()).clone().connect().await?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(LEADER_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    if !got {
        conn.close().await.ok();
        return Ok(None);
    }
    Ok(Some(LeaderLock { conn }))
}

/// Blocks until the lock is ours, logging every 15 s while another runner holds it.
pub async fn acquire(pool: &PgPool) -> Result<LeaderLock, sqlx::Error> {
    loop {
        if let Some(l) = try_acquire(pool).await? {
            return Ok(l);
        }
        tracing::warn!("another mm-fleet-runner holds the leader lock; waiting");
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
}
