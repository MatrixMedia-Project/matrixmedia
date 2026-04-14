use async_trait::async_trait;
use chrono::DateTime;
use std::time::Duration;

use livekit_api::access_token::{AccessToken, VideoGrants};
use livekit_api::services::egress::{
    EgressClient, EgressListFilter, EgressListOptions, EgressOutput, RoomCompositeOptions,
};
use livekit_api::services::room::{CreateRoomOptions, RoomClient};
use livekit_protocol::{
    EncodedFileOutput, S3Upload, SegmentedFileOutput, encoded_file_output, segmented_file_output,
};

use crate::{
    CreateRoomRequest, EgressInfo, EgressS3Config, EgressStatus, HlsEgressRequest,
    LocalRecordingRequest, ParticipantInfo, ParticipantPermissions, RecordingEgressRequest,
    RoomStats, SfuAdapter, SfuError, SfuRoom, SfuToken,
};

/// LiveKit SFU adapter.
///
/// Connects to a LiveKit server via its Twirp HTTP API to manage rooms,
/// participants, and token generation.
pub struct LiveKitAdapter {
    url: String,
    /// URL returned to clients (may differ from `url` when behind a reverse proxy).
    public_url: String,
    api_key: String,
    api_secret: String,
    room_client: RoomClient,
    egress_client: EgressClient,
}

impl LiveKitAdapter {
    /// Create a new LiveKit adapter.
    ///
    /// - `url`: LiveKit server URL for server-side API calls (e.g. `http://livekit:7880`)
    /// - `api_key`: LiveKit API key
    /// - `api_secret`: LiveKit API secret
    pub fn new(url: String, api_key: String, api_secret: String) -> Self {
        let public_url = std::env::var("MM_SFU_LIVEKIT_PUBLIC_URL")
            .unwrap_or_else(|_| url.clone());
        let room_client = RoomClient::with_api_key(&url, &api_key, &api_secret);
        let egress_client = EgressClient::with_api_key(&url, &api_key, &api_secret);
        Self {
            url,
            public_url,
            api_key,
            api_secret,
            room_client,
            egress_client,
        }
    }

    /// Map a LiveKit `ServiceError` to our `SfuError`.
    fn map_service_err(err: livekit_api::services::ServiceError) -> SfuError {
        let msg = err.to_string();
        if msg.contains("not found") || msg.contains("NotFound") {
            SfuError::RoomNotFound(msg)
        } else {
            SfuError::ConnectionFailed(msg)
        }
    }

    /// Convert our `EgressS3Config` to a LiveKit `S3Upload` protobuf message.
    fn to_lk_s3_upload(config: &EgressS3Config) -> S3Upload {
        S3Upload {
            access_key: config.access_key.clone(),
            secret: config.secret_key.clone(),
            region: config.region.clone(),
            endpoint: config.endpoint.clone(),
            bucket: config.bucket.clone(),
            force_path_style: config.force_path_style,
            ..Default::default()
        }
    }

    /// Convert a LiveKit `EgressInfo` protobuf to our domain `EgressInfo`.
    fn map_egress_info(lk: &livekit_protocol::EgressInfo) -> EgressInfo {
        let status = match lk.status {
            0 => EgressStatus::Starting,
            1 => EgressStatus::Active,
            2 => EgressStatus::Ending,
            3 => EgressStatus::Complete,
            4 => EgressStatus::Failed(lk.error.clone()),
            5 => EgressStatus::Failed(format!("aborted: {}", lk.error)),
            6 => EgressStatus::Failed("egress limit reached".to_string()),
            _ => EgressStatus::Failed(format!("unknown status: {}", lk.status)),
        };

        let started_at = if lk.started_at > 0 {
            DateTime::from_timestamp(lk.started_at, 0)
        } else {
            None
        };

        // Build output URL from segment results or file results.
        let output_url = lk
            .segment_results
            .first()
            .map(|s| s.playlist_name.clone())
            .or_else(|| lk.file_results.first().map(|f| f.filename.clone()));

        EgressInfo {
            egress_id: lk.egress_id.clone(),
            status,
            room_name: lk.room_name.clone(),
            started_at,
            output_url,
        }
    }
}

#[async_trait]
impl SfuAdapter for LiveKitAdapter {
    fn name(&self) -> &str {
        "livekit"
    }

