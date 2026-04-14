pub mod livekit;
pub mod token;
pub mod webhook;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Error type for SFU operations.
#[derive(Debug, thiserror::Error)]
pub enum SfuError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("room not found: {0}")]
    RoomNotFound(String),

    #[error("participant not found: {0}")]
    ParticipantNotFound(String),

    #[error("timeout after {0}s")]
    Timeout(u64),

    #[error("circuit breaker open: SFU unavailable, retry after {0}s")]
    CircuitOpen(u64),

    #[error("internal: {0}")]
    Internal(String),
}

/// Request to create a new SFU room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRoomRequest {
    /// Application-level room name (typically the stream ID).
    pub name: String,
    /// Maximum number of participants.
    pub max_participants: u32,
    /// Whether to enable recording (Phase 3+).
    pub enable_recording: bool,
    /// Media capabilities for this room.
    #[serde(default)]
    pub media_config: SfuMediaConfig,
}

/// Media capabilities for an SFU room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SfuMediaConfig {
    /// Whether audio publishing is enabled.
    pub audio_enabled: bool,
    /// Whether video (camera) publishing is enabled.
    pub video_enabled: bool,
    /// Whether screen sharing is enabled.
    pub screen_share_enabled: bool,
    /// Audio codec (default "opus").
    pub audio_codec: String,
    /// Maximum audio bitrate in bps (default 48000).
    pub max_audio_bitrate: u32,
    /// Maximum video bitrate in bps (e.g. 2,500,000 for 720p). `None` for audio-only.
    pub max_video_bitrate: Option<u32>,
    /// Maximum video resolution. `None` for audio-only.
    pub max_video_resolution: Option<VideoResolution>,
    /// Whether to enable simulcast for video tracks.
    pub simulcast_enabled: bool,
}

impl Default for SfuMediaConfig {
    fn default() -> Self {
        Self {
            audio_enabled: true,
            video_enabled: false,
            screen_share_enabled: false,
            audio_codec: "opus".to_string(),
            max_audio_bitrate: 48_000,
            max_video_bitrate: None,
            max_video_resolution: None,
            simulcast_enabled: false,
        }
    }
}

/// Video resolution descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoResolution {
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
}

/// A room on the SFU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SfuRoom {
    /// SFU-assigned room identifier.
    pub sfu_room_id: String,
    /// Application-level room name.
    pub name: String,
    /// Current participant count.
    pub num_participants: u32,
}

/// Information about a participant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParticipantInfo {
    /// SFU-assigned participant identifier.
    pub sfu_participant_id: String,
    /// Matrix user ID.
    pub identity: String,
    /// Display name, if known.
    pub name: Option<String>,
}

impl fmt::Display for ParticipantInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.identity, self.sfu_participant_id)
    }
}

/// Permissions granted to a participant's SFU token.
///
/// The `can_publish_audio`, `can_publish_video`, and `can_publish_screen` flags
/// control which track sources a participant may publish. When any of these is
/// `true`, the token is generated with `can_publish_sources` set to the
/// corresponding LiveKit track source names, which supersedes the blanket
/// `can_publish` flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParticipantPermissions {
    /// Blanket publish permission (used when source-level flags are all false,
    /// i.e. the legacy audio-only path).
    pub can_publish: bool,
    /// Whether the participant can publish audio (microphone) tracks.
    pub can_publish_audio: bool,
    /// Whether the participant can publish video (camera) tracks.
    pub can_publish_video: bool,
    /// Whether the participant can publish screen-share tracks.
    pub can_publish_screen: bool,
    /// Whether the participant can subscribe to other participants' tracks.
    pub can_subscribe: bool,
    /// Whether the participant can publish data messages.
    pub can_publish_data: bool,
}

impl ParticipantPermissions {
    /// Full-access host: can publish mic, camera, AND screen share simultaneously.
    pub fn full_host() -> Self {
        Self {
            can_publish: true,
            can_publish_audio: true,
            can_publish_video: true,
            can_publish_screen: true,
            can_subscribe: true,
            can_publish_data: true,
        }
    }

