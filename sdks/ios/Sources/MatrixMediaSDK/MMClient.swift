import Foundation
import Combine

/// Entry point for the MatrixMedia SDK.
///
/// ## Usage
/// ```swift
/// let client = MMClient(
///     serverURL: URL(string: "https://mm.example.com")!,
///     tokenProvider: {
///         // Return a fresh Matrix OpenID token each time.
///         // NEVER cache the token -- this closure is called when a new token is needed.
///         return try await myMatrixClient.getOpenIDToken()
///     }
/// )
///
/// let user = try await client.authenticate()
/// let stream = try await client.joinStream(roomID: "!abc:example.org")
/// ```
///
/// ## Token Provider Contract
///
/// The `tokenProvider` closure is called whenever the SDK needs a fresh Matrix OpenID token.
/// This design avoids the stale-token bug from PTT v1 (Knowledge Base lesson #9):
/// **never cache the token; always return a fresh one from your Matrix SDK.**
///
/// ## Server URL
///
/// In release builds the server URL must use HTTPS. In debug builds, HTTP and localhost
/// are permitted for development.
@MainActor
public final class MMClient: ObservableObject {

    // MARK: - Public Properties

    /// Current connection state of the client.
    @Published public private(set) var connectionState: MMConnectionState = .disconnected

    /// The authenticated user, if any.
    @Published public private(set) var currentUser: MMUser?

    // MARK: - Internal Dependencies

    let serverURL: URL
    let tokenProvider: @Sendable () async throws -> MMOpenIDToken
    let apiClient: MMAPIClient
    let tokenManager: TokenManager

    // MARK: - Private State

    private var activeStreams: [String: MMStream] = [:]

    // MARK: - Init

    /// Creates a new MatrixMedia client.
    ///
    /// - Parameters:
    ///   - serverURL: Base URL of the mm-core server (e.g., `https://mm.example.com`).
    ///   - tokenProvider: Async closure that returns a **fresh** Matrix OpenID token.
    ///     Called on every authentication attempt. Must not cache tokens.
    public init(
        serverURL: URL,
        tokenProvider: @escaping @Sendable () async throws -> MMOpenIDToken
    ) {
        #if !DEBUG
        // PTT lesson #11: reject localhost in release builds.
        // Mobile devices cannot reach localhost -- must use LAN IP or public domain.
        precondition(
            serverURL.host != "localhost" && serverURL.host != "127.0.0.1",
            "MatrixMediaSDK: localhost is not reachable from device. Use a LAN IP or public domain."
        )
        precondition(
            serverURL.scheme == "https",
            "MatrixMediaSDK: HTTPS is required in release builds."
        )
        #endif

        self.serverURL = serverURL
        self.tokenProvider = tokenProvider
        self.tokenManager = TokenManager(serverURL: serverURL, tokenProvider: tokenProvider)
        self.apiClient = MMAPIClient(baseURL: serverURL, tokenManager: tokenManager)

        // Wire circular dependency: TokenManager needs MMAPIClient for token exchange.
        self.tokenManager.apiClient = self.apiClient
    }

    // MARK: - Authentication

    /// Authenticate with the mm-core server.
    ///
    /// Calls the `tokenProvider` to obtain a Matrix OpenID token, exchanges it with mm-core
    /// for an MM session JWT, and stores the session internally.
    ///
    /// - Returns: The authenticated ``MMUser``.
    /// - Throws: ``MMError/notAuthenticated`` if the token provider fails,
    ///   or ``MMError/serverError(code:message:)`` for server-side errors.
    @discardableResult
    public func authenticate() async throws -> MMUser {
        connectionState = .authenticating
        do {
            let user = try await tokenManager.authenticate()
            currentUser = user
            connectionState = .connected
            return user
        } catch {
            connectionState = .disconnected
            throw error
        }
    }

    // MARK: - Join Stream

    /// Join an existing stream as a listener.
    ///
    /// - Parameter roomID: The Matrix room ID containing the active stream.
    /// - Returns: An ``MMStream`` connected to the live audio/video feed.
    /// - Throws: ``MMError/notAuthenticated`` if not authenticated,
    ///   ``MMError/streamNotFound`` if no active stream in the room.
    public func joinStream(roomID: String) async throws -> MMStream {
        guard connectionState == .connected else {
            throw MMError.notAuthenticated
        }

        // 1. Get active stream info for the room
        let streamInfo = try await apiClient.getActiveStream(roomID: roomID)

        // 2. Join the stream (returns SFU token)
        let joinResponse = try await apiClient.joinStream(streamID: streamInfo.id)

        // 3. Create LiveKit bridge in subscriber mode (with E2EE info if present)
        // When mm-switch URL is present, the viewer can connect directly to
        // mm-switch via WebRTC for lower latency and server-side ad insertion.
        // NOTE: mm-switch direct WebRTC is not yet implemented in the iOS SDK.
        // The bridge falls back to LiveKit SFU for now. See: services/mm-switch/
        let bridge = LiveKitBridge(
            sfuURL: joinResponse.sfuURL,
            sfuToken: joinResponse.sfuToken,
            participantID: joinResponse.participantID,
            mode: .subscriber,
            e2ee: joinResponse.e2ee,
            switchURL: joinResponse.switchURL,
            switchSourceID: joinResponse.switchSourceID,
            switchViewerID: joinResponse.switchViewerID
        )

        // 4. Connect to the SFU (or mm-switch when implemented)
        try await bridge.connect()

        // 5. Create and track the stream
        let stream = MMStream(
            streamID: streamInfo.id,
            roomID: roomID,
            bridge: bridge,
            apiClient: apiClient,
            e2ee: joinResponse.e2ee
        )
        activeStreams[streamInfo.id] = stream
        return stream
    }

