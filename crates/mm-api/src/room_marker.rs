//! The MM-room marker, `dev.matrixmedia.room_config` (ADR-0011 section 3).
//!
//! A state event (key `""`, content version 2) that says "this is a
//! MatrixMedia room, run by this server", mirrors the owner's on/off switch,
//! and carries this server's `event_namespaces` block for clients of other
//! servers. mm-core writes it and never reads it back: the database stays the
//! authority and the marker grants nothing.
//!
//! Write points (all best-effort; none ever fails the request that triggered
//! it): the bot joining a room, `enable-mm`, the owner flipping the switch,
//! and stream start (which repairs any room the backfill missed). Each write
//! is skipped when [`content_hash`] matches what `mm_room_marker` recorded.
//!
//! When `matrix.pin_event_power_levels` is on, the first write in a room also
//! requires power level 100 for this server's current-name state types in the
//! room's `m.room.power_levels` (section 4, rule 8). Where the bot cannot do
//! that, the pinning is skipped with a log line and the marker is still
//! written.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use mm_core::config::Config;
use mm_core::event_namespace::EventSuffix;
use mm_matrix::events::{
    ROOM_CONFIG_EVENT_TYPE, ROOM_CONFIG_VERSION, RoomConfigContent, WriteNames,
};

use crate::error::ApiError;
use crate::event_namespaces::EventNamespaces;
use crate::middleware::AdminAuth;
use crate::state::SharedState;
use crate::stream_lifecycle::MarkerContext;

/// What one marker write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerOutcome {
    /// Written; the event id.
    Written(String),
    /// The recorded marker already has this content.
    Unchanged,
    /// `server.public_url` or `matrix.server_name` is unset: the marker
    /// cannot name the server, so it is not written.
    NotConfigured,
    /// The homeserver refused or failed the write.
    Failed(String),
}

/// What the power-level pinning did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinOutcome {
    /// `m.room.power_levels` was rewritten.
    Pinned,
    /// Every type already required 100 or more.
    AlreadyPinned,
    /// The bot's power is below 100 or below what changing the power levels
    /// needs. Nothing was written.
    NoPower,
    /// Off by setting.
    Disabled,
    /// Reading or writing the power levels failed.
    Failed(String),
}

static LOGGED_NOT_CONFIGURED: AtomicBool = AtomicBool::new(false);

/// The marker content for a room, or `None` when this server cannot name
/// itself (no `server.public_url` or no `matrix.server_name`).
pub fn marker_content(
    cfg: &Config,
    enabled: bool,
    namespaces: &EventNamespaces,
) -> Option<RoomConfigContent> {
    let url = cfg.server.public_url.clone().filter(|u| !u.is_empty())?;
    if cfg.matrix.server_name.is_empty() {
        return None;
    }
    Some(RoomConfigContent {
        version: ROOM_CONFIG_VERSION,
        enabled,
        mm_server_url: url,
        mm_matrix_server: cfg.matrix.server_name.clone(),
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
        event_namespaces: serde_json::to_value(namespaces).ok(),
    })
}

/// Hash of what a marker says: `version`, `enabled`, `mm_server_url`,
/// `mm_matrix_server` (ADR section 3) and the namespace block, which must be
/// rewritten when the namespace settings change. Not `updated_at_ms`.
pub fn content_hash(content: &RoomConfigContent) -> String {
    let canonical = json!([
        content.version,
        content.enabled,
        content.mm_server_url,
        content.mm_matrix_server,
        content.event_namespaces,
    ]);
    hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
}

/// The state types pinned at power level 100: this server's current-name
/// stream state events and the marker itself.
pub fn pinned_types(names: &WriteNames) -> Vec<String> {
    vec![
        names.current(EventSuffix::Stream),
        names.current(EventSuffix::StreamE2eeKey),
        ROOM_CONFIG_EVENT_TYPE.to_string(),
    ]
}

fn level(v: Option<&Value>, default: i64) -> i64 {
    v.and_then(Value::as_i64).unwrap_or(default)
}

/// The bot cannot make a power-levels change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LacksPower;

/// The new power-levels content with every type in `types` at 100 or above,
/// or `Ok(None)` when nothing needs to change. `Err(LacksPower)` when `bot` cannot
/// make the change (below 100, or below what editing the power levels needs).
pub fn pin_levels(
    power_levels: &Value,
    bot: &str,
    types: &[String],
) -> Result<Option<Value>, LacksPower> {
    let users_default = level(power_levels.get("users_default"), 0);
    let state_default = level(power_levels.get("state_default"), 50);
    let bot_level = level(
        power_levels.get("users").and_then(|u| u.get(bot)),
        users_default,
    );
    let events = power_levels
        .get("events")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let needed = level(events.get("m.room.power_levels"), state_default);
    let missing: Vec<&String> = types
        .iter()
        .filter(|t| level(events.get(t.as_str()), -1) < 100)
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }
    if bot_level < 100 || bot_level < needed {
        return Err(LacksPower);
    }
    let mut events = events;
    for t in missing {
        events.insert(t.clone(), json!(100));
    }
    let mut out = power_levels.clone();
    out["events"] = Value::Object(events);
    Ok(Some(out))
}