    /// Create permissions for an audio-only host (backward-compatible default).
    pub fn audio_host() -> Self {
        Self {
            can_publish: true,
            can_publish_audio: true,
            can_publish_video: false,
            can_publish_screen: false,
            can_subscribe: true,
            can_publish_data: true,
        }
    }

    /// Create permissions for a video host (audio + camera).
    pub fn video_host() -> Self {
        Self {
            can_publish: true,
            can_publish_audio: true,
            can_publish_video: true,
            can_publish_screen: false,
            can_subscribe: true,
            can_publish_data: true,
        }
    }

    /// Create permissions for a screen-share host (audio + screen share).
    pub fn screen_share_host() -> Self {
        Self {
            can_publish: true,
            can_publish_audio: true,
            can_publish_video: false,
            can_publish_screen: true,
            can_subscribe: true,
            can_publish_data: true,
        }
    }

    /// Create permissions for a viewer (subscribe-only).
    pub fn viewer() -> Self {
        Self {
            can_publish: false,
            can_publish_audio: false,
            can_publish_video: false,
            can_publish_screen: false,
            can_subscribe: true,
            can_publish_data: false,
        }
    }

    /// Build the list of LiveKit track source names this permission set allows.
    ///
    /// Returns an empty vec when no source-level flags are set (blanket
    /// `can_publish` mode).
    pub fn allowed_sources(&self) -> Vec<String> {
        let mut sources = Vec::new();
        if self.can_publish_audio {
            sources.push("microphone".to_string());
        }
        if self.can_publish_video {
            sources.push("camera".to_string());
        }
        if self.can_publish_screen {
            sources.push("screen_share".to_string());
            sources.push("screen_share_audio".to_string());
        }
        sources
    }

    /// Returns true if any source-level publish flag is set.
    pub fn has_source_level_grants(&self) -> bool {
        self.can_publish_audio || self.can_publish_video || self.can_publish_screen
    }
}

/// An SFU access token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SfuToken {
    pub token: String,
    pub url: String,
}

/// Statistics for an SFU room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomStats {
    pub sfu_room_id: String,
    pub num_participants: u32,
    pub num_publishers: u32,
    pub num_subscribers: u32,
}

// ---------------------------------------------------------------------------
// Egress types
// ---------------------------------------------------------------------------

/// Request to start HLS egress for a room.
///
/// HLS egress produces `.m3u8` playlists and `.ts` segment files, uploaded
/// to an S3-compatible bucket for CDN delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlsEgressRequest {
    /// SFU room name to record.
    pub room_name: String,
    /// S3 storage configuration for segment upload.
    pub s3_config: EgressS3Config,
    /// Duration of each HLS segment in seconds (default 4).
    pub segment_duration_secs: u32,
    /// Minimum number of segments to keep in the playlist (default 3).
    pub min_playlist_size: u32,
    /// If true, record audio only (no video composite).
    pub audio_only: bool,
}

impl HlsEgressRequest {
    /// Create a request with sensible defaults for segment duration and playlist size.
    pub fn new(room_name: String, s3_config: EgressS3Config) -> Self {
        Self {
            room_name,
            s3_config,
            segment_duration_secs: 4,
            min_playlist_size: 3,
            audio_only: false,
        }
    }
}

/// Request to start a recording egress for a room (MP4 output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingEgressRequest {
    /// SFU room name to record.
    pub room_name: String,
    /// S3 storage configuration for the recording file.
    pub s3_config: EgressS3Config,
    /// If true, record audio only (no video composite).
    pub audio_only: bool,
}

impl RecordingEgressRequest {
    /// Create a request with default settings (audio+video).
    pub fn new(room_name: String, s3_config: EgressS3Config) -> Self {
        Self {
            room_name,
            s3_config,
            audio_only: false,
        }
    }
}

/// Request to start a local file recording (no S3, saves to LiveKit container filesystem).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRecordingRequest {
    /// SFU room name to record.
    pub room_name: String,
    /// Output file path inside the LiveKit container (e.g. `/data/recordings/rec_123.mp4`).
    pub output_path: String,
    /// If true, record audio only.
    pub audio_only: bool,
}

