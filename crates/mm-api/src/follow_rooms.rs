//! Go-live follow rooms (Twitch parity #3, V065) — the Matrix path.
//!
//! One hidden, read-only personal Matrix room per (follower, creator)
//! pair. The user never sees the room: follow = the bot invites them in,
//! unfollow = the bot kicks them (leave), and the streamer (via the bot)
//! can remove or block a follower. Only the bot posts, reusing the
//! existing `feed.*` timeline events, so the newsfeed indexer and
//! `GET /feed` pick follow-room updates up with no client changes: the
//! app merges the rooms into one Following feed.
//!
//! Tiered delivery is a write-time decision: an update whose audience is
//! Tier k is posted only into the rooms of followers whose active
//! subscription tier for that creator is >= k (higher tiers include
//! lower-tier updates). Non-qualifying followers never receive the event
//! (and therefore no push) at all.
//!
//! Every homeserver touch here is best-effort: a failed room create or
//! post is logged and never fails a follow, a stream start, or a
//! recording finalisation. Fan-out runs on spawned tasks.

use mm_core::error::{ErrorCode, MMError};
use mm_db::Database;
use mm_matrix::client::HomeserverClient;
use mm_matrix::events;

use crate::state::SharedState;
use crate::stream_lifecycle::MarkerContext;

/// Room type stamped into `creation_content` for follow rooms. MM
/// clients hide rooms of this type; clients that do not support the
/// feature never get invited (follow is an MM-app API action).
pub const FOLLOW_ROOM_TYPE: &str = "dev.matrixmedia.follow";

/// Whether a follower at `follower_tier` qualifies for an update whose
/// audience starts at `min_tier` (0 = everyone). Higher tiers include
/// lower-tier updates.
pub fn tier_allows(follower_tier: i32, min_tier: i32) -> bool {
    follower_tier >= min_tier
}

/// The follower's highest active subscription tier level for a creator
/// (0 = no active subscription). Creator-scoped tiers count, and so do
/// platform-default tiers (`creator_user_id = None` on the tier).
async fn follower_tier_level(
    db: &dyn Database,
    follower_user_id: &str,
    creator_user_id: &str,
) -> Result<i32, MMError> {
    let subs = db.get_user_subscriptions(follower_user_id).await?;
    let mut level = 0;
    for sub in subs {
        if sub.creator_user_id != creator_user_id || sub.status != "active" {
            continue;
        }
        if let Some(tier) = db.get_tier(sub.tier_id).await?
            && tier.is_active
            && tier
                .creator_user_id
                .as_deref()
                .is_none_or(|c| c == creator_user_id)
        {
            level = level.max(tier.tier_level);
        }
    }
    Ok(level)
}

/// The createRoom body for a follow room. Bot is PL 100; everyone else
/// needs PL 50 to send, invite, kick or ban — followers are read-only.
/// History visibility comes from the streamer's preference: `shared`
/// (new followers can read past notices, the default) or `joined`.
fn follow_room_body(
    bot_user_id: &str,
    follower_user_id: &str,
    history_visible: bool,
) -> serde_json::Value {
    serde_json::json!({
        "visibility": "private",
        "preset": "private_chat",
        "name": "MatrixMedia updates",
        "creation_content": { "type": FOLLOW_ROOM_TYPE },
        "invite": [follower_user_id],
        "initial_state": [
            {
                "type": "m.room.guest_access",
                "state_key": "",
                "content": { "guest_access": "forbidden" }
            },
            {
                "type": "m.room.history_visibility",
                "state_key": "",
                "content": {
                    "history_visibility": if history_visible { "shared" } else { "joined" }
                }
            }
        ],
        "power_level_content_override": {
            "ban": 50,
            "kick": 50,
            "invite": 50,
            "redact": 50,
            "events_default": 50,
            "state_default": 50,
            "users_default": 0,
            "users": { bot_user_id: 100 }
        }
    })
}

/// Ensure the (follower, creator) follow room exists and the follower is
/// invited into it. Returns the room id. Blocked followers are refused —
/// this is also the follow-time block enforcement point.
pub async fn ensure_follow_room(
    db: &dyn Database,
    hs: &HomeserverClient,
    follower_user_id: &str,
    creator_user_id: &str,
) -> Result<String, MMError> {
    if db.is_blocked_by(creator_user_id, follower_user_id).await? {
        return Err(MMError::api(ErrorCode::Forbidden, "blocked by creator"));
    }
    if let Some(room) = db
        .get_follow_room(follower_user_id, creator_user_id)
        .await?
    {
        // Re-invite (e.g. after a kick): an invite to an already-joined
        // user errors on the homeserver, which is fine to ignore here.
        let _ = hs.invite_user(&room.room_id, follower_user_id).await;
        return Ok(room.room_id);
    }
    let history_visible = db
        .get_notify_history_visible(creator_user_id)
        .await
        .unwrap_or(true);
    let room_id = hs
        .create_room(&follow_room_body(
            hs.bot_user_id(),
            follower_user_id,
            history_visible,
        ))
        .await?;
    db.upsert_follow_room(follower_user_id, creator_user_id, &room_id)
        .await?;
    Ok(room_id)
}

/// How a membership teardown should leave the follower in the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveMode {
    /// Follower left (unfollow) or the owner removed them: kick.
    Kick,
    /// Owner blocked them: ban (prevents rejoin until unbanned).
    Ban,
}

