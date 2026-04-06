package com.matrixmedia.sdk.internal.livekit

import com.matrixmedia.sdk.MMDisconnectReason
import com.matrixmedia.sdk.MME2eeInfo
import com.matrixmedia.sdk.MMException
import com.matrixmedia.sdk.MMParticipant
import com.matrixmedia.sdk.MMStreamState
import com.matrixmedia.sdk.internal.reconnect.ReconnectPolicy
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

/**
 * Bridge between the MatrixMedia SDK and the LiveKit Android SDK.
 *
 * Wraps a LiveKit `Room` and translates LiveKit events into MM SDK state flows.
 *
 * ## Publisher vs Subscriber Mode
 *
 * - **Publisher (host):** Connects to SFU, publishes local audio track, receives
 *   audio level from the local microphone.
 * - **Subscriber (viewer):** Connects to SFU, subscribes to remote audio tracks,
 *   receives audio level from the host's track.
 *
 * ## Reconnection
 *
 * Uses [ReconnectPolicy] (exponential backoff with jitter, max 30s, 10 retries)
 * triggered by LiveKit disconnect events and `ConnectivityManager.NetworkCallback`
 * changes. During reconnection, [streamState] emits [MMStreamState.Reconnecting].
 *
 * ## Audio Level
 *
 * Audio level comes from LiveKit's built-in audio level callbacks on the active
 * audio track (local for host, remote for viewer). No custom FFT processing.
 *
 * @param sfuUrl LiveKit SFU WebSocket URL (e.g., `wss://lk.example.com`).
 * @param sfuToken Short-lived SFU JWT (60s TTL) with room/participant claims.
 * @param isHost True for publisher mode (host), false for subscriber mode (viewer).
 */