/// S3-compatible storage configuration for egress output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressS3Config {
    /// S3 endpoint URL (e.g. `https://s3.amazonaws.com` or a MinIO URL).
    pub endpoint: String,
    /// S3 bucket name.
    pub bucket: String,
    /// AWS region (e.g. `us-east-1`).
    pub region: String,
    /// AWS access key ID.
    pub access_key: String,
    /// AWS secret access key.
    pub secret_key: String,
    /// Path prefix within the bucket (e.g. `streams/{stream_id}/`).
    pub path_prefix: String,
    /// Use path-style addressing (required for MinIO and some S3-compatible stores).
    pub force_path_style: bool,
}

/// Information about an egress session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressInfo {
    /// SFU-assigned egress identifier.
    pub egress_id: String,
    /// Current status of the egress.
    pub status: EgressStatus,
    /// The SFU room name being recorded/streamed.
    pub room_name: String,
    /// When the egress started (if known).
    pub started_at: Option<DateTime<Utc>>,
    /// Output URL (e.g. `s3://bucket/streams/123/playlist.m3u8`).
    pub output_url: Option<String>,
}

/// Status of an egress session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressStatus {
    /// Egress is initializing.
    Starting,
    /// Egress is actively recording/streaming.
    Active,
    /// Egress is winding down.
    Ending,
    /// Egress completed successfully.
    Complete,
    /// Egress failed with an error message.
    Failed(String),
}

impl fmt::Display for EgressStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Active => write!(f, "active"),
            Self::Ending => write!(f, "ending"),
            Self::Complete => write!(f, "complete"),
            Self::Failed(msg) => write!(f, "failed: {msg}"),
        }
    }
}

/// Abstraction over SFU providers (LiveKit, etc.).
///
/// Default implementation: LiveKit via `livekit-api` crate.
/// Designed to be swappable for testing or alternative SFU backends.
#[async_trait]
pub trait SfuAdapter: Send + Sync + 'static {
    fn name(&self) -> &str;

    async fn health_check(&self) -> Result<(), SfuError>;

    async fn create_room(&self, req: CreateRoomRequest) -> Result<SfuRoom, SfuError>;

    async fn delete_room(&self, sfu_room_id: &str) -> Result<(), SfuError>;

    async fn generate_token(
        &self,
        room: &SfuRoom,
        participant: &ParticipantInfo,
        permissions: ParticipantPermissions,
    ) -> Result<SfuToken, SfuError>;

    async fn remove_participant(
        &self,
        sfu_room_id: &str,
        participant_id: &str,
    ) -> Result<(), SfuError>;

    async fn list_participants(&self, sfu_room_id: &str) -> Result<Vec<ParticipantInfo>, SfuError>;

    async fn room_stats(&self, sfu_room_id: &str) -> Result<RoomStats, SfuError>;

    /// Phase 4 E2EE extension point (placeholder).
    async fn supports_e2ee(&self) -> bool {
        false
    }

    // ----- Egress methods (default: not supported) -----

    /// Start HLS egress for a room (outputs `.m3u8` + `.ts` segments to S3).
    ///
    /// The default implementation returns an error indicating that egress is
    /// not supported by this adapter.
    async fn start_hls_egress(&self, _req: HlsEgressRequest) -> Result<EgressInfo, SfuError> {
        Err(SfuError::Internal(
            "egress not supported by this adapter".into(),
        ))
    }

    /// Start recording egress for a room (outputs MP4 to S3).
    ///
    /// The default implementation returns an error indicating that egress is
    /// not supported by this adapter.
    async fn start_recording_egress(
        &self,
        _req: RecordingEgressRequest,
    ) -> Result<EgressInfo, SfuError> {
        Err(SfuError::Internal(
            "egress not supported by this adapter".into(),
        ))
    }

    /// Start local file recording (no S3 — saves to container filesystem).
    async fn start_local_recording(
        &self,
        _req: LocalRecordingRequest,
    ) -> Result<EgressInfo, SfuError> {
        Err(SfuError::Internal(
            "local recording not supported by this adapter".into(),
        ))
    }

    /// Stop an active egress.
    ///
    /// The default implementation returns an error indicating that egress is
    /// not supported by this adapter.
    async fn stop_egress(&self, _egress_id: &str) -> Result<(), SfuError> {
        Err(SfuError::Internal(
            "egress not supported by this adapter".into(),
        ))
    }

    /// List active egresses for a room.
    ///
    /// The default implementation returns an empty list.
    async fn list_egresses(&self, _room_name: &str) -> Result<Vec<EgressInfo>, SfuError> {
        Ok(vec![])
    }

    /// Check if this adapter supports egress operations.
    fn supports_egress(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Circuit Breaker
// ---------------------------------------------------------------------------

/// State machine for a circuit breaker.
#[derive(Debug)]
enum CircuitState {
    /// Normal operation; tracks timestamps of recent failures.
    Closed { failures: Vec<Instant> },
    /// Requests are rejected immediately until the recovery timeout elapses.
    Open { opened_at: Instant },
    /// A single probe request is allowed through to test recovery.
    HalfOpen,
}

/// A simple circuit breaker that opens after `failure_threshold` failures
/// within `failure_window`, stays open for `recovery_timeout`, then allows
/// a single half-open probe.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: Arc<Mutex<CircuitState>>,
    failure_threshold: u32,
    recovery_timeout: Duration,
    failure_window: Duration,
}