    async fn health_check(&self) -> Result<(), SfuError> {
        // List rooms with an empty filter to verify connectivity.
        self.room_client
            .list_rooms(Vec::new())
            .await
            .map_err(Self::map_service_err)?;
        Ok(())
    }

    async fn create_room(&self, req: CreateRoomRequest) -> Result<SfuRoom, SfuError> {
        let options = CreateRoomOptions {
            max_participants: req.max_participants,
            empty_timeout: 300, // 5 minutes default
            ..Default::default()
        };

        let room = self
            .room_client
            .create_room(&req.name, options)
            .await
            .map_err(Self::map_service_err)?;

        Ok(SfuRoom {
            sfu_room_id: room.sid,
            name: room.name,
            num_participants: room.num_participants,
        })
    }

    async fn delete_room(&self, sfu_room_id: &str) -> Result<(), SfuError> {
        self.room_client
            .delete_room(sfu_room_id)
            .await
            .map_err(Self::map_service_err)
    }

    async fn generate_token(
        &self,
        room: &SfuRoom,
        participant: &ParticipantInfo,
        permissions: ParticipantPermissions,
    ) -> Result<SfuToken, SfuError> {
        // When source-level grants are specified, use can_publish_sources to
        // restrict which track types the participant may publish. This supersedes
        // the blanket can_publish flag in LiveKit.
        let can_publish_sources = if permissions.has_source_level_grants() {
            permissions.allowed_sources()
        } else {
            Vec::new()
        };

        let grants = VideoGrants {
            room_join: true,
            room: room.name.clone(),
            can_publish: permissions.can_publish,
            can_subscribe: permissions.can_subscribe,
            can_publish_data: permissions.can_publish_data,
            can_publish_sources,
            ..Default::default()
        };

        let token = AccessToken::with_api_key(&self.api_key, &self.api_secret)
            .with_identity(&participant.identity)
            .with_name(participant.name.as_deref().unwrap_or(&participant.identity))
            .with_ttl(Duration::from_secs(60))
            .with_grants(grants)
            .to_jwt()
            .map_err(|e| SfuError::Internal(format!("token generation failed: {e}")))?;

        Ok(SfuToken {
            token,
            url: self.public_url.clone(),
        })
    }

    async fn remove_participant(
        &self,
        sfu_room_id: &str,
        participant_id: &str,
    ) -> Result<(), SfuError> {
        self.room_client
            .remove_participant(sfu_room_id, participant_id)
            .await
            .map_err(Self::map_service_err)
    }

    async fn list_participants(&self, sfu_room_id: &str) -> Result<Vec<ParticipantInfo>, SfuError> {
        let participants = self
            .room_client
            .list_participants(sfu_room_id)
            .await
            .map_err(Self::map_service_err)?;

        Ok(participants
            .into_iter()
            .map(|p| ParticipantInfo {
                sfu_participant_id: p.sid,
                identity: p.identity,
                name: if p.name.is_empty() {
                    None
                } else {
                    Some(p.name)
                },
            })
            .collect())
    }

    async fn room_stats(&self, sfu_room_id: &str) -> Result<RoomStats, SfuError> {
        let participants = self
            .room_client
            .list_participants(sfu_room_id)
            .await
            .map_err(Self::map_service_err)?;

        let total = participants.len() as u32;
        let publishers = participants.iter().filter(|p| p.is_publisher).count() as u32;
        let subscribers = total.saturating_sub(publishers);

        Ok(RoomStats {
            sfu_room_id: sfu_room_id.to_string(),
            num_participants: total,
            num_publishers: publishers,
            num_subscribers: subscribers,
        })
    }

    // ----- Egress methods -----

    async fn start_hls_egress(&self, req: HlsEgressRequest) -> Result<EgressInfo, SfuError> {
        let s3_upload = Self::to_lk_s3_upload(&req.s3_config);

        let segments_output = SegmentedFileOutput {
            protocol: 0, // HLS
            filename_prefix: format!("{}segment", req.s3_config.path_prefix),
            playlist_name: format!("{}playlist.m3u8", req.s3_config.path_prefix),
            live_playlist_name: format!("{}live.m3u8", req.s3_config.path_prefix),
            segment_duration: req.segment_duration_secs,
            filename_suffix: 0, // Index
            disable_manifest: false,
            output: Some(segmented_file_output::Output::S3(s3_upload)),
        };

        let options = RoomCompositeOptions {
            audio_only: req.audio_only,
            ..Default::default()
        };

        let outputs = vec![EgressOutput::Segments(segments_output)];

        let lk_info = self
            .egress_client
            .start_room_composite_egress(&req.room_name, outputs, options)
            .await
            .map_err(Self::map_service_err)?;

        Ok(Self::map_egress_info(&lk_info))
    }

