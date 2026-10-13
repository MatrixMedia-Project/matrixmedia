//! Creatives uploaded before conversion existed (spec §6.2): their `{id}.webm` in the public ads
//! dir becomes the retained original and is re-encoded under profile v1. They keep serving the
//! old file until the new rendition is live (D7); the sweep retires the old file a day later.

use mm_ads::conversion::{Limits, SourceKind};
use mm_ads::conversion_db::{self as q, Enqueued, NewJob};

use crate::job::Ctx;
use crate::store::{self, CreativeId};

/// Queues a conversion for every legacy creative that has none. One creative that cannot be
/// imported (a missing or unreadable file) is logged and skipped, so it never holds up the others.
pub async fn import(ctx: &Ctx) -> anyhow::Result<u32> {
    let mut queued = 0;
    for (id, placement) in q::legacy_candidates(&ctx.pool).await? {
        match import_one(ctx, &id, placement).await {
            Ok(true) => queued += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(creative = %id, error = %e, "legacy import failed for this creative; the others go on"),
        }
    }
    Ok(queued)
}

async fn import_one(ctx: &Ctx, id: &str, placement: String) -> anyhow::Result<bool> {
    let Some(c) = CreativeId::parse(id) else {
        tracing::warn!(creative = %id, "legacy creative id is not a UUID; skipped");
        return Ok(false);
    };
    let legacy = ctx.layout.legacy(&c);
    if !tokio::fs::try_exists(&legacy).await.unwrap_or(false) {
        tracing::warn!(creative = %id, path = %legacy.display(), "legacy file missing; left as it is");
        return Ok(false);
    }
    // The retained original is copied under a temporary name, hashed, and only then renamed to its
    // content-addressed name: a crash never leaves a truncated `{sha}.src` that the job would
    // trust by name, and the checksum describes exactly the bytes that were kept.
    let dir = ctx.layout.sources_dir(&c);
    store::create_dir(&dir, 0o750).await?;
    let tmp = dir.join(format!(".legacy.{}.part", store::nonce()));
    let copied = async {
        tokio::fs::copy(&legacy, &tmp).await?;
        tokio::fs::File::open(&tmp).await?.sync_all().await?;
        let (sha, bytes) = store::sha256_file(&tmp).await?;
        if bytes == 0 {
            return Ok(None);
        }
        tokio::fs::rename(&tmp, ctx.layout.source(&c, &sha)).await?;
        std::io::Result::Ok(Some((sha, bytes)))
    }
    .await;
    let (sha, bytes) = match copied {
        Ok(Some(kept)) => kept,
        Ok(None) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            tracing::warn!(creative = %id, path = %legacy.display(), "legacy file is empty; left as it is");
            return Ok(false);
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
    };
    // It was accepted once: generous limits, so today's settings never reject it.
    let limits = Limits { placement, min_duration_secs: 1, max_duration_secs: 120, pre_roll_max_secs: 120, mid_roll_max_secs: 120 };
    let (enqueued, _) = q::enqueue(&ctx.pool, &NewJob {
        creative_id: id,
        upload_id: None,
        source_sha256: &sha,
        source_bytes: bytes as i64,
        source_kind: SourceKind::Matroska,
        limits: &limits,
        created_by: "legacy-import",
    })
    .await?;
    if matches!(enqueued, Enqueued::New(_)) {
        tracing::info!(creative = %id, "legacy creative queued for conversion");
    }
    Ok(matches!(enqueued, Enqueued::New(_)))
}
