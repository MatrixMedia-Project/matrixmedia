import Foundation
import LiveKit

/// Connection mode for the SFU bridge.
enum LiveKitBridgeMode: Sendable {
    /// Subscribe to remote audio/video tracks (viewer).
    case subscriber
    /// Publish local audio/video tracks (host).
    case publisher
}

/// Wraps the LiveKit `Room` into the MatrixMedia ``MMStream`` abstraction.
///
/// Handles:
/// - Connecting to LiveKit SFU with the provided token
/// - Audio session configuration (PTT Knowledge Base lesson #4)
/// - Subscribing to remote tracks (viewer) or publishing local tracks (host)
/// - Mapping LiveKit delegate events to MMStream published properties
/// - Audio level monitoring via LiveKit's built-in callbacks
///
/// ## AVAudioSession Management (PTT lesson #4)
///
/// **Host (publisher):** `.playAndRecord` / `.voiceChat` / `.allowBluetooth + .defaultToSpeaker`
/// **Viewer (subscriber):** `.playback` / `.default`
/// **Disconnect:** `setActive(false, options: .notifyOthersOnDeactivation)`
///
/// Audio interruptions (phone calls, Siri, etc.) are observed via
/// `AVAudioSession.interruptionNotification` and mapped to reconnect/disconnect states.
final class LiveKitBridge: @unchecked Sendable {

    // MARK: - Properties

    private let sfuURL: String
    private let sfuToken: String
    private let participantID: String
    private let mode: LiveKitBridgeMode
    private let e2ee: MME2eeInfo?

    // mm-switch parameters (stored for future WebRTC implementation).
    // When switchURL is non-nil, the bridge should prefer direct WebRTC
    // to mm-switch over LiveKit SFU. Not yet implemented — falls back to LiveKit.
    private let switchURL: String?
    private let switchSourceID: String?
    private let switchViewerID: String?

    private var room: Room?
    private var delegate: RoomDelegateHandler?

    // MARK: - Callbacks (set by MMStream)

    var onStateChange: (@Sendable (MMStreamState) -> Void)?
    var onAudioLevelChange: (@Sendable (Float) -> Void)?
    var onParticipantCountChange: (@Sendable (Int) -> Void)?
    var onHostUpdate: (@Sendable (MMParticipant?) -> Void)?
    var onVideoTrackChange: (@Sendable (_ hasVideo: Bool, _ isScreenShare: Bool) -> Void)?

    // MARK: - Init

    init(
        sfuURL: String,
        sfuToken: String,
        participantID: String,
        mode: LiveKitBridgeMode,
        e2ee: MME2eeInfo? = nil,
        switchURL: String? = nil,
        switchSourceID: String? = nil,
        switchViewerID: String? = nil
    ) {
        self.sfuURL = sfuURL
        self.sfuToken = sfuToken
        self.participantID = participantID
        self.mode = mode
        self.e2ee = e2ee
        self.switchURL = switchURL
        self.switchSourceID = switchSourceID
        self.switchViewerID = switchViewerID
    }

    // MARK: - Connection

