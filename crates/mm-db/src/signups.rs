//! Audit-trail writes for /mm/v1/register.

use sha2::{Digest, Sha256};
use sqlx::PgPool;

pub async fn record_signup(
    pool: &PgPool,
    username: &str,
    mxid: &str,
    tos_version: &str,
    client_ip: &str,
    ip_pepper: &str,
) -> sqlx::Result<()> {
    let mut h = Sha256::new();
    h.update(client_ip.as_bytes());
    h.update(ip_pepper.as_bytes());
    let ip_hash: [u8; 32] = h.finalize().into();

    sqlx::query(
        "INSERT INTO mm_signups (username, mxid, tos_version, ip_hash) VALUES ($1, $2, $3, $4)",
    )
    .bind(username)
    .bind(mxid)
    .bind(tos_version)
    .bind(&ip_hash[..])
    .execute(pool)
    .await?;

    Ok(())
}