    async fn start_recording_egress(
        &self,
        req: RecordingEgressRequest,
    ) -> Result<EgressInfo, SfuError> {
        let s3_upload = Self::to_lk_s3_upload(&req.s3_config);

        let file_output = EncodedFileOutput {
            file_type: 0, // MP4 (default)
            filepath: format!("{}recording.mp4", req.s3_config.path_prefix),
            disable_manifest: false,
            output: Some(encoded_file_output::Output::S3(s3_upload)),
        };

        let options = RoomCompositeOptions {
            audio_only: req.audio_only,
            ..Default::default()
        };

        let outputs = vec![EgressOutput::File(file_output)];

        let lk_info = self
            .egress_client
            .start_room_composite_egress(&req.room_name, outputs, options)
            .await
            .map_err(Self::map_service_err)?;

        Ok(Self::map_egress_info(&lk_info))
    }

    async fn start_local_recording(
        &self,
        req: LocalRecordingRequest,
    ) -> Result<EgressInfo, SfuError> {
        let file_output = EncodedFileOutput {
            file_type: 0,
            filepath: req.output_path,
            disable_manifest: true,
            output: None,
        };

        let options = RoomCompositeOptions {
            audio_only: req.audio_only,
            ..Default::default()
        };

        let outputs = vec![EgressOutput::File(file_output)];

        let lk_info = self
            .egress_client
            .start_room_composite_egress(&req.room_name, outputs, options)
            .await
            .map_err(Self::map_service_err)?;

        Ok(Self::map_egress_info(&lk_info))
    }

    async fn stop_egress(&self, egress_id: &str) -> Result<(), SfuError> {
        self.egress_client
            .stop_egress(egress_id)
            .await
            .map_err(Self::map_service_err)?;
        Ok(())
    }

    async fn list_egresses(&self, room_name: &str) -> Result<Vec<EgressInfo>, SfuError> {
        let options = EgressListOptions {
            filter: EgressListFilter::Room(room_name.to_string()),
            active: false,
        };

        let lk_list = self
            .egress_client
            .list_egress(options)
            .await
            .map_err(Self::map_service_err)?;

        Ok(lk_list.iter().map(Self::map_egress_info).collect())
    }

