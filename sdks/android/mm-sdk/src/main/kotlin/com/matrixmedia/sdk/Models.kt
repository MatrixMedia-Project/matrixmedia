package com.matrixmedia.sdk

/**
 * MatrixMedia SDK public types.
 *
 * All data classes in this file are part of the public API surface.
 * Breaking changes require a major version bump.
 */

// -- Authentication --

/**
 * MM server authentication token.
 *
 * @param accessToken JWT issued by the MM server.
 * @param matrixServerName The Matrix server name associated with this token.
 */
data class MMAuthToken(
    val accessToken: String,
    val matrixServerName: String
)

/**
 * Matrix OpenID token used to authenticate with the MM server.
 * Obtained from the Matrix homeserver via `/_matrix/client/v3/user/{userId}/openid/request_token`.
 *
 * @param accessToken The OpenID access token.
 * @param tokenType Token type (typically "Bearer").
 * @param matrixServerName The Matrix server name that issued this token.
 * @param expiresIn Token lifetime in seconds.
 */
data class MMOpenIDToken(
    val accessToken: String,
    val tokenType: String,
    val matrixServerName: String,
    val expiresIn: Int
)

/**
 * Authenticated MM user.
 */
data class MMUser(
    val userId: String,
    val displayName: String?,
    val avatarUrl: String?
)

// -- Stream configuration --

/**
 * Configuration for starting a new stream.
 *
 * @param mediaType The type of media for this stream. Defaults to [MMMediaType.Audio].
 * @param title Optional human-readable stream title.
 * @param e2ee Whether to request end-to-end encryption for this stream. Defaults to false.
 *   When true, the MM server returns an [MME2eeInfo] block with the key material,
 *   and the SDK configures LiveKit's frame encryption so the SFU only sees ciphertext.
 */
data class MMStreamConfig(
    val mediaType: MMMediaType = MMMediaType.Audio,
    val title: String? = null,
    val e2ee: Boolean = false
)

/**
 * End-to-end encryption key material returned by mm-core for E2EE-enabled streams.
 *
 * When present, the SDK configures LiveKit's frame encryption layer so that
 * media is encrypted/decrypted on-device using the provided key. The SFU only
 * sees ciphertext.
 *
 * @param enabled Whether E2EE is active for this stream.
 * @param algorithm The AEAD algorithm identifier (e.g., "aes-gcm").
 * @param keyId Opaque key identifier (e.g., for key rotation tracking).
 * @param keyGeneration Monotonic generation counter for the key.
 * @param keyB64 Base64-encoded raw key bytes.
 */
data class MME2eeInfo(
    val enabled: Boolean,
    val algorithm: String,
    val keyId: String,
    val keyGeneration: Int,
    val keyB64: String
)

/**
 * Media types supported by MM streams.
 */
enum class MMMediaType {
    Audio,
    Video,
    ScreenShare
}

// -- Stream state --

/**
 * Represents the current state of an [MMStream].
 */
sealed class MMStreamState {
    /** Establishing connection to the SFU. */
    object Connecting : MMStreamState()

    /** Connected and streaming. */
    object Connected : MMStreamState()

    /** Temporarily disconnected; attempting to reconnect. */
    object Reconnecting : MMStreamState()

    /** Disconnected from the stream. */
    data class Disconnected(val reason: MMDisconnectReason) : MMStreamState()

    /** An unrecoverable error occurred. */
    data class Error(val error: MMException) : MMStreamState()
}

/**
 * Top-level connection state for [MMClient].
 */
enum class MMConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting
}

/**
 * Reason for stream disconnection.
 */
sealed class MMDisconnectReason {
    /** The local user chose to leave. */
    object UserInitiated : MMDisconnectReason()

    /** The host ended the stream. */
    object HostEnded : MMDisconnectReason()

    /** Network connectivity was lost. */
    object NetworkError : MMDisconnectReason()

    /** The user was kicked by a moderator. */
    object Kicked : MMDisconnectReason()

    /** The server returned an error. */
    data class ServerError(val message: String) : MMDisconnectReason()
}

// -- Participants --

/**
 * A participant in an MM stream.
 */
data class MMParticipant(
    val id: String,
    val userId: String,
    val displayName: String?,
    val isHost: Boolean
)

// -- Video configuration --

/**
 * Configuration for video capture and publishing.
 *
 * @param maxBitrate Maximum video bitrate in bits per second.
 * @param maxWidth Maximum video width in pixels.
 * @param maxHeight Maximum video height in pixels.
 * @param maxFrameRate Maximum frame rate (frames per second).
 * @param simulcastEnabled Whether simulcast is enabled (multiple quality layers).
 */
data class MMVideoConfig(
    val maxBitrate: Int = 1_500_000,
    val maxWidth: Int = 1280,
    val maxHeight: Int = 720,
    val maxFrameRate: Int = 30,
    val simulcastEnabled: Boolean = true
)

// -- Recording (VoD) --

/**
 * A recorded stream available for playback after the live stream has ended.
 *
 * @param id Recording identifier.
 * @param streamId The stream this recording belongs to.
 * @param hostUserId Matrix user ID of the original stream host.
 * @param mediaType Media type of the recording (audio/video/screen share).
 * @param title Optional human-readable title inherited from the stream.
 * @param status Current processing status.
 * @param durationMs Duration of the recording in milliseconds. Null until ready.
 * @param sizeBytes File size in bytes. Null until ready.
 * @param playbackUrl Direct HTTP(S) CDN URL for streaming playback.
 * @param mxcUrl Matrix content URI (mxc://) for recordings stored in Matrix media.
 * @param createdAt ISO-8601 creation timestamp.
 */
data class MMRecording(
    val id: String,
    val streamId: String,
    val hostUserId: String,
    val mediaType: MMMediaType,
    val title: String?,
    val status: MMRecordingStatus,
    val durationMs: Long?,
    val sizeBytes: Long?,
    val playbackUrl: String?,
    val mxcUrl: String?,
    val createdAt: String
)

/**
 * Processing status of a recorded stream.
 */
enum class MMRecordingStatus {
    Recording,
    Processing,
    Ready,
    Failed,
    Deleted
}

// -- Internal API response models (not public API but used across internal packages) --

internal data class MMStreamInfo(
    val streamId: String,
    val roomId: String,
    val hostUserId: String,
    val mediaType: String,
    val title: String?,
    val status: String,
    val participantCount: Int
)

internal data class MMJoinResult(
    val streamId: String,
    val sfuUrl: String,
    val sfuToken: String,
    val participantId: String,
    val e2ee: MME2eeInfo? = null
)

internal data class MMCreateResult(
    val streamId: String,
    val sfuUrl: String,
    val sfuToken: String,
    val e2ee: MME2eeInfo? = null
)

internal data class MMTokenResponse(
    val accessToken: String,
    val refreshToken: String,
    val expiresInSeconds: Int,
    val userId: String,
    val displayName: String?,
    val avatarUrl: String?
)

internal data class MMRefreshResponse(
    val accessToken: String,
    val refreshToken: String,
    val expiresInSeconds: Int
)
