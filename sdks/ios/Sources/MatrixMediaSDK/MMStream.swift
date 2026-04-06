import Foundation
import Combine

/// An active stream connection.
///
/// Obtained via ``MMClient/joinStream(roomID:)`` or ``MMClient/startStream(roomID:config:)``.
/// Provides real-time state updates via published properties for SwiftUI integration.
///
/// ## SwiftUI Usage
/// ```swift
/// struct StreamView: View {
///     @ObservedObject var stream: MMStream
///
///     var body: some View {
///         VStack {
///             Text(stream.state == .connected ? "Live" : "Connecting...")
///             MMAudioRenderer(stream: stream)
///             Text("Viewers: \(stream.viewerCount)")
///             Button("Leave") {
///                 Task { await stream.leave() }
///             }
///         }
///     }
/// }
/// ```
@MainActor
public final class MMStream: ObservableObject {

    // MARK: - Public Published State

    /// Current connection state.
    @Published public private(set) var state: MMStreamState = .connecting

    /// The stream host participant, if known.
    @Published public private(set) var host: MMParticipant?

    /// Number of viewers currently connected.
    @Published public private(set) var viewerCount: Int = 0

    /// Current audio level (0.0 ... 1.0) for driving the ``MMAudioRenderer``.
    @Published public private(set) var audioLevel: Float = 0

    /// Whether the local microphone is muted (only meaningful for host streams).
    @Published public private(set) var isMuted: Bool = false

    /// Whether the stream currently has an active video track.
    @Published public private(set) var hasVideo: Bool = false

    /// Whether the active video track is a screen share (vs camera).
    @Published public private(set) var isScreenShare: Bool = false

    /// Whether end-to-end encryption is active on this stream.
    @Published public private(set) var e2eeEnabled: Bool = false

    /// The current E2EE key identifier, if E2EE is enabled.
    @Published public private(set) var e2eeKeyId: String?

    // MARK: - Public Read-Only

    /// The server-assigned stream identifier.
    public let streamID: String

    /// The Matrix room containing this stream.
    public let roomID: String

    // MARK: - Internal

    internal let bridge: LiveKitBridge
    private let apiClient: MMAPIClient
    private var cancellables = Set<AnyCancellable>()

    // MARK: - Init

    init(
        streamID: String,
        roomID: String,
        bridge: LiveKitBridge,
        apiClient: MMAPIClient,
        e2ee: MME2eeInfo? = nil
    ) {
        self.streamID = streamID
        self.roomID = roomID
        self.bridge = bridge
        self.apiClient = apiClient

        if let e2ee = e2ee, e2ee.enabled {
            self.e2eeEnabled = true
            self.e2eeKeyId = e2ee.keyId
        }

        setupBridgeBindings()
    }

    // MARK: - Public API

    /// Leave the stream as a viewer, or stop publishing as a host.
    public func leave() async {
        state = .disconnected(reason: .userInitiated)
        await bridge.disconnect()

        // Notify the server (best-effort, don't throw on failure)
        try? await apiClient.leaveStream(streamID: streamID)
    }

    /// End the stream (host only). Disconnects all participants.
    ///
    /// - Throws: ``MMError/notAuthorized`` if the caller is not the host.
    public func stop() async throws {
        try await apiClient.endStream(streamID: streamID)
        state = .disconnected(reason: .streamEnded)
        await bridge.disconnect()
    }

    /// Mute or unmute the local microphone (host only).
    ///
    /// - Parameter muted: `true` to mute, `false` to unmute.
    public func setMuted(_ muted: Bool) {
        bridge.setMicrophoneEnabled(!muted)
        isMuted = muted
    }

    /// Set the playback volume for the remote audio.
    ///
    /// - Parameter volume: Volume from 0.0 (silent) to 1.0 (full).
    public func setVolume(_ volume: Float) {
        bridge.setVolume(max(0, min(1, volume)))
    }

    /// Enable the local camera and begin publishing video.
    ///
    /// - Throws: ``MMError/notAuthorized`` if the caller lacks publish permission,
    ///           or ``MMError/deviceUnavailable`` if no camera is available.
    public func enableCamera() async throws {
        try await bridge.enableCamera()
        hasVideo = true
        isScreenShare = false
    }

    /// Disable the local camera and stop publishing video.
    public func disableCamera() async {
        await bridge.disableCamera()
        hasVideo = false
    }

    /// Enable screen sharing and begin publishing the screen capture.
    ///
    /// - Throws: ``MMError/notAuthorized`` if the caller lacks publish permission.
    public func enableScreenShare() async throws {
        try await bridge.enableScreenShare()
        hasVideo = true
        isScreenShare = true
    }

    /// Disable screen sharing.
    public func disableScreenShare() async {
        await bridge.disableScreenShare()
        hasVideo = false
        isScreenShare = false
    }

    // MARK: - Bridge Bindings

    private func setupBridgeBindings() {
        // Bind bridge state changes to published properties.
        // The LiveKitBridge updates these via callbacks set here.

        bridge.onStateChange = { [weak self] newState in
            Task { @MainActor in
                self?.state = newState
            }
        }

        bridge.onAudioLevelChange = { [weak self] level in
            Task { @MainActor in
                self?.audioLevel = level
            }
        }

        bridge.onParticipantCountChange = { [weak self] count in
            Task { @MainActor in
                self?.viewerCount = count
            }
        }

        bridge.onHostUpdate = { [weak self] participant in
            Task { @MainActor in
                self?.host = participant
            }
        }

        bridge.onVideoTrackChange = { [weak self] hasVideo, isScreenShare in
            Task { @MainActor in
                self?.hasVideo = hasVideo
                self?.isScreenShare = isScreenShare
            }
        }
    }
}