/// Tear down a follower's membership as the bot. The room and its
/// mapping are kept, so a later re-follow (or unblock + re-follow)
/// re-invites into the same room. Best-effort: membership may already
/// be gone (never invited, already left), which is not an error here.
pub async fn teardown_follow_membership(
    db: &dyn Database,
    hs: &HomeserverClient,
    follower_user_id: &str,
    creator_user_id: &str,
    mode: LeaveMode,
) {
    let room = match db.get_follow_room(follower_user_id, creator_user_id).await {
        Ok(Some(room)) => room,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                follower = %follower_user_id,
                creator = %creator_user_id,
                error = %e,
                "follow room lookup failed during membership teardown"
            );
            return;
        }
    };
    let result = match mode {
        LeaveMode::Kick => {
            hs.kick_user(&room.room_id, follower_user_id, Some("unfollowed"))
                .await
        }
        LeaveMode::Ban => {
            hs.ban_user(&room.room_id, follower_user_id, Some("blocked by creator"))
                .await
        }
    };
    if let Err(e) = result {
        tracing::warn!(
            follower = %follower_user_id,
            creator = %creator_user_id,
            room_id = %room.room_id,
            error = %e,
            "follow room membership teardown failed (continuing)"
        );
    }
}

/// One update to fan out. Borrows its content so callers keep owning the
/// payload they already built for the channel room.
enum FollowUpdate<'a> {
    GoLive(&'a events::FeedBroadcastStartedContent),
    NewVideo(&'a events::FeedRecordingAvailableContent),
}

/// Post one update into every qualifying follower's room. Delivery is
/// filtered per follower: blocked users are not followers (follow-time
/// enforcement), and the tier gate is `follower_tier >= min_tier`.
async fn fan_out(
    ctx: &MarkerContext<'_>,
    creator_user_id: &str,
    min_tier: i32,
    update: &FollowUpdate<'_>,
) {
    let followers = match ctx.db.get_creator_followers(creator_user_id).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(creator = %creator_user_id, error = %e, "follow fan-out: follower lookup failed");
            return;
        }
    };
    for follow in followers {
        let follower = follow.user_id.as_str();
        let tier = match follower_tier_level(ctx.db, follower, creator_user_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(follower = %follower, creator = %creator_user_id, error = %e, "follow fan-out: tier lookup failed, skipping");
                continue;
            }
        };
        if !tier_allows(tier, min_tier) {
            continue;
        }
        let room_id = match ensure_follow_room(ctx.db, ctx.hs_client, follower, creator_user_id)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(follower = %follower, creator = %creator_user_id, error = %e, "follow fan-out: room ensure failed, skipping");
                continue;
            }
        };
        let posted = match update {
            FollowUpdate::GoLive(content) => {
                crate::matrix_writes::emit_feed_broadcast_started(ctx, &room_id, content).await
            }
            FollowUpdate::NewVideo(content) => {
                crate::matrix_writes::emit_feed_recording_available(ctx, &room_id, content).await
            }
        };
        if let Err(e) = posted {
            tracing::warn!(follower = %follower, creator = %creator_user_id, room_id = %room_id, error = %e, "follow fan-out: post failed");
        }
    }
}

/// Fan a go-live update out to the creator's followers. Audience is the
/// stream's own gate (`min_tier_level`), defaulting to everyone.
pub async fn notify_go_live(
    ctx: &MarkerContext<'_>,
    stream: &mm_db::models::Stream,
    title: Option<&str>,
) {
    let content = events::build_feed_broadcast_started(
        &stream.id,
        &stream.host_user_id,
        title,
        stream.started_at.timestamp_millis(),
    );
    let min_tier = stream.min_tier_level.unwrap_or(0);
    fan_out(
        ctx,
        &stream.host_user_id,
        min_tier,
        &FollowUpdate::GoLive(&content),
    )
    .await;
}

/// Fan a new-video (recording available) update out to followers.
pub async fn notify_new_video(
    ctx: &MarkerContext<'_>,
    stream: &mm_db::models::Stream,
    content: &events::FeedRecordingAvailableContent,
) {
    let min_tier = stream.min_tier_level.unwrap_or(0);
    fan_out(
        ctx,
        &stream.host_user_id,
        min_tier,
        &FollowUpdate::NewVideo(content),
    )
    .await;
}

/// Spawn the go-live fan-out without blocking stream creation.
pub fn spawn_notify_go_live(
    state: &SharedState,
    stream: &mm_db::models::Stream,
    title: Option<String>,
) {
    let state = state.clone();
    let stream = stream.clone();
    tokio::spawn(async move {
        let cfg = state.config();
        let ctx = MarkerContext::from_state(&state, &cfg);
        notify_go_live(&ctx, &stream, title.as_deref()).await;
    });
}

#[cfg(test)]
mod tests {
    use super::tier_allows;

    #[test]
    fn everyone_reaches_everyone() {
        assert!(tier_allows(0, 0));
        assert!(tier_allows(3, 0));
    }

    #[test]
    fn higher_tiers_include_lower_tier_updates() {
        assert!(tier_allows(3, 1));
        assert!(tier_allows(2, 2));
        assert!(!tier_allows(1, 2));
        assert!(!tier_allows(0, 1));
    }
}