impl CircuitBreaker {
    /// Create a new circuit breaker.
    ///
    /// - `failure_threshold`: number of failures within the window before opening.
    /// - `recovery_timeout`: how long to stay open before allowing a half-open probe.
    pub fn new(failure_threshold: u32, recovery_timeout: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(CircuitState::Closed {
                failures: Vec::new(),
            })),
            failure_threshold,
            recovery_timeout,
            failure_window: Duration::from_secs(30),
        }
    }

    /// Create a circuit breaker with a custom failure window.
    pub fn with_failure_window(mut self, window: Duration) -> Self {
        self.failure_window = window;
        self
    }

    /// Execute `f` through the circuit breaker.
    ///
    /// - **Closed**: call proceeds; on failure, record it; if threshold reached, open.
    /// - **Open**: reject immediately with `CircuitOpen` until `recovery_timeout` elapses.
    /// - **HalfOpen**: allow one probe; on success, close; on failure, re-open.
    pub async fn call<F, Fut, T, E>(&self, f: F) -> Result<T, SfuError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Into<SfuError>,
    {
        // Check state before calling
        {
            let mut state = self.state.lock().await;
            match &*state {
                CircuitState::Open { opened_at } => {
                    if opened_at.elapsed() >= self.recovery_timeout {
                        tracing::info!("circuit breaker: transitioning open -> half-open");
                        *state = CircuitState::HalfOpen;
                        // Fall through to allow the probe
                    } else {
                        let remaining = (self.recovery_timeout - opened_at.elapsed()).as_secs();
                        return Err(SfuError::CircuitOpen(remaining));
                    }
                }
                CircuitState::Closed { .. } | CircuitState::HalfOpen => {
                    // Proceed
                }
            }
        }

        // Execute the call
        let result = f().await;

        // Update state based on result
        let mut state = self.state.lock().await;
        match result {
            Ok(val) => {
                match &*state {
                    CircuitState::HalfOpen => {
                        tracing::info!("circuit breaker: half-open probe succeeded, closing");
                        *state = CircuitState::Closed {
                            failures: Vec::new(),
                        };
                    }
                    CircuitState::Closed { .. } => {
                        // Success in closed state -- nothing to do
                    }
                    CircuitState::Open { .. } => {
                        // Shouldn't happen, but just in case
                    }
                }
                Ok(val)
            }
            Err(err) => {
                let sfu_err = err.into();
                match &mut *state {
                    CircuitState::HalfOpen => {
                        tracing::warn!("circuit breaker: half-open probe failed, re-opening");
                        *state = CircuitState::Open {
                            opened_at: Instant::now(),
                        };
                    }
                    CircuitState::Closed { failures } => {
                        let now = Instant::now();
                        // Prune failures outside the window
                        failures.retain(|t| now.duration_since(*t) < self.failure_window);
                        failures.push(now);
                        if failures.len() >= self.failure_threshold as usize {
                            tracing::warn!(
                                "circuit breaker: {} failures in window, opening circuit",
                                failures.len()
                            );
                            *state = CircuitState::Open { opened_at: now };
                        }
                    }
                    CircuitState::Open { .. } => {
                        // Already open, no change
                    }
                }
                Err(sfu_err)
            }
        }
    }

    /// Check if the circuit breaker is currently open.
    pub async fn is_open(&self) -> bool {
        let state = self.state.lock().await;
        matches!(&*state, CircuitState::Open { .. })
    }

    /// Forcibly reset the circuit breaker to closed state (for testing).
    pub async fn reset(&self) {
        let mut state = self.state.lock().await;
        *state = CircuitState::Closed {
            failures: Vec::new(),
        };
    }
}