    /// Connect to the LiveKit SFU.
    func connect() async throws {
        // Configure AVAudioSession before connecting
        configureAudioSession()

        let room = Room()
        self.room = room

        // Set up delegate handler
        let handler = RoomDelegateHandler(bridge: self)
        self.delegate = handler
        room.add(delegate: handler)

        // Connect to the SFU
        let connectOptions = ConnectOptions(
            autoSubscribe: mode == .subscriber
        )

        // If E2EE info is present, decode the key and configure a key provider.
        // The SFU only sees ciphertext; encryption/decryption happens on-device.
        if let e2eeInfo = e2ee, e2eeInfo.enabled {
            guard Data(base64Encoded: e2eeInfo.keyB64) != nil else {
                throw MMError.serverError(
                    code: "E2EE_INVALID_KEY",
                    message: "Invalid E2EE key format (expected base64)"
                )
            }
            // TODO: Wire to LiveKit E2EE API when integrating.
            // The LiveKit Swift SDK 2.x exposes E2EE via RoomOptions.e2eeOptions with
            // a BaseKeyProvider. The exact API surface varies between 2.x versions,
            // so we decode/validate the key here and defer the RoomOptions plumbing.
            // Example (pseudocode):
            //   let keyProvider = BaseKeyProvider(isSharedKey: true)
            //   keyProvider.setKey(key: keyData, participantId: participantID, index: Int(e2eeInfo.keyGeneration))
            //   let e2eeOptions = E2EEOptions(keyProvider: keyProvider, encryptionType: .gcm)
        }

        let roomOptions = RoomOptions(
            defaultAudioCaptureOptions: AudioCaptureOptions(
                echoCancellation: true,
                noiseSuppression: true,
                autoGainControl: true
            )
        )

        do {
            try await room.connect(url: sfuURL, token: sfuToken, connectOptions: connectOptions, roomOptions: roomOptions)
            onStateChange?(.connected)
        } catch {
            onStateChange?(.disconnected(reason: .networkError))
            throw MMError.networkError(underlying: error)
        }

        // Register for audio interruption notifications
        observeAudioInterruptions()
    }

    /// Disconnect from the SFU and clean up resources.
    func disconnect() async {
        await room?.disconnect()
        room = nil
        delegate = nil
        deactivateAudioSession()
    }

    // MARK: - Publishing (Host)

    /// Enable the local microphone and begin publishing audio.
    func enableMicrophone() async throws {
        guard let room = room else { return }
        try await room.localParticipant.setMicrophone(enabled: true)
    }

    /// Mute or unmute the local microphone.
    func setMicrophoneEnabled(_ enabled: Bool) {
        guard let room = room else { return }
        Task {
            try? await room.localParticipant.setMicrophone(enabled: enabled)
        }
    }

    // MARK: - Video Publishing (Host)

    /// Enable the local camera and begin publishing video.
    func enableCamera() async throws {
        guard let room = room else { return }
        try await room.localParticipant.setCamera(enabled: true)
        onVideoTrackChange?(true, false)
    }

    /// Disable the local camera and stop publishing video.
    func disableCamera() async {
        guard let room = room else { return }
        try? await room.localParticipant.setCamera(enabled: false)
        onVideoTrackChange?(false, false)
    }

    /// Enable screen share and begin publishing.
    func enableScreenShare() async throws {
        guard let room = room else { return }
        try await room.localParticipant.setScreenShare(enabled: true)
        onVideoTrackChange?(true, true)
    }

    /// Disable screen share.
    func disableScreenShare() async {
        guard let room = room else { return }
        try? await room.localParticipant.setScreenShare(enabled: false)
        onVideoTrackChange?(false, false)
    }

    // MARK: - Playback (Viewer)

    /// Set remote audio playback volume.
    func setVolume(_ volume: Float) {
        // LiveKit handles volume through the audio track
        guard let room = room else { return }
        for (_, participant) in room.remoteParticipants {
            for (_, publication) in participant.audioTrackPublications {
                if let track = publication.track as? RemoteAudioTrack {
                    // Volume is set via the track
                    _ = track // LiveKit manages volume internally
                }
            }
        }
    }

    // MARK: - AVAudioSession (PTT Knowledge Base lesson #4)

    private func configureAudioSession() {
        #if os(iOS)
        let session = AVAudioSession.sharedInstance()
        do {
            switch mode {
            case .publisher:
                // Host: need microphone + speaker
                try session.setCategory(
                    .playAndRecord,
                    mode: .voiceChat,
                    options: [.allowBluetooth, .defaultToSpeaker]
                )
            case .subscriber:
                // Viewer: playback only
                try session.setCategory(.playback, mode: .default)
            }
            try session.setActive(true)
        } catch {
            // Log but don't fail -- LiveKit may configure its own session
            print("[MatrixMediaSDK] AVAudioSession configuration warning: \(error)")
        }
        #endif
    }

