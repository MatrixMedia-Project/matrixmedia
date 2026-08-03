//! TDD coverage for the reversible recordings `hidden` moderation flag (E3).
//!
//! Runs against an in-memory SQLite backend (no external services), mirroring
//! the `test_db()` harness in `src/sqlite.rs`. Verifies that
//! `set_recording_hidden` withholds a recording from the viewer-facing
//! `list_room_recordings` listing and that the operation is reversible.

use chrono::Utc;
use mm_core::types::{RoomId, UserId};
use mm_db::models::Recording;
use mm_db::sqlite::SqliteDatabase;
use mm_db::Database;
use uuid::Uuid;

/// Fresh in-memory SQLite database with all migrations applied.
async fn test_db() -> SqliteDatabase {
    let url = format!(
        "sqlite:file:moddb_{}?mode=memory&cache=shared",
        Uuid::new_v4()
    );
    let db = SqliteDatabase::new(&url).await.unwrap();
    db.migrate().await.unwrap();
    db
}

/// Seed one `ready` recording in a room so it appears in `list_room_recordings`.
async fn seed_ready_recording(db: &SqliteDatabase) -> (i64, String) {
    let room = db
        .get_or_create_room(&RoomId("!mod:example.com".to_string()))
        .await
        .unwrap();
    let host = UserId("@host:example.com".to_string());
    let stream = db
        .create_stream(room.id, &host, Some("Mod Stream"), "video", None, None)
        .await
        .unwrap();

    let rec_id = format!("rec_{}", Uuid::new_v4());
    let recording = Recording {
        id: rec_id.clone(),
        stream_id: stream.id.clone(),
        room_id: room.id,
        host_user_id: "@host:example.com".to_string(),
        status: "ready".to_string(),
        media_type: "video".to_string(),
        storage_key: format!("recordings/{}/recording.mp4", stream.id),
        storage_backend: "local".to_string(),
        mxc_url: None,
        cdn_url: None,
        duration_ms: Some(1000),
        size_bytes: Some(2048),
        mime_type: "video/mp4".to_string(),
        sha256: None,
        title: Some("Mod Recording".to_string()),
        egress_id: None,
        created_at: Utc::now(),
        completed_at: Some(Utc::now()),
        min_tier_level: None,
        mp4_status: "none".to_string(),
        mp4_key: None,
    };
    db.create_recording(&recording).await.unwrap();
    (room.id, rec_id)
}

#[tokio::test]
async fn test_set_recording_hidden_toggles_viewer_listing() {
    let db = test_db().await;
    let (room_id, rec_id) = seed_ready_recording(&db).await;

    // Initially visible in the viewer-facing listing.
    let listed = db.list_room_recordings(room_id, 50, None).await.unwrap();
    assert_eq!(listed.len(), 1, "ready recording should be listed initially");
    assert_eq!(listed[0].id, rec_id);

    // Hide it -> withheld from viewers.
    let updated = db.set_recording_hidden(&rec_id, true).await.unwrap();
    assert!(updated, "hiding an existing recording should update a row");
    let listed = db.list_room_recordings(room_id, 50, None).await.unwrap();
    assert_eq!(listed.len(), 0, "hidden recording must be withheld from viewers");

    // get_recording (viewer read) also withholds it.
    let got = db.get_recording(&rec_id).await.unwrap();
    assert!(got.is_none(), "hidden recording must be withheld from get_recording");

    // Un-hide it -> visible again (reversible).
    let updated = db.set_recording_hidden(&rec_id, false).await.unwrap();
    assert!(updated, "un-hiding an existing recording should update a row");
    let listed = db.list_room_recordings(room_id, 50, None).await.unwrap();
    assert_eq!(listed.len(), 1, "un-hidden recording should be listed again");
    assert_eq!(listed[0].id, rec_id);
}

#[tokio::test]
async fn test_set_recording_hidden_missing_row_returns_false() {
    let db = test_db().await;
    let updated = db
        .set_recording_hidden("rec_does_not_exist", true)
        .await
        .unwrap();
    assert!(!updated, "hiding a non-existent recording should affect no rows");
}
