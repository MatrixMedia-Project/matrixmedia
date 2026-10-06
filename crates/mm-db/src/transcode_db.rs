//! The transcode opt-in, as stored (V040; FR-314a/c).
//!
//! One module owns this SQL so the Creator Studio API and the fleet runner read
//! the same facts the same way. Precedence (override over default, release as a
//! veto) is NOT decided here — it lives in [`TranscodeOptIn`], once.

use sqlx::{PgPool, Row};

use mm_core::fleet::transcode::{TranscodeOptIn, TranscodeOverride};

/// One broadcast's transcode facts, with what a caller needs to authorise a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastTranscode {
    pub host_user_id: String,
    /// `mm_streams.status`; only an `active` broadcast's override can change.
    pub active: bool,
    pub opt_in: TranscodeOptIn,
}

/// Why a per-broadcast override was not written. Each variant is a different
/// answer to the caller (404 / 403 / 410), which is the reason to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideRefused {
    NotFound,
    NotHost,
    Ended,
}

/// The broadcaster's default; `false` when they have never saved defaults.
pub async fn broadcaster_default(pool: &PgPool, user_id: &str) -> Result<bool, sqlx::Error> {
    let on: Option<bool> = sqlx::query_scalar(
        "SELECT transcode_opt_in_default FROM mm_creator_defaults WHERE creator_user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(on.unwrap_or(false))
}

/// Set the broadcaster's default. Touches no other default: a broadcaster with no
/// defaults row gets one with the column defaults for everything else.
///
/// Does not clear any broadcast's `transcode_released` — changing the default is
/// not a decision about a broadcast an operator released (FR-314c, D13).
pub async fn set_broadcaster_default(
    pool: &PgPool,
    user_id: &str,
    on: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO mm_creator_defaults (creator_user_id, transcode_opt_in_default)
         VALUES ($1, $2)
         ON CONFLICT (creator_user_id) DO UPDATE
            SET transcode_opt_in_default = EXCLUDED.transcode_opt_in_default,
                updated_at = now()",
    )
    .bind(user_id)
    .bind(on)
    .execute(pool)
    .await?;
    Ok(())
}

/// Everything stored about one broadcast's transcode choice, or `None` when the
/// broadcast does not exist.
pub async fn for_broadcast(
    pool: &PgPool,
    stream_id: &str,
) -> Result<Option<BroadcastTranscode>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT s.host_user_id,
                s.status,
                s.transcode_opt_in,
                s.transcode_released,
                COALESCE(d.transcode_opt_in_default, false) AS broadcaster_default
           FROM mm_streams s
           LEFT JOIN mm_creator_defaults d ON d.creator_user_id = s.host_user_id
          WHERE s.id = $1",
    )
    .bind(stream_id)
    .fetch_optional(pool)
    .await?;

    row.map(|r| {
        Ok(BroadcastTranscode {
            host_user_id: r.try_get("host_user_id")?,
            active: r.try_get::<String, _>("status")? == "active",
            opt_in: TranscodeOptIn {
                broadcaster_default: r.try_get("broadcaster_default")?,
                broadcast_override: parse_override(r.try_get("transcode_opt_in")?)?,
                released: r.try_get("transcode_released")?,
            },
        })
    })
    .transpose()
}

/// Set one broadcast's override, as its host. Returns the resulting choice.
///
/// An explicit `On` is the broadcaster opting this broadcast in again, so it is the
/// one write that clears an operator release (FR-314c). `Inherit` does not, even
/// when the default is on: deferring to a default is not a decision about a
/// broadcast an operator stopped.
///
/// The host and `active` conditions are in the UPDATE itself, so a broadcast that
/// ends between the caller's read and this write is refused rather than changed.
pub async fn set_broadcast_override(
    pool: &PgPool,
    stream_id: &str,
    host_user_id: &str,
    choice: TranscodeOverride,
) -> Result<Result<TranscodeOptIn, OverrideRefused>, sqlx::Error> {
    let row = sqlx::query(
        "UPDATE mm_streams s
            SET transcode_opt_in = $3,
                transcode_released = CASE WHEN $3 = 'on' THEN false
                                          ELSE s.transcode_released END
          WHERE s.id = $1 AND s.host_user_id = $2 AND s.status = 'active'
      RETURNING s.transcode_opt_in,
                s.transcode_released,
                COALESCE((SELECT d.transcode_opt_in_default
                            FROM mm_creator_defaults d
                           WHERE d.creator_user_id = s.host_user_id), false)
                    AS broadcaster_default",
    )
    .bind(stream_id)
    .bind(host_user_id)
    .bind(choice.as_str())
    .fetch_optional(pool)
    .await?;

    if let Some(r) = row {
        return Ok(Ok(TranscodeOptIn {
            broadcaster_default: r.try_get("broadcaster_default")?,
            broadcast_override: parse_override(r.try_get("transcode_opt_in")?)?,
            released: r.try_get("transcode_released")?,
        }));
    }

    // Nothing written: say why.
    Ok(Err(match for_broadcast(pool, stream_id).await? {
        None => OverrideRefused::NotFound,
        Some(b) if b.host_user_id != host_user_id => OverrideRefused::NotHost,
        Some(_) => OverrideRefused::Ended,
    }))
}

/// The CHECK makes an unknown value unreachable; reaching it anyway is a schema
/// drift, reported as a decode error rather than read as "not opted in".
fn parse_override(s: String) -> Result<TranscodeOverride, sqlx::Error> {
    s.parse().map_err(|e: String| sqlx::Error::Decode(e.into()))
}