/// Pin the current-name state types at power level 100 in `room_id`.
pub async fn pin_power_levels(ctx: &MarkerContext<'_>, room_id: &str) -> PinOutcome {
    if !ctx.matrix.pin_event_power_levels {
        return PinOutcome::Disabled;
    }
    let current = match ctx
        .hs_client
        .get_state_raw(room_id, "m.room.power_levels", "")
        .await
    {
        Ok(Some(pl)) => pl,
        Ok(None) => json!({}),
        Err(e) => return PinOutcome::Failed(e.to_string()),
    };
    let types = pinned_types(&WriteNames::from_config(ctx.matrix));
    match pin_levels(&current, ctx.hs_client.bot_user_id(), &types) {
        Ok(None) => PinOutcome::AlreadyPinned,
        Ok(Some(new)) => match ctx
            .hs_client
            .send_state_event(room_id, "m.room.power_levels", "", &new)
            .await
        {
            Ok(_) => PinOutcome::Pinned,
            Err(e) => PinOutcome::Failed(e.to_string()),
        },
        Err(LacksPower) => {
            tracing::info!(
                room_id,
                "room marker: the bot lacks the power to pin the MatrixMedia \
                state types at 100; pinning skipped"
            );
            PinOutcome::NoPower
        }
    }
}

/// Write the marker in `room_id` unless the recorded one already says the same.
pub async fn ensure_room_marker(
    ctx: &MarkerContext<'_>,
    cfg: &Config,
    namespaces: &EventNamespaces,
    room_id: &str,
    enabled: bool,
) -> MarkerOutcome {
    let Some(content) = marker_content(cfg, enabled, namespaces) else {
        if !LOGGED_NOT_CONFIGURED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "room marker not written: server.public_url or matrix.server_name is unset"
            );
        }
        return MarkerOutcome::NotConfigured;
    };
    let hash = content_hash(&content);
    if let Ok(Some((_, recorded))) = ctx.db.get_room_marker(room_id).await
        && recorded == hash
    {
        return MarkerOutcome::Unchanged;
    }
    let _ = pin_power_levels(ctx, room_id).await;
    let json = match serde_json::to_value(&content) {
        Ok(v) => v,
        Err(e) => return MarkerOutcome::Failed(e.to_string()),
    };
    match crate::matrix_writes::send_state(
        ctx,
        room_id,
        ROOM_CONFIG_EVENT_TYPE,
        crate::matrix_writes::StatePayload::Json(&json),
    )
    .await
    {
        Ok(event_id) => {
            if let Err(e) = ctx.db.set_room_marker(room_id, &event_id, &hash).await {
                tracing::warn!(room_id, error = %e, "room marker written but not recorded");
            }
            MarkerOutcome::Written(event_id)
        }
        Err(e) => {
            tracing::warn!(room_id, error = %e, "room marker write failed");
            MarkerOutcome::Failed(e.to_string())
        }
    }
}

/// [`ensure_room_marker`] from handler state: reads the room's switch and the
/// announcement, and never fails.
pub async fn ensure_room_marker_for(state: &SharedState, room_id: &str) -> MarkerOutcome {
    let cfg = state.config();
    let enabled = match crate::rooms::load_mm_config(state, room_id).await {
        Ok(c) => c.mm_enabled,
        Err(e) => {
            tracing::warn!(room_id, error = %e.0, "room marker: mm-config unreadable; not written");
            return MarkerOutcome::Failed("mm-config unreadable".into());
        }
    };
    let namespaces = crate::event_namespaces::load(&cfg.matrix, state.db.as_ref()).await;
    let ctx = MarkerContext::from_state(state, &cfg);
    ensure_room_marker(&ctx, &cfg, &namespaces, room_id, enabled).await
}

// ---------------------------------------------------------------------------
// Backfill (admin route)
// ---------------------------------------------------------------------------

/// Body of `POST /_mm/admin/v1/room-markers/backfill`.
#[derive(Debug, Default, Deserialize)]
pub struct BackfillRequest {
    /// Count only; write nothing. Defaults to `true`: a real run is asked for
    /// explicitly.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Pause between writes, milliseconds (default 100).
    #[serde(default)]
    pub throttle_ms: Option<u64>,
}

fn default_true() -> bool {
    true
}

/// What a backfill found (and, on a real run, did).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct BackfillReport {
    pub dry_run: bool,
    /// Rooms the bot is joined to.
    pub eligible: usize,
    /// Rooms whose recorded marker already says the same.
    pub already_marked: usize,
    /// Rooms that need a marker (written on a real run).
    pub to_write: usize,
    /// Rooms where the bot cannot send state events (listed in `no_power_rooms`).
    pub no_power: usize,
    pub written: usize,
    pub failed: usize,
    pub no_power_rooms: Vec<String>,
    /// Set when the server cannot write markers at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_configured: Option<String>,
}