// ---------------------------------------------------------------------------
// CircuitBreakerAdapter
// ---------------------------------------------------------------------------

/// Wraps any `SfuAdapter` with a circuit breaker for resilience.
pub struct CircuitBreakerAdapter<A: SfuAdapter> {
    inner: A,
    breaker: CircuitBreaker,
}

impl<A: SfuAdapter> CircuitBreakerAdapter<A> {
    /// Wrap an existing adapter with default circuit breaker settings
    /// (3 failures, 30s recovery).
    pub fn new(inner: A) -> Self {
        Self {
            inner,
            breaker: CircuitBreaker::new(3, Duration::from_secs(30)),
        }
    }

    /// Wrap an existing adapter with a custom circuit breaker.
    pub fn with_breaker(inner: A, breaker: CircuitBreaker) -> Self {
        Self { inner, breaker }
    }

    /// Get a reference to the circuit breaker (for diagnostics).
    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }
}

#[async_trait]
impl<A: SfuAdapter> SfuAdapter for CircuitBreakerAdapter<A> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn health_check(&self) -> Result<(), SfuError> {
        let inner = &self.inner;
        self.breaker.call(|| inner.health_check()).await
    }

    async fn create_room(&self, req: CreateRoomRequest) -> Result<SfuRoom, SfuError> {
        let inner = &self.inner;
        self.breaker.call(|| inner.create_room(req.clone())).await
    }

    async fn delete_room(&self, sfu_room_id: &str) -> Result<(), SfuError> {
        let inner = &self.inner;
        let id = sfu_room_id.to_owned();
        self.breaker.call(|| inner.delete_room(&id)).await
    }

    async fn generate_token(
        &self,
        room: &SfuRoom,
        participant: &ParticipantInfo,
        permissions: ParticipantPermissions,
    ) -> Result<SfuToken, SfuError> {
        // Token generation is local (no network call), so skip circuit breaker.
        self.inner
            .generate_token(room, participant, permissions)
            .await
    }

    async fn remove_participant(
        &self,
        sfu_room_id: &str,
        participant_id: &str,
    ) -> Result<(), SfuError> {
        let inner = &self.inner;
        let room = sfu_room_id.to_owned();
        let pid = participant_id.to_owned();
        self.breaker
            .call(|| inner.remove_participant(&room, &pid))
            .await
    }

    async fn list_participants(&self, sfu_room_id: &str) -> Result<Vec<ParticipantInfo>, SfuError> {
        let inner = &self.inner;
        let room = sfu_room_id.to_owned();
        self.breaker.call(|| inner.list_participants(&room)).await
    }

    async fn room_stats(&self, sfu_room_id: &str) -> Result<RoomStats, SfuError> {
        let inner = &self.inner;
        let room = sfu_room_id.to_owned();
        self.breaker.call(|| inner.room_stats(&room)).await
    }

    async fn supports_e2ee(&self) -> bool {
        self.inner.supports_e2ee().await
    }

    async fn start_hls_egress(&self, req: HlsEgressRequest) -> Result<EgressInfo, SfuError> {
        let inner = &self.inner;
        self.breaker
            .call(|| inner.start_hls_egress(req.clone()))
            .await
    }

    async fn start_recording_egress(
        &self,
        req: RecordingEgressRequest,
    ) -> Result<EgressInfo, SfuError> {
        let inner = &self.inner;
        self.breaker
            .call(|| inner.start_recording_egress(req.clone()))
            .await
    }

    async fn stop_egress(&self, egress_id: &str) -> Result<(), SfuError> {
        let inner = &self.inner;
        let id = egress_id.to_owned();
        self.breaker.call(|| inner.stop_egress(&id)).await
    }

    async fn list_egresses(&self, room_name: &str) -> Result<Vec<EgressInfo>, SfuError> {
        let inner = &self.inner;
        let name = room_name.to_owned();
        self.breaker.call(|| inner.list_egresses(&name)).await
    }

    fn supports_egress(&self) -> bool {
        self.inner.supports_egress()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_failures() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(30))
            .with_failure_window(Duration::from_secs(60));

        let call_count = Arc::new(AtomicU32::new(0));

        // Simulate 3 failures
        for _ in 0..3 {
            let cc = call_count.clone();
            let result: Result<(), SfuError> = cb
                .call(|| async move {
                    cc.fetch_add(1, Ordering::SeqCst);
                    Err::<(), SfuError>(SfuError::ConnectionFailed("down".into()))
                })
                .await;
            assert!(result.is_err());
        }

        // All 3 calls should have gone through
        assert_eq!(call_count.load(Ordering::SeqCst), 3);

        // Circuit should now be open
        assert!(cb.is_open().await);

        // Next call should be rejected without executing
        let cc = call_count.clone();
        let result: Result<(), SfuError> = cb
            .call(|| async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok::<(), SfuError>(())
            })
            .await;
        assert!(matches!(result, Err(SfuError::CircuitOpen(_))));
        // Call count should NOT have increased
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_probe() {
        // Use a very short recovery timeout for testing
        let cb = CircuitBreaker::new(2, Duration::from_millis(50))
            .with_failure_window(Duration::from_secs(60));

        // Trip the breaker with 2 failures
        for _ in 0..2 {
            let _: Result<(), SfuError> = cb
                .call(|| async { Err::<(), SfuError>(SfuError::ConnectionFailed("down".into())) })
                .await;
        }
        assert!(cb.is_open().await);

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Next call should go through (half-open probe) and succeed
        let result: Result<&str, SfuError> = cb.call(|| async { Ok::<&str, SfuError>("ok") }).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "ok");

        // Circuit should be closed again
        assert!(!cb.is_open().await);
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_failure_reopens() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(50))
            .with_failure_window(Duration::from_secs(60));

        // Trip the breaker
        for _ in 0..2 {
            let _: Result<(), SfuError> = cb
                .call(|| async { Err::<(), SfuError>(SfuError::ConnectionFailed("down".into())) })
                .await;
        }
        assert!(cb.is_open().await);

        // Wait for recovery
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Half-open probe fails
        let result: Result<(), SfuError> = cb
            .call(|| async { Err::<(), SfuError>(SfuError::ConnectionFailed("still down".into())) })
            .await;
        assert!(result.is_err());

        // Circuit should be open again
        assert!(cb.is_open().await);
    }

    #[tokio::test]
    async fn test_circuit_breaker_success_does_not_trip() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(30));

        // Successful calls should not trip the breaker
        for _ in 0..10 {
            let result: Result<i32, SfuError> = cb.call(|| async { Ok::<i32, SfuError>(42) }).await;
            assert_eq!(result.unwrap(), 42);
        }

        assert!(!cb.is_open().await);
    }

    // ----- Egress type tests -----

    fn make_test_s3_config() -> EgressS3Config {
        EgressS3Config {
            endpoint: "https://s3.us-east-1.amazonaws.com".to_string(),
            bucket: "matrixmedia-egress".to_string(),
            region: "us-east-1".to_string(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            path_prefix: "streams/test-stream-123/".to_string(),
            force_path_style: false,
        }
    }

    #[test]
    fn test_egress_types_serialize() {
        let info = EgressInfo {
            egress_id: "EG_abc123".to_string(),
            status: EgressStatus::Active,
            room_name: "my-room".to_string(),
            started_at: Some(Utc::now()),
            output_url: Some("s3://bucket/streams/123/playlist.m3u8".to_string()),
        };

        let json = serde_json::to_string(&info).expect("should serialize EgressInfo");
        let deserialized: EgressInfo =
            serde_json::from_str(&json).expect("should deserialize EgressInfo");

        assert_eq!(deserialized.egress_id, "EG_abc123");
        assert_eq!(deserialized.status, EgressStatus::Active);
        assert_eq!(deserialized.room_name, "my-room");
        assert!(deserialized.output_url.is_some());

        // Verify Failed variant serializes the message
        let failed = EgressInfo {
            egress_id: "EG_fail".to_string(),
            status: EgressStatus::Failed("codec error".to_string()),
            room_name: "room-2".to_string(),
            started_at: None,
            output_url: None,
        };

        let json = serde_json::to_string(&failed).expect("should serialize failed EgressInfo");
        let deserialized: EgressInfo =
            serde_json::from_str(&json).expect("should deserialize failed EgressInfo");
        assert_eq!(
            deserialized.status,
            EgressStatus::Failed("codec error".to_string())
        );
    }

    #[test]
    fn test_egress_s3_config() {
        let config = make_test_s3_config();

        assert_eq!(config.endpoint, "https://s3.us-east-1.amazonaws.com");
        assert_eq!(config.bucket, "matrixmedia-egress");
        assert_eq!(config.region, "us-east-1");
        assert!(!config.force_path_style);

        // Verify it serializes/deserializes
        let json = serde_json::to_string(&config).expect("should serialize S3 config");
        let deserialized: EgressS3Config =
            serde_json::from_str(&json).expect("should deserialize S3 config");
        assert_eq!(deserialized.bucket, config.bucket);
        assert_eq!(deserialized.path_prefix, "streams/test-stream-123/");
    }

    #[test]
    fn test_hls_request_defaults() {
        let config = make_test_s3_config();
        let req = HlsEgressRequest::new("test-room".to_string(), config);

        assert_eq!(req.room_name, "test-room");
        assert_eq!(req.segment_duration_secs, 4);
        assert_eq!(req.min_playlist_size, 3);
        assert!(!req.audio_only);
    }

    /// A dummy adapter that does not support egress; used to verify the default
    /// trait implementations return the expected errors/values.
    struct NoEgressAdapter;

    #[async_trait]
    impl SfuAdapter for NoEgressAdapter {
        fn name(&self) -> &str {
            "no-egress"
        }
        async fn health_check(&self) -> Result<(), SfuError> {
            Ok(())
        }
        async fn create_room(&self, _req: CreateRoomRequest) -> Result<SfuRoom, SfuError> {
            unimplemented!()
        }
        async fn delete_room(&self, _id: &str) -> Result<(), SfuError> {
            unimplemented!()
        }
        async fn generate_token(
            &self,
            _room: &SfuRoom,
            _p: &ParticipantInfo,
            _perm: ParticipantPermissions,
        ) -> Result<SfuToken, SfuError> {
            unimplemented!()
        }
        async fn remove_participant(&self, _r: &str, _p: &str) -> Result<(), SfuError> {
            unimplemented!()
        }
        async fn list_participants(&self, _r: &str) -> Result<Vec<ParticipantInfo>, SfuError> {
            unimplemented!()
        }
        async fn room_stats(&self, _r: &str) -> Result<RoomStats, SfuError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn test_default_adapter_no_egress() {
        let adapter = NoEgressAdapter;

        // supports_egress should be false by default
        assert!(!adapter.supports_egress());

        // start_hls_egress should return an error
        let config = make_test_s3_config();
        let hls_req = HlsEgressRequest::new("room".to_string(), config.clone());
        let result = adapter.start_hls_egress(hls_req).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("egress not supported")
        );

        // start_recording_egress should return an error
        let rec_req = RecordingEgressRequest::new("room".to_string(), config);
        let result = adapter.start_recording_egress(rec_req).await;
        assert!(result.is_err());

        // stop_egress should return an error
        let result = adapter.stop_egress("EG_123").await;
        assert!(result.is_err());

        // list_egresses should return an empty list (not an error)
        let result = adapter.list_egresses("room").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // ParticipantPermissions tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_audio_host_permissions() {
        let p = ParticipantPermissions::audio_host();
        assert!(p.can_publish);
        assert!(p.can_publish_audio);
        assert!(!p.can_publish_video);
        assert!(!p.can_publish_screen);
        assert!(p.can_subscribe);
        assert!(p.can_publish_data);
        assert!(p.has_source_level_grants());

        let sources = p.allowed_sources();
        assert_eq!(sources, vec!["microphone"]);
    }

    #[test]
    fn test_video_host_permissions() {
        let p = ParticipantPermissions::video_host();
        assert!(p.can_publish);
        assert!(p.can_publish_audio);
        assert!(p.can_publish_video);
        assert!(!p.can_publish_screen);
        assert!(p.can_subscribe);
        assert!(p.can_publish_data);
        assert!(p.has_source_level_grants());

        let sources = p.allowed_sources();
        assert!(sources.contains(&"microphone".to_string()));
        assert!(sources.contains(&"camera".to_string()));
        assert!(!sources.contains(&"screen_share".to_string()));
    }

    #[test]
    fn test_screen_share_host_permissions() {
        let p = ParticipantPermissions::screen_share_host();
        assert!(p.can_publish);
        assert!(p.can_publish_audio);
        assert!(!p.can_publish_video);
        assert!(p.can_publish_screen);
        assert!(p.can_subscribe);
        assert!(p.can_publish_data);
        assert!(p.has_source_level_grants());

        let sources = p.allowed_sources();
        assert!(sources.contains(&"microphone".to_string()));
        assert!(sources.contains(&"screen_share".to_string()));
        assert!(sources.contains(&"screen_share_audio".to_string()));
        assert!(!sources.contains(&"camera".to_string()));
    }

    #[test]
    fn test_viewer_permissions() {
        let p = ParticipantPermissions::viewer();
        assert!(!p.can_publish);
        assert!(!p.can_publish_audio);
        assert!(!p.can_publish_video);
        assert!(!p.can_publish_screen);
        assert!(p.can_subscribe);
        assert!(!p.can_publish_data);
        assert!(!p.has_source_level_grants());

        let sources = p.allowed_sources();
        assert!(sources.is_empty());
    }

    // -----------------------------------------------------------------------
    // SfuMediaConfig / VideoResolution tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_sfu_media_config_defaults() {
        let cfg = SfuMediaConfig::default();
        assert!(cfg.audio_enabled);
        assert!(!cfg.video_enabled);
        assert!(!cfg.screen_share_enabled);
        assert_eq!(cfg.audio_codec, "opus");
        assert_eq!(cfg.max_audio_bitrate, 48_000);
        assert!(cfg.max_video_bitrate.is_none());
        assert!(cfg.max_video_resolution.is_none());
        assert!(!cfg.simulcast_enabled);
    }

    #[test]
    fn test_sfu_media_config_video() {
        let cfg = SfuMediaConfig {
            audio_enabled: true,
            video_enabled: true,
            screen_share_enabled: false,
            audio_codec: "opus".to_string(),
            max_audio_bitrate: 48_000,
            max_video_bitrate: Some(2_500_000),
            max_video_resolution: Some(VideoResolution {
                width: 1280,
                height: 720,
                frame_rate: 30,
            }),
            simulcast_enabled: true,
        };
        assert!(cfg.video_enabled);
        assert_eq!(cfg.max_video_bitrate, Some(2_500_000));
        let res = cfg.max_video_resolution.unwrap();
        assert_eq!(res.width, 1280);
        assert_eq!(res.height, 720);
        assert_eq!(res.frame_rate, 30);
    }

    #[test]
    fn test_create_room_request_with_media_config() {
        let req = CreateRoomRequest {
            name: "test-room".to_string(),
            max_participants: 50,
            enable_recording: false,
            media_config: SfuMediaConfig::default(),
        };
        assert_eq!(req.name, "test-room");
        assert!(req.media_config.audio_enabled);
        assert!(!req.media_config.video_enabled);
    }
}
