//! Caption-job enqueue + serving helpers (video-to-text E1).
//!
//! On end/finalise the end path ([`crate::stream_lifecycle`]) enqueues one
//! caption job per just-finalised local recording via [`enqueue_caption_jobs`],
//! next to `mp4_tracker::track_mp4_transcode`. Each job owns a
//! `mm_recording_captions` row (V057) for the source language. The row is
//! the whole handoff: mm-caption-worker polls `pending` rows, transcribes
//! with whisper.cpp, publishes the `.vtt` sidecar, and marks rows `ready`
//! (or `failed` + error). Serving (`caption_urls`, nginx `text/vtt`) and the
//! feed announcement read only `ready` rows, so they need no changes as
//! engines land.

use std::collections::BTreeMap;

use mm_db::models::CaptionTrack;
use sqlx::PgPool;

/// Source language transcribed first. Translations (E2) add rows per target
/// language; serving and the feed prefer this one.
pub const SOURCE_LANG: &str = "en";

/// Storage key of the `.vtt` sibling for a recording file + language:
/// `/data/recordings/{id}.webm` → `/data/recordings/{id}.en.vtt`
/// (same stem-swap trick as the MP4 rendition key in `mp4_tracker`).
pub fn vtt_storage_key(recording_storage_key: &str, lang: &str) -> String {
    let stem = recording_storage_key
        .strip_suffix(".webm")
        .or_else(|| recording_storage_key.strip_suffix(".mp4"))
        .unwrap_or(recording_storage_key);
    format!("{stem}.{lang}.vtt")
}

/// Public URL for a caption track's storage key on the existing nginx
/// `/_mm/recordings/` static route (the file's basename — never the raw
/// storage path). `None` when the key has no basename, so callers can skip
/// the track instead of handing out a broken URL.
pub fn vtt_url(public_url: &str, storage_key: &str) -> Option<String> {
    let filename = storage_key.rsplit('/').next().filter(|s| !s.is_empty())?;
    Some(format!("{public_url}/_mm/recordings/{filename}"))
}

/// Build the `caption_urls` map (language tag → WebVTT URL) for one
/// recording from its ready tracks. Tracks without a storage key are
/// skipped: the map must never contain a URL that 404s.
pub fn caption_urls_for(
    public_url: &str,
    recording_id: &str,
    tracks: &[CaptionTrack],
) -> BTreeMap<String, String> {
    tracks
        .iter()
        .filter(|t| t.recording_id == recording_id)
        .filter_map(|t| {
            let key = t.storage_key.as_deref()?;
            Some((t.lang.clone(), vtt_url(public_url, key)?))
        })
        .collect()
}

/// Enqueue caption jobs for the stream's just-finalised local recordings.
///
/// Runs in the end path after `mark_recordings_ready`, next to MP4 tracking.
/// One idempotent `(recording, en, source)` row per recording; re-runs (a
/// retried end, a restarted server) keep the existing row and only re-drive
/// it while it is still `pending`.
pub async fn enqueue_caption_jobs(pool: &PgPool, stream_id: &str) {
    let rows: Vec<(String, String)> = match sqlx::query_as(
        "SELECT id, storage_key FROM mm_recordings \
         WHERE stream_id = $1 AND status = 'ready' AND storage_backend = 'local'",
    )
    .bind(stream_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(stream_id = %stream_id, error = %e,
                "end: failed to list recordings for caption enqueue");
            return;
        }
    };
    for (rec_id, storage_key) in rows {
        let key = vtt_storage_key(&storage_key, SOURCE_LANG);
        let inserted = sqlx::query(
            "INSERT INTO mm_recording_captions (recording_id, lang, kind, status, storage_key) \
             VALUES ($1, $2, 'source', 'pending', $3) \
             ON CONFLICT (recording_id, lang, kind) DO NOTHING",
        )
        .bind(&rec_id)
        .bind(SOURCE_LANG)
        .bind(&key)
        .execute(pool)
        .await;
        match inserted {
            Ok(r) => {
                // Fresh row, or a re-run over an existing one. The row is
                // the whole handoff: mm-caption-worker polls `pending` rows
                // and resolves them (ready + sidecar, or failed + error).
                // Nothing is spawned here — the worker owns the lifecycle,
                // so a duplicate resolve can never race it.
                if r.rows_affected() > 0 {
                    tracing::info!(recording_id = %rec_id, "caption job enqueued");
                }
            }
            Err(e) => {
                tracing::warn!(recording_id = %rec_id, error = %e,
                    "end: failed to enqueue caption job");
            }
        }
    }
}