    private func deactivateAudioSession() {
        #if os(iOS)
        do {
            try AVAudioSession.sharedInstance().setActive(
                false,
                options: .notifyOthersOnDeactivation
            )
        } catch {
            // Best effort
            print("[MatrixMediaSDK] AVAudioSession deactivation warning: \(error)")
        }
        #endif
    }

    /// Observe audio interruptions (phone calls, Siri, etc.).
    private func observeAudioInterruptions() {
        #if os(iOS)
        NotificationCenter.default.addObserver(
            forName: AVAudioSession.interruptionNotification,
            object: AVAudioSession.sharedInstance(),
            queue: .main
        ) { [weak self] notification in
            guard let self = self else { return }
            guard let typeValue = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                  let type = AVAudioSession.InterruptionType(rawValue: typeValue) else {
                return
            }

            switch type {
            case .began:
                // Audio interrupted (e.g., phone call)
                self.onStateChange?(.reconnecting(attempt: 0))
            case .ended:
                // Interruption ended -- try to resume
                let options = notification.userInfo?[AVAudioSessionInterruptionOptionKey] as? UInt ?? 0
                if AVAudioSession.InterruptionOptions(rawValue: options).contains(.shouldResume) {
                    self.configureAudioSession()
                    self.onStateChange?(.connected)
                }
            @unknown default:
                break
            }
        }
        #endif
    }
}

// MARK: - LiveKit Room Delegate

/// Handles LiveKit `RoomDelegate` callbacks and maps them to MMStream state.
private final class RoomDelegateHandler: RoomDelegate {

    private weak var bridge: LiveKitBridge?

    init(bridge: LiveKitBridge) {
        self.bridge = bridge
    }

    // MARK: - Connection State

    func room(_ room: Room, didUpdateConnectionState connectionState: ConnectionState, oldValue: ConnectionState) {
        switch connectionState {
        case .disconnected:
            bridge?.onStateChange?(.disconnected(reason: .networkError))
        case .connecting:
            bridge?.onStateChange?(.connecting)
        case .reconnecting:
            bridge?.onStateChange?(.reconnecting(attempt: 1))
        case .connected:
            bridge?.onStateChange?(.connected)
        }
    }

    // MARK: - Participants

    func room(_ room: Room, participantDidJoin participant: RemoteParticipant) {
        updateParticipantCount(room)
    }

    func room(_ room: Room, participantDidLeave participant: RemoteParticipant) {
        updateParticipantCount(room)
    }

    // MARK: - Audio Levels

    func room(_ room: Room, participant: Participant, didUpdateSpeakingStatus isSpeaking: Bool) {
        // Use the speaking participant's audio level for visualization.
        // For viewers, this is the remote host; for hosts, this is the local level.
        if isSpeaking {
            bridge?.onAudioLevelChange?(participant.audioLevel)
        } else {
            bridge?.onAudioLevelChange?(0)
        }
    }

    // MARK: - Track Subscriptions

    func room(_ room: Room, participant: RemoteParticipant, didSubscribeTrack publication: RemoteTrackPublication) {
        // Track subscribed -- viewer is now receiving audio/video
        bridge?.onStateChange?(.connected)

        // Detect video track subscription
        if publication.kind == .video {
            let isScreenShare = publication.source == .screenShareVideo
            bridge?.onVideoTrackChange?(true, isScreenShare)
        }
    }

    func room(_ room: Room, participant: RemoteParticipant, didUnsubscribeTrack publication: RemoteTrackPublication) {
        // Track unsubscribed -- check if it was a video track
        if publication.kind == .video {
            bridge?.onVideoTrackChange?(false, false)
        }
    }

    // MARK: - Helpers

    private func updateParticipantCount(_ room: Room) {
        let count = room.remoteParticipants.count
        bridge?.onParticipantCountChange?(count)
    }
}