/// The backfill over the bot's joined rooms. `enabled_of` answers the
/// owner's switch for a room. Running it twice changes nothing; it never
/// promotes the bot.
pub async fn backfill<F, Fut>(
    ctx: &MarkerContext<'_>,
    cfg: &Config,
    namespaces: &EventNamespaces,
    dry_run: bool,
    throttle: std::time::Duration,
    enabled_of: F,
) -> Result<BackfillReport, mm_core::error::MMError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut report = BackfillReport {
        dry_run,
        ..Default::default()
    };
    let rooms = ctx.hs_client.joined_rooms().await?;
    report.eligible = rooms.len();
    for room in rooms {
        let enabled = enabled_of(room.clone()).await;
        let Some(content) = marker_content(cfg, enabled, namespaces) else {
            report.not_configured =
                Some("server.public_url or matrix.server_name is unset".to_string());
            return Ok(report);
        };
        if let Ok(Some((_, recorded))) = ctx.db.get_room_marker(&room).await
            && recorded == content_hash(&content)
        {
            report.already_marked += 1;
            continue;
        }
        if !ctx
            .hs_client
            .bot_has_state_power(&room)
            .await
            .unwrap_or(false)
        {
            report.no_power += 1;
            report.no_power_rooms.push(room);
            continue;
        }
        report.to_write += 1;
        if dry_run {
            continue;
        }
        match ensure_room_marker(ctx, cfg, namespaces, &room, enabled).await {
            MarkerOutcome::Written(_) | MarkerOutcome::Unchanged => report.written += 1,
            _ => report.failed += 1,
        }
        tokio::time::sleep(throttle).await;
    }
    Ok(report)
}

/// `POST /room-markers/backfill` (admin): write the marker into every room
/// the bot is in. `{"dry_run": true}` (the default) returns the counts only.
pub async fn backfill_handler(
    admin: AdminAuth,
    State(state): State<SharedState>,
    body: Option<Json<BackfillRequest>>,
) -> Result<Json<BackfillReport>, ApiError> {
    admin.require_admin()?;
    let req = body.map(|b| b.0).unwrap_or(BackfillRequest {
        dry_run: true,
        throttle_ms: None,
    });
    let cfg = state.config();
    let namespaces = crate::event_namespaces::load(&cfg.matrix, state.db.as_ref()).await;
    let ctx = MarkerContext::from_state(&state, &cfg);
    let throttle = std::time::Duration::from_millis(req.throttle_ms.unwrap_or(100).min(10_000));
    let report = backfill(&ctx, &cfg, &namespaces, req.dry_run, throttle, |room| {
        let state = state.clone();
        async move {
            crate::rooms::load_mm_config(&state, &room)
                .await
                .map(|c| c.mm_enabled)
                .unwrap_or(true)
        }
    })
    .await?;
    tracing::info!(?report, "room marker backfill");
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> Vec<String> {
        pinned_types(&WriteNames::new("dev.matrixmedia", true))
    }

    #[test]
    fn pinning_adds_the_missing_types_at_100_and_keeps_everything_else() {
        let pl = json!({
            "users": { "@mmbot:x": 100, "@alice:x": 100 },
            "events": { "m.room.name": 50, "dev.matrixmedia.stream": 50 },
            "state_default": 50,
            "ban": 50
        });
        let new = pin_levels(&pl, "@mmbot:x", &types()).unwrap().unwrap();
        assert_eq!(new["events"]["dev.matrixmedia.stream"], 100);
        assert_eq!(new["events"]["dev.matrixmedia.stream.e2ee_key"], 100);
        assert_eq!(new["events"]["dev.matrixmedia.room_config"], 100);
        assert_eq!(new["events"]["m.room.name"], 50);
        assert_eq!(new["ban"], 50);
        assert_eq!(new["users"], pl["users"]);
        // Already pinned: nothing to write.
        assert_eq!(pin_levels(&new, "@mmbot:x", &types()), Ok(None));
    }

    #[test]
    fn pinning_is_refused_where_the_bot_lacks_the_power() {
        let pl = json!({ "users": { "@mmbot:x": 50 }, "state_default": 50 });
        assert_eq!(pin_levels(&pl, "@mmbot:x", &types()), Err(LacksPower));
        let pl = json!({ "users": { "@mmbot:x": 100 }, "events": { "m.room.power_levels": 150 } });
        assert_eq!(pin_levels(&pl, "@mmbot:x", &types()), Err(LacksPower));
        let pl = json!({ "users_default": 100 });
        assert!(pin_levels(&pl, "@mmbot:x", &types()).unwrap().is_some());
    }

    #[test]
    fn the_hash_ignores_the_timestamp_and_follows_the_rest() {
        let ns = crate::event_namespaces::build(&Default::default(), &[]);
        let mut cfg = Config::default();
        cfg.server.public_url = Some("https://mm.example.org".into());
        cfg.matrix.server_name = "example.org".into();
        let a = marker_content(&cfg, true, &ns).unwrap();
        let mut b = a.clone();
        b.updated_at_ms += 1000;
        assert_eq!(content_hash(&a), content_hash(&b));
        b.enabled = false;
        assert_ne!(content_hash(&a), content_hash(&b));
        cfg.server.public_url = None;
        assert!(marker_content(&cfg, true, &ns).is_none());
    }
}