/// Job resolution lives in mm-caption-worker now (E1): it polls `pending`
/// rows with `SELECT ... FOR UPDATE SKIP LOCKED`, transcribes, publishes
/// the sidecar, and marks rows `ready` (or `failed` with the error). Crash
/// recovery is inherent — a dead worker leaves rows `pending` for the next
/// poll — so there is no in-process resolver and no boot sweep here.

/// Outcome of [`rerun_source_captions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerunOutcome {
    /// No `mm_recordings` row with that id — the caller answers 404.
    NoRecording,
    /// The `(recording, en, source)` row is `pending` again (or was just
    /// inserted that way): mm-caption-worker picks it up on its next poll.
    ResetToPending,
}

/// Re-run the English source caption job for one recording.
///
/// Resets the `(recording, en, source)` row to `pending` with a clear error
/// so mm-caption-worker transcribes it again; a recording that never got a
/// row (it ended while captions were off) gets a fresh pending row with the
/// same precomputed storage key [`enqueue_caption_jobs`] would have written.
/// English source only — translations and summaries are a later engine step.
pub async fn rerun_source_captions(
    pool: &PgPool,
    recording_id: &str,
) -> Result<RerunOutcome, sqlx::Error> {
    let storage_key: Option<String> =
        sqlx::query_scalar("SELECT storage_key FROM mm_recordings WHERE id = $1")
            .bind(recording_id)
            .fetch_optional(pool)
            .await?;
    let Some(storage_key) = storage_key else {
        return Ok(RerunOutcome::NoRecording);
    };
    let key = vtt_storage_key(&storage_key, SOURCE_LANG);
    sqlx::query(
        "INSERT INTO mm_recording_captions (recording_id, lang, kind, status, storage_key, error) \
         VALUES ($1, $2, 'source', 'pending', $3, NULL) \
         ON CONFLICT (recording_id, lang, kind) DO UPDATE \
            SET status = 'pending', error = NULL, updated_at = now()",
    )
    .bind(recording_id)
    .bind(SOURCE_LANG)
    .bind(&key)
    .execute(pool)
    .await?;
    Ok(RerunOutcome::ResetToPending)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(recording_id: &str, lang: &str, storage_key: Option<&str>) -> CaptionTrack {
        CaptionTrack {
            recording_id: recording_id.to_string(),
            lang: lang.to_string(),
            kind: "source".to_string(),
            status: "ready".to_string(),
            storage_key: storage_key.map(|s| s.to_string()),
            summary_text: None,
        }
    }

    #[test]
    fn vtt_key_swaps_webm_and_mp4_stems() {
        assert_eq!(
            vtt_storage_key("/data/recordings/abc.webm", "en"),
            "/data/recordings/abc.en.vtt"
        );
        assert_eq!(
            vtt_storage_key("/data/recordings/abc.mp4", "de"),
            "/data/recordings/abc.de.vtt"
        );
    }

    #[test]
    fn vtt_key_keeps_bare_names_usable() {
        assert_eq!(vtt_storage_key("abc", "en"), "abc.en.vtt");
    }

    #[test]
    fn vtt_url_uses_basename_on_static_route() {
        assert_eq!(
            vtt_url("https://m.example.org", "/data/recordings/abc.en.vtt"),
            Some("https://m.example.org/_mm/recordings/abc.en.vtt".to_string())
        );
        assert_eq!(
            vtt_url("", "/data/recordings/abc.en.vtt"),
            Some("/_mm/recordings/abc.en.vtt".to_string())
        );
    }

    #[test]
    fn vtt_url_rejects_empty_basename() {
        assert_eq!(vtt_url("https://m.example.org", ""), None);
        assert_eq!(vtt_url("https://m.example.org", "/data/recordings/"), None);
    }

    #[test]
    fn caption_map_covers_ready_tracks_per_recording() {
        let tracks = vec![
            track("rec_a", "en", Some("/data/recordings/rec_a.en.vtt")),
            track("rec_a", "de", Some("/data/recordings/rec_a.de.vtt")),
            track("rec_b", "en", Some("/data/recordings/rec_b.en.vtt")),
        ];
        let urls = caption_urls_for("", "rec_a", &tracks);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls["en"], "/_mm/recordings/rec_a.en.vtt");
        assert_eq!(urls["de"], "/_mm/recordings/rec_a.de.vtt");
        assert!(caption_urls_for("", "rec_missing", &tracks).is_empty());
    }

    #[test]
    fn caption_map_skips_tracks_without_a_key() {
        let tracks = vec![track("rec_a", "en", None)];
        assert!(caption_urls_for("", "rec_a", &tracks).is_empty());
    }
}
