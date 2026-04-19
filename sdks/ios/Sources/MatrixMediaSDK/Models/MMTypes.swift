import Foundation

// MARK: - Authentication

/// Token returned by the host app's token provider.
/// Contains the Matrix OpenID token fields needed to authenticate with mm-core.
public struct MMOpenIDToken: Sendable {
    public let accessToken: String
    public let tokenType: String
    public let matrixServerName: String
    public let expiresInMs: Int

    public init(accessToken: String, tokenType: String, matrixServerName: String, expiresInMs: Int) {
        self.accessToken = accessToken
        self.tokenType = tokenType
        self.matrixServerName = matrixServerName
        self.expiresInMs = expiresInMs
    }
}

/// Token pair returned after authenticating with mm-core.
/// The session JWT is short-lived (15 min); refresh token is long-lived (24h).
public struct MMAuthToken: Sendable {
    public let sessionToken: String
    public let refreshToken: String
    public let expiresAt: Date
    public let userId: String

    public init(sessionToken: String, refreshToken: String, expiresAt: Date, userId: String) {
        self.sessionToken = sessionToken
        self.refreshToken = refreshToken
        self.expiresAt = expiresAt
        self.userId = userId
    }
}

// MARK: - User

/// Authenticated MatrixMedia user.
public struct MMUser: Sendable, Identifiable {
    public let id: String
    public let displayName: String?
    public let avatarURL: URL?
    public let homeserver: String

    public init(id: String, displayName: String?, avatarURL: URL?, homeserver: String) {
        self.id = id
        self.displayName = displayName
        self.avatarURL = avatarURL
        self.homeserver = homeserver
    }
}

// MARK: - Stream Configuration

/// Configuration for creating a new stream.
public struct MMStreamConfig: Sendable {
    public let title: String?
    public let mediaType: MMMediaType
    public let maxParticipants: Int?
    public let e2ee: Bool

    /// Minimum subscription tier required to view this stream (0 = open).
    /// If `nil`, the host's `default_stream_min_tier` is used server-side.
    public let minTier: Int?

    public init(
        title: String? = nil,
        mediaType: MMMediaType = .audio,
        maxParticipants: Int? = nil,
        e2ee: Bool = false,
        minTier: Int? = nil
    ) {
        self.title = title
        self.mediaType = mediaType
        self.maxParticipants = maxParticipants
        self.e2ee = e2ee
        self.minTier = minTier
    }
}

// MARK: - E2EE

/// End-to-end encryption key material returned by mm-core for E2EE-enabled streams.
///
/// When present, the SDK configures LiveKit's frame encryption layer so that
/// media is encrypted/decrypted on-device using the provided key. The SFU only
/// sees ciphertext.
public struct MME2eeInfo: Sendable, Codable {
    public let enabled: Bool
    public let algorithm: String
    public let keyId: String
    public let keyGeneration: UInt32
    public let keyB64: String

    public init(enabled: Bool, algorithm: String, keyId: String, keyGeneration: UInt32, keyB64: String) {
        self.enabled = enabled
        self.algorithm = algorithm
        self.keyId = keyId
        self.keyGeneration = keyGeneration
        self.keyB64 = keyB64
    }

    enum CodingKeys: String, CodingKey {
        case enabled, algorithm
        case keyId = "key_id"
        case keyGeneration = "key_generation"
        case keyB64 = "key_b64"
    }
}

/// Media type for a stream.
public enum MMMediaType: String, Sendable, Codable {
    case audio
    case video
    case screenShare = "screen_share"
}

// MARK: - Stream Info

/// Server-side stream metadata returned from mm-core API.
public struct MMStreamInfo: Sendable, Identifiable, Codable {
    public let id: String
    public let roomID: String
    public let hostUserID: String
    public let mediaType: MMMediaType
    public let title: String?
    public let status: MMStreamStatus
    public let participantCount: Int
    public let startedAt: Date
    public let endedAt: Date?

    enum CodingKeys: String, CodingKey {
        case id
        case roomID = "room_id"
        case hostUserID = "host_user_id"
        case mediaType = "media_type"
        case title
        case status
        case participantCount = "participant_count"
        case startedAt = "started_at"
        case endedAt = "ended_at"
    }
}

/// Server-side stream status.
public enum MMStreamStatus: String, Sendable, Codable {
    case active
    case ended
}

// MARK: - Participant

/// A participant in a stream.
public struct MMParticipant: Sendable, Identifiable {
    public let id: String
    public let userID: String
    public let displayName: String?
    public let role: MMParticipantRole
    public let joinedAt: Date

    public init(id: String, userID: String, displayName: String?, role: MMParticipantRole, joinedAt: Date) {
        self.id = id
        self.userID = userID
        self.displayName = displayName
        self.role = role
        self.joinedAt = joinedAt
    }
}

/// Role of a participant in a stream.
public enum MMParticipantRole: String, Sendable, Codable {
    case host
    case presenter
    case viewer
}

// MARK: - Disconnect Reason

/// Reason for stream disconnection.
public enum MMDisconnectReason: Sendable {
    case userInitiated
    case streamEnded
    case kicked
    case networkError
    case serverError
    case tokenExpired
    case unknown
}

// MARK: - SFU Join Response

