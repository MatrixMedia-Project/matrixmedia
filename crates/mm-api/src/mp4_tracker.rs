//! Tracks mm-switch MP4 transcodes and writes mm_recordings.mp4_status.
//!
//! mm-switch transcodes asynchronously after finalise (transcode.go);
//! it has no DB access, so mm-core polls GET /api/recordings/{id}/mp4
//! and owns the column. Spawned from end_stream per recording, plus a
//! one-shot startup sweep for rows orphaned by an mm-core restart.

use std::sync::Arc;
use std::time::Duration;

use mm_core::switch_client::SwitchClient;
use sqlx::PgPool;

/// veryfast x264 runs ~2-4x realtime; allow a long wall-clock window
/// (4 polls/min * 240 = 60 min) before declaring the rendition failed.
const POLL_EVERY: Duration = Duration::from_secs(15);
const MAX_POLLS: u32 = 240;

/// Poll mm-switch until the recording's MP4 rendition resolves, then
/// persist the outcome. The WebM (`storage_key` / `status`) is never
/// touched — a failed rendition leaves playback exactly as pre-feature.
pub async fn track_mp4_transcode(pool: PgPool, switch: Arc<SwitchClient>, recording_id: String) {
    for _ in 0..MAX_POLLS {
        tokio::time::sleep(POLL_EVERY).await;
        match switch.record_mp4_status(&recording_id).await {
            Ok(s) if s == "ready" => {
                let _ = sqlx::query(
                    "UPDATE mm_recordings \
                     SET mp4_status = 'ready', \
                         mp4_key = regexp_replace(storage_key, '\\.webm$', '.mp4') \
                     WHERE id = $1",
                )
                .bind(&recording_id)
                .execute(&pool)
                .await;
                tracing::info!(recording_id = %recording_id, "mp4 rendition ready");
                return;
            }
            Ok(s) if s == "failed" => {
                mark_failed(&pool, &recording_id).await;
                tracing::warn!(recording_id = %recording_id, "mp4 transcode failed");
                return;
            }
            // pending / unknown / transient transport error → keep polling
            _ => {}
        }
    }
    // Window exhausted. Guard on 'pending' so a racing success isn't
    // clobbered; the backfill script repairs files that land later.
    mark_failed(&pool, &recording_id).await;
    tracing::warn!(recording_id = %recording_id, "mp4 transcode poll window exhausted");
}

async fn mark_failed(pool: &PgPool, recording_id: &str) {
    let _ = sqlx::query(
        "UPDATE mm_recordings SET mp4_status = 'failed' \
         WHERE id = $1 AND mp4_status = 'pending'",
    )
    .bind(recording_id)
    .execute(pool)
    .await;
}

/// One-shot boot sweep: re-attach pollers to rows left 'pending' by a
/// restart. Bounded by the V030 partial index.
pub async fn resume_pending(pool: PgPool, switch: Arc<SwitchClient>) {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM mm_recordings WHERE mp4_status = 'pending'")
            .fetch_all(&pool)
            .await
            .unwrap_or_default();
    if !ids.is_empty() {
        tracing::info!(count = ids.len(), "resuming mp4 transcode pollers");
    }
    for id in ids {
        tokio::spawn(track_mp4_transcode(pool.clone(), switch.clone(), id));
    }
}
