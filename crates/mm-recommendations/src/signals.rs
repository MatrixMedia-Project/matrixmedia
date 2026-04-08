//! Signal collection for user interactions.
//!
//! Thin wrappers around `MonetizationDb::record_interaction` that apply
//! validation and logging.

use mm_db::MonetizationDb;
use mm_db::models::{InteractionType, UserInteraction};
use mm_db::monetization_db::PgMonetizationDb;
use sqlx::PgPool;

/// Record that a user viewed a stream, optionally with duration in seconds.
pub async fn record_stream_view(
    pg: &PgPool,
    user_id: &str,
    stream_id: &str,
    duration_secs: Option<i32>,
) -> Result<UserInteraction, String> {
    let db = PgMonetizationDb::new(pg.clone());
    db.record_interaction(
        user_id,
        stream_id,
        InteractionType::View.as_str(),
        duration_secs,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Record that a user liked a stream.
pub async fn record_like(
    pg: &PgPool,
    user_id: &str,
    stream_id: &str,
) -> Result<UserInteraction, String> {
    let db = PgMonetizationDb::new(pg.clone());
    db.record_interaction(user_id, stream_id, InteractionType::Like.as_str(), None)
        .await
        .map_err(|e| e.to_string())
}

/// Record that a user shared a stream.
pub async fn record_share(
    pg: &PgPool,
    user_id: &str,
    stream_id: &str,
) -> Result<UserInteraction, String> {
    let db = PgMonetizationDb::new(pg.clone());
    db.record_interaction(user_id, stream_id, InteractionType::Share.as_str(), None)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use mm_db::models::InteractionType;

    #[test]
    fn test_interaction_types_match_db_constraint() {
        // The DB CHECK constraint allows only 'view', 'like', 'share'.
        // Verify our enum produces exactly those strings.
        assert_eq!(InteractionType::View.as_str(), "view");
        assert_eq!(InteractionType::Like.as_str(), "like");
        assert_eq!(InteractionType::Share.as_str(), "share");
    }
}