/// Response from mm-core /streams/{id}/join endpoint.
struct MMJoinResponse: Codable, Sendable {
    let sfuURL: String
    let sfuToken: String
    let participantID: String
    let e2ee: MME2eeInfo?
    /// mm-switch URL for direct WebRTC connection (bypasses LiveKit SFU).
    let switchURL: String?
    /// Source ID to subscribe to on mm-switch.
    let switchSourceID: String?
    /// Server-assigned viewer ID for mm-switch (must be used verbatim).
    let switchViewerID: String?
    /// HMAC token for mm-switch viewer authentication.
    let switchViewerToken: String?

    /// Whether mm-switch is available for this join.
    var useSwitch: Bool { switchURL != nil && !(switchURL?.isEmpty ?? true) }

    enum CodingKeys: String, CodingKey {
        case sfuURL = "sfu_url"
        case sfuToken = "sfu_token"
        case participantID = "participant_id"
        case e2ee
        case switchURL = "switch_url"
        case switchSourceID = "switch_source_id"
        case switchViewerID = "switch_viewer_id"
        case switchViewerToken = "switch_viewer_token"
    }
}

/// Response from mm-core /streams endpoint (create).
struct MMCreateStreamResponse: Codable, Sendable {
    let streamID: String
    let sfuURL: String
    let sfuToken: String
    let participantID: String
    let e2ee: MME2eeInfo?
    /// mm-switch URL for direct WebRTC publish (bypasses LiveKit SFU).
    let switchURL: String?
    /// Source ID to publish as on mm-switch.
    let switchSourceID: String?
    /// HMAC token for mm-switch publisher authentication.
    let switchPublisherToken: String?

    /// Whether the host should publish directly to mm-switch.
    var useSwitchPublish: Bool {
        switchURL != nil && !(switchURL?.isEmpty ?? true) &&
        switchSourceID != nil && !(switchSourceID?.isEmpty ?? true)
    }

    enum CodingKeys: String, CodingKey {
        case streamID = "stream_id"
        case sfuURL = "sfu_url"
        case sfuToken = "sfu_token"
        case participantID = "participant_id"
        case e2ee
        case switchURL = "switch_url"
        case switchSourceID = "switch_source_id"
        case switchPublisherToken = "switch_publisher_token"
    }
}

// MARK: - Video Configuration

/// Configuration for video capture and publishing.
public struct MMVideoConfig: Sendable, Codable {
    /// Maximum video bitrate in bits per second.
    public let maxBitrate: Int
    /// Maximum video width in pixels.
    public let maxWidth: Int
    /// Maximum video height in pixels.
    public let maxHeight: Int
    /// Maximum frame rate (frames per second).
    public let maxFrameRate: Int
    /// Whether simulcast is enabled (multiple quality layers).
    public let simulcastEnabled: Bool

    public init(
        maxBitrate: Int = 1_500_000,
        maxWidth: Int = 1280,
        maxHeight: Int = 720,
        maxFrameRate: Int = 30,
        simulcastEnabled: Bool = true
    ) {
        self.maxBitrate = maxBitrate
        self.maxWidth = maxWidth
        self.maxHeight = maxHeight
        self.maxFrameRate = maxFrameRate
        self.simulcastEnabled = simulcastEnabled
    }
}

// MARK: - Recording (VoD)

/// A recorded stream available for playback after the live stream ends.
public struct MMRecording: Sendable, Codable, Identifiable {
    public let id: String
    public let streamId: String
    public let hostUserId: String
    public let mediaType: MMMediaType
    public let title: String?
    public let status: MMRecordingStatus
    public let durationMs: Int64?
    public let sizeBytes: Int64?
    public let playbackUrl: String?
    public let mxcUrl: String?
    public let createdAt: Date

    public init(
        id: String,
        streamId: String,
        hostUserId: String,
        mediaType: MMMediaType,
        title: String?,
        status: MMRecordingStatus,
        durationMs: Int64?,
        sizeBytes: Int64?,
        playbackUrl: String?,
        mxcUrl: String?,
        createdAt: Date
    ) {
        self.id = id
        self.streamId = streamId
        self.hostUserId = hostUserId
        self.mediaType = mediaType
        self.title = title
        self.status = status
        self.durationMs = durationMs
        self.sizeBytes = sizeBytes
        self.playbackUrl = playbackUrl
        self.mxcUrl = mxcUrl
        self.createdAt = createdAt
    }

    enum CodingKeys: String, CodingKey {
        case id
        case streamId = "stream_id"
        case hostUserId = "host_user_id"
        case mediaType = "media_type"
        case title
        case status
        case durationMs = "duration_ms"
        case sizeBytes = "size_bytes"
        case playbackUrl = "playback_url"
        case mxcUrl = "mxc_url"
        case createdAt = "created_at"
    }
}

/// Status of a recorded stream.
public enum MMRecordingStatus: String, Sendable, Codable {
    case recording
    case processing
    case ready
    case failed
    case deleted
}

/// Error response envelope from mm-core.
struct MMErrorResponse: Codable, Sendable {
    let error: String
    let message: String
    let retryAfterMs: Int?

    enum CodingKeys: String, CodingKey {
        case error
        case message
        case retryAfterMs = "retry_after_ms"
    }
}