    fn supports_egress(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adapter_creation() {
        let adapter = LiveKitAdapter::new(
            "http://localhost:7880".to_string(),
            "testkey".to_string(),
            "testsecret".to_string(),
        );
        assert_eq!(adapter.name(), "livekit");
        assert_eq!(adapter.url, "http://localhost:7880");
    }

    #[tokio::test]
    async fn test_generate_token_returns_jwt() {
        let adapter = LiveKitAdapter::new(
            "http://localhost:7880".to_string(),
            "testkey".to_string(),
            "testsecret_must_be_long_enough".to_string(),
        );

        let room = SfuRoom {
            sfu_room_id: "RM_abc123".to_string(),
            name: "test-room".to_string(),
            num_participants: 0,
        };

        let participant = ParticipantInfo {
            sfu_participant_id: "PA_xyz".to_string(),
            identity: "@alice:example.com".to_string(),
            name: Some("Alice".to_string()),
        };

        let permissions = ParticipantPermissions::audio_host();

        let token = adapter
            .generate_token(&room, &participant, permissions)
            .await
            .expect("token generation should succeed");

        // Should be a valid JWT (3 dot-separated parts)
        assert_eq!(token.token.split('.').count(), 3);
        assert_eq!(token.url, "http://localhost:7880");

        // Verify claims by decoding without verification
        let claims = livekit_api::access_token::Claims::from_unverified(&token.token)
            .expect("should parse claims");
        assert_eq!(claims.sub, "@alice:example.com");
        assert_eq!(claims.name, "Alice");
        assert_eq!(claims.iss, "testkey");
        assert!(claims.video.room_join);
        assert_eq!(claims.video.room, "test-room");
        assert!(claims.video.can_publish);
        assert!(claims.video.can_subscribe);
        // Audio host should have microphone in can_publish_sources
        assert!(
            claims
                .video
                .can_publish_sources
                .contains(&"microphone".to_string())
        );
        assert!(
            !claims
                .video
                .can_publish_sources
                .contains(&"camera".to_string())
        );
    }

    #[tokio::test]
    async fn test_generate_token_publish_disabled() {
        let adapter = LiveKitAdapter::new(
            "http://localhost:7880".to_string(),
            "testkey".to_string(),
            "testsecret_must_be_long_enough".to_string(),
        );

        let room = SfuRoom {
            sfu_room_id: "RM_abc123".to_string(),
            name: "listen-only-room".to_string(),
            num_participants: 5,
        };

        let participant = ParticipantInfo {
            sfu_participant_id: "PA_bob".to_string(),
            identity: "@bob:example.com".to_string(),
            name: None,
        };

        let permissions = ParticipantPermissions::viewer();

        let token = adapter
            .generate_token(&room, &participant, permissions)
            .await
            .expect("token generation should succeed");

        let claims = livekit_api::access_token::Claims::from_unverified(&token.token)
            .expect("should parse claims");
        assert!(!claims.video.can_publish);
        assert!(claims.video.can_subscribe);
        assert!(!claims.video.can_publish_data);
        // Viewer should have no publish sources
        assert!(claims.video.can_publish_sources.is_empty());
        // When name is None, identity is used as the name
        assert_eq!(claims.name, "@bob:example.com");
    }

    #[tokio::test]
    async fn test_generate_token_video_host_permissions() {
        let adapter = LiveKitAdapter::new(
            "http://localhost:7880".to_string(),
            "testkey".to_string(),
            "testsecret_must_be_long_enough".to_string(),
        );

        let room = SfuRoom {
            sfu_room_id: "RM_vid".to_string(),
            name: "video-room".to_string(),
            num_participants: 0,
        };

        let participant = ParticipantInfo {
            sfu_participant_id: "PA_host".to_string(),
            identity: "@host:example.com".to_string(),
            name: Some("Host".to_string()),
        };

        let permissions = ParticipantPermissions::video_host();

        let token = adapter
            .generate_token(&room, &participant, permissions)
            .await
            .expect("token generation should succeed");

        let claims = livekit_api::access_token::Claims::from_unverified(&token.token)
            .expect("should parse claims");
        assert!(claims.video.can_publish);
        assert!(claims.video.can_subscribe);
        assert!(claims.video.can_publish_data);
        // Video host should have microphone + camera
        assert!(
            claims
                .video
                .can_publish_sources
                .contains(&"microphone".to_string())
        );
        assert!(
            claims
                .video
                .can_publish_sources
                .contains(&"camera".to_string())
        );
        assert!(
            !claims
                .video
                .can_publish_sources
                .contains(&"screen_share".to_string())
        );
    }

    #[tokio::test]
    async fn test_generate_token_screen_share_permissions() {
        let adapter = LiveKitAdapter::new(
            "http://localhost:7880".to_string(),
            "testkey".to_string(),
            "testsecret_must_be_long_enough".to_string(),
        );

        let room = SfuRoom {
            sfu_room_id: "RM_scr".to_string(),
            name: "screen-room".to_string(),
            num_participants: 0,
        };

        let participant = ParticipantInfo {
            sfu_participant_id: "PA_presenter".to_string(),
            identity: "@presenter:example.com".to_string(),
            name: None,
        };

        let permissions = ParticipantPermissions::screen_share_host();

        let token = adapter
            .generate_token(&room, &participant, permissions)
            .await
            .expect("token generation should succeed");

        let claims = livekit_api::access_token::Claims::from_unverified(&token.token)
            .expect("should parse claims");
        assert!(claims.video.can_publish);
        assert!(claims.video.can_subscribe);
        // Screen share host should have microphone + screen_share + screen_share_audio
        assert!(
            claims
                .video
                .can_publish_sources
                .contains(&"microphone".to_string())
        );
        assert!(
            claims
                .video
                .can_publish_sources
                .contains(&"screen_share".to_string())
        );
        assert!(
            claims
                .video
                .can_publish_sources
                .contains(&"screen_share_audio".to_string())
        );
        assert!(
            !claims
                .video
                .can_publish_sources
                .contains(&"camera".to_string())
        );
    }
}