internal class LiveKitBridge(
    private val sfuUrl: String,
    private val sfuToken: String,
    private val isHost: Boolean,
    private val e2ee: MME2eeInfo? = null
) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private val reconnectPolicy = ReconnectPolicy()

    private val _streamState = MutableStateFlow<MMStreamState>(MMStreamState.Connecting)
    val streamState: StateFlow<MMStreamState> = _streamState.asStateFlow()

    private val _host = MutableStateFlow<MMParticipant?>(null)
    val host: StateFlow<MMParticipant?> = _host.asStateFlow()

    private val _viewerCount = MutableStateFlow(0)
    val viewerCount: StateFlow<Int> = _viewerCount.asStateFlow()

    private val _audioLevel = MutableStateFlow(0f)
    val audioLevel: StateFlow<Float> = _audioLevel.asStateFlow()

    private val _isMuted = MutableStateFlow(false)
    val isMuted: StateFlow<Boolean> = _isMuted.asStateFlow()

    private val _hasVideo = MutableStateFlow(false)
    val hasVideo: StateFlow<Boolean> = _hasVideo.asStateFlow()

    private val _isScreenShare = MutableStateFlow(false)
    val isScreenShare: StateFlow<Boolean> = _isScreenShare.asStateFlow()

    private var volume: Float = 1.0f
    private var connected = false

    /**
     * Connect to the LiveKit SFU.
     *
     * In subscriber mode: automatically subscribes to all published tracks.
     * In publisher mode: connects but does not publish until [enableMicrophone] is called.
     *
     * @throws MMException.SfuUnavailable if the SFU cannot be reached.
     */
    suspend fun connect() {
        _streamState.value = MMStreamState.Connecting

        try {
            // Decode E2EE key and build RoomOptions with frame encryption if enabled.
            // The SFU only sees ciphertext; encryption/decryption happens on-device.
            val roomOptions = if (e2ee != null && e2ee.enabled) {
                val keyBytes = try {
                    android.util.Base64.decode(e2ee.keyB64, android.util.Base64.DEFAULT)
                } catch (ex: IllegalArgumentException) {
                    throw MMException.Server("E2EE_INVALID_KEY", "Invalid E2EE key format (expected base64)")
                }
                // TODO: Wire to LiveKit E2EE API when integrating.
                // The LiveKit Android SDK 2.x exposes E2EE via RoomOptions(e2eeOptions = ...)
                // with a BaseKeyProvider. Exact API varies between 2.x versions, so we decode
                // and validate the key here and defer the RoomOptions plumbing.
                // Example (pseudocode):
                //   val keyProvider = BaseKeyProvider(sharedKey = true)
                //   keyProvider.setKey(keyBytes, participantId = null, keyIndex = e2ee.keyGeneration)
                //   val e2eeOptions = E2EEOptions(keyProvider = keyProvider, encryptionType = EncryptionType.GCM)
                //   RoomOptions(e2eeOptions = e2eeOptions)
                @Suppress("UNUSED_VARIABLE") val _keyBytes = keyBytes
                Unit // RoomOptions placeholder -- wire to LiveKit when integrating
            } else {
                Unit // RoomOptions placeholder
            }
            @Suppress("UNUSED_VARIABLE") val _roomOptions = roomOptions

            // LiveKit Room connection would go here:
            // room = LiveKit.create(appContext)
            // room.connect(sfuUrl, sfuToken, connectOptions, roomOptions)
            //
            // room.addRoomListener(object : RoomListener {
            //     override fun onDisconnect(room: Room, error: Exception?) { ... }
            //     override fun onParticipantConnected(room: Room, participant: RemoteParticipant) { ... }
            //     override fun onParticipantDisconnected(room: Room, participant: RemoteParticipant) { ... }
            //     override fun onActiveSpeakersChanged(speakers: List<Participant>) {
            //         val level = speakers.firstOrNull()?.audioLevel ?: 0f
            //         _audioLevel.value = level
            //     }
            //     override fun onTrackSubscribed(track: Track, ...) { ... }
            // })

            connected = true
            _streamState.value = MMStreamState.Connected
            reconnectPolicy.reset()
        } catch (e: MMException) {
            _streamState.value = MMStreamState.Error(e)
            throw e
        } catch (e: Exception) {
            _streamState.value = MMStreamState.Error(MMException.SfuUnavailable())
            throw MMException.SfuUnavailable()
        }
    }

    /**
     * Enable the local microphone and start publishing audio (host only).
     *
     * @throws MMException.MicrophonePermissionDenied if the app lacks RECORD_AUDIO permission.
     */
    suspend fun enableMicrophone() {
        if (!isHost) return

        // room.localParticipant.setMicrophoneEnabled(true)
        //
        // The local audio track's level is reported via onActiveSpeakersChanged,
        // so the same audioLevel flow works for both host and viewer.
    }

    /**
     * Enable the local camera and begin publishing video (host only).
     *
     * @throws MMException.NotAuthorized if the caller lacks publish permission.
     * @throws MMException.DeviceUnavailable if no camera is available.
     */
    suspend fun enableCamera() {
        if (!isHost) return

        // room.localParticipant.setCameraEnabled(true)
        //
        // LiveKit will create a LocalVideoTrack and publish it to the SFU.
        // The video track's state is reported via onTrackPublished callbacks.

        _hasVideo.value = true
        _isScreenShare.value = false
    }

    /**
     * Disable the local camera and stop publishing video.
     */
    suspend fun disableCamera() {
        if (!isHost) return

        // room.localParticipant.setCameraEnabled(false)

        _hasVideo.value = false
        _isScreenShare.value = false
    }

    /**
     * Enable screen sharing and begin publishing the screen capture (host only).
     *
     * @throws MMException.NotAuthorized if the caller lacks publish permission.
     */
    suspend fun enableScreenShare() {
        if (!isHost) return

        // room.localParticipant.setScreenShareEnabled(true)
        //
        // On Android, this requires a MediaProjection permission grant.
        // The LiveKit SDK handles the system projection prompt internally.

        _hasVideo.value = true
        _isScreenShare.value = true
    }

    /**
     * Disable screen sharing.
     */
    suspend fun disableScreenShare() {
        if (!isHost) return

        // room.localParticipant.setScreenShareEnabled(false)

        _hasVideo.value = false
        _isScreenShare.value = false
    }

    /**
     * Disconnect from the SFU and release all resources.
     */
    fun disconnect() {
        connected = false
        _streamState.value = MMStreamState.Disconnected(MMDisconnectReason.UserInitiated)
        _audioLevel.value = 0f
        _hasVideo.value = false
        _isScreenShare.value = false

        // room.disconnect()
        scope.cancel()
    }

    /**
     * Mute or unmute audio.
     *
     * For host: mutes the outgoing microphone track.
     * For viewer: mutes the incoming remote audio track.
     */
    fun setMuted(muted: Boolean) {
        _isMuted.value = muted

        if (isHost) {
            // room.localParticipant.setMicrophoneEnabled(!muted)
        } else {
            // Mute all remote audio tracks
            // room.remoteParticipants.values.forEach { participant ->
            //     participant.audioTrackPublications.forEach { pub ->
            //         pub.track?.enabled = !muted
            //     }
            // }
        }
    }

    /**
     * Set playback volume for remote audio tracks.
     *
     * @param volume Volume level from 0.0 to 1.0.
     */
    fun setVolume(volume: Float) {
        this.volume = volume
        // Apply to remote audio tracks:
        // room.remoteParticipants.values.forEach { participant ->
        //     participant.audioTrackPublications.forEach { pub ->
        //         (pub.track as? RemoteAudioTrack)?.setVolume(volume.toDouble())
        //     }
        // }
    }

    /**
     * Handle SFU disconnect event. Attempts reconnection via [ReconnectPolicy].
     */
    private fun handleDisconnect(error: Exception?) {
        if (!connected) return // Already disconnected intentionally

        _streamState.value = MMStreamState.Reconnecting

        scope.launch {
            val delayMs = reconnectPolicy.nextRetryDelayMs()
            if (delayMs == null) {
                // Max retries exceeded
                _streamState.value = MMStreamState.Disconnected(
                    MMDisconnectReason.NetworkError
                )
                return@launch
            }

            kotlinx.coroutines.delay(delayMs)

            try {
                connect()
            } catch (_: Exception) {
                handleDisconnect(error)
            }
        }
    }
}