    // MARK: - Start Stream

    /// Create and start a new stream as a host.
    ///
    /// Requires microphone permission for audio streams. The SDK will check permission
    /// and throw ``MMError/microphonePermissionDenied`` if not granted.
    ///
    /// - Parameters:
    ///   - roomID: The Matrix room ID to stream in.
    ///   - config: Stream configuration (media type, title, max participants).
    /// - Returns: An ``MMStream`` connected as the host/publisher.
    /// - Throws: ``MMError/microphonePermissionDenied``, ``MMError/notAuthenticated``,
    ///   ``MMError/streamAlreadyActive``.
    public func startStream(
        roomID: String,
        config: MMStreamConfig = MMStreamConfig()
    ) async throws -> MMStream {
        guard connectionState == .connected else {
            throw MMError.notAuthenticated
        }

        // Check microphone permission for audio/video streams
        if config.mediaType == .audio || config.mediaType == .video {
            try await checkMicrophonePermission()
        }

        // 1. Create the stream on mm-core
        let createResponse = try await apiClient.createStream(roomID: roomID, config: config)

        // 2. Create LiveKit bridge in publisher mode (with E2EE info if present)
        // When mm-switch URL is present, the host can publish directly to
        // mm-switch via WebRTC (bypasses LiveKit for the streaming path).
        // NOTE: mm-switch direct publish is not yet implemented in the iOS SDK.
        // Falls back to LiveKit SFU for now. See: services/mm-switch/
        let bridge = LiveKitBridge(
            sfuURL: createResponse.sfuURL,
            sfuToken: createResponse.sfuToken,
            participantID: createResponse.participantID,
            mode: .publisher,
            e2ee: createResponse.e2ee,
            switchURL: createResponse.switchURL,
            switchSourceID: createResponse.switchSourceID
        )

        // 3. Connect and start publishing
        try await bridge.connect()
        try await bridge.enableMicrophone()

        // 4. Create and track the stream
        let stream = MMStream(
            streamID: createResponse.streamID,
            roomID: roomID,
            bridge: bridge,
            apiClient: apiClient,
            e2ee: createResponse.e2ee
        )
        activeStreams[createResponse.streamID] = stream
        return stream
    }

    // MARK: - Disconnect

    /// Disconnect from all active streams and clear the session.
    public func disconnect() async {
        for (_, stream) in activeStreams {
            await stream.leave()
        }
        activeStreams.removeAll()
        tokenManager.clearSession()
        currentUser = nil
        connectionState = .disconnected
    }

    // MARK: - Stream Queries

    /// Fetch active streams in a Matrix room.
    ///
    /// - Parameter roomID: The Matrix room ID.
    /// - Returns: Array of ``MMStreamInfo`` for active streams.
    public func listStreams(roomID: String) async throws -> [MMStreamInfo] {
        guard connectionState == .connected else {
            throw MMError.notAuthenticated
        }
        return try await apiClient.listStreams(roomID: roomID)
    }

    // MARK: - Recordings (VoD)

    /// List ready recordings in a Matrix room.
    ///
    /// - Parameters:
    ///   - roomID: The Matrix room ID.
    ///   - limit: Maximum number of recordings to return (default 20).
    /// - Returns: Array of ``MMRecording`` with status `.ready`.
    public func listRoomRecordings(roomID: String, limit: Int = 20) async throws -> [MMRecording] {
        guard connectionState == .connected else {
            throw MMError.notAuthenticated
        }
        let response = try await apiClient.listRoomRecordings(roomId: roomID, limit: limit)
        return response.recordings
    }

    /// Get details for a specific recording.
    ///
    /// - Parameter recordingID: The recording identifier.
    /// - Returns: The ``MMRecording``.
    public func getRecording(recordingID: String) async throws -> MMRecording {
        guard connectionState == .connected else {
            throw MMError.notAuthenticated
        }
        return try await apiClient.getRecording(recordingId: recordingID)
    }

    /// Delete a recording. Only the host of the original stream may delete its recording.
    ///
    /// - Parameter recordingID: The recording identifier.
    public func deleteRecording(recordingID: String) async throws {
        guard connectionState == .connected else {
            throw MMError.notAuthenticated
        }
        try await apiClient.deleteRecording(recordingId: recordingID)
    }

    // MARK: - Internal

    func removeStream(_ streamID: String) {
        activeStreams.removeValue(forKey: streamID)
    }

    // MARK: - Microphone Permission (PTT lesson #4)

    #if canImport(AVFoundation)
    private func checkMicrophonePermission() async throws {
        #if os(iOS) || os(macOS)
        @preconcurrency import AVFoundation
        let session = AVAudioSession.sharedInstance()
        switch session.recordPermission {
        case .granted:
            return
        case .denied:
            throw MMError.microphonePermissionDenied
        case .undetermined:
            let granted = await withCheckedContinuation { continuation in
                session.requestRecordPermission { granted in
                    continuation.resume(returning: granted)
                }
            }
            if !granted {
                throw MMError.microphonePermissionDenied
            }
        @unknown default:
            throw MMError.microphonePermissionDenied
        }
        #endif
    }
    #else
    private func checkMicrophonePermission() async throws {
        // No-op on platforms without AVFoundation
    }
    #endif
}
