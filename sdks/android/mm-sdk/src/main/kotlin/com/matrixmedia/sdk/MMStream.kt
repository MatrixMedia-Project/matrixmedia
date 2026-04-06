package com.matrixmedia.sdk

import com.matrixmedia.sdk.internal.livekit.LiveKitBridge
import com.matrixmedia.sdk.internal.networking.MMApiClient
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Represents an active MM stream session.
 *
 * Provides real-time state observation via [StateFlow] properties and
 * control methods for leaving, stopping, muting, and adjusting volume.
 *
 * Obtained from [MMClient.joinStream] (viewer) or [MMClient.startStream] (host).
 *
 * ## Lifecycle
 *
 * - Viewer: call [leave] to disconnect gracefully.
 * - Host: call [stop] to end the stream for all participants.
 *
 * The [state] flow emits [MMStreamState] values reflecting the connection
 * lifecycle, including automatic reconnection attempts on network changes.
 */
class MMStream internal constructor(
    private val bridge: LiveKitBridge,
    private val apiClient: MMApiClient,
    private val streamId: String,
    e2ee: MME2eeInfo? = null
) {
    private val _e2eeEnabled = MutableStateFlow(e2ee?.enabled == true)
    private val _e2eeKeyId = MutableStateFlow<String?>(e2ee?.takeIf { it.enabled }?.keyId)

    /** Whether end-to-end encryption is active on this stream. */
    val e2eeEnabled: StateFlow<Boolean> = _e2eeEnabled.asStateFlow()

    /** The current E2EE key identifier, if E2EE is enabled. */
    val e2eeKeyId: StateFlow<String?> = _e2eeKeyId.asStateFlow()

    /** Current stream connection state. */
    val state: StateFlow<MMStreamState>
        get() = bridge.streamState

    /** The current host participant, or null if unknown. */
    val host: StateFlow<MMParticipant?>
        get() = bridge.host

    /** Current number of viewers in the stream. */
    val viewerCount: StateFlow<Int>
        get() = bridge.viewerCount

    /**
     * Current audio level (0.0 = silence, 1.0 = maximum).
     *
     * Updated from LiveKit's built-in audio level callbacks. Useful for
     * driving [com.matrixmedia.sdk.ui.MMAudioRenderer] visualizations.
     */
    val audioLevel: StateFlow<Float>
        get() = bridge.audioLevel

    /** Whether the local audio output is muted. */
    val isMuted: StateFlow<Boolean>
        get() = bridge.isMuted

    /** Whether the stream currently has an active video track. */
    val hasVideo: StateFlow<Boolean>
        get() = bridge.hasVideo

    /** Whether the active video track is a screen share (vs camera). */
    val isScreenShare: StateFlow<Boolean>
        get() = bridge.isScreenShare

    /**
     * Leave the stream as a viewer.
     *
     * Notifies the MM server and disconnects from the SFU. After calling
     * this method, the [MMStream] instance is no longer usable.
     */
    suspend fun leave() {
        try {
            apiClient.leaveStream(streamId)
        } finally {
            bridge.disconnect()
        }
    }

    /**
     * Stop the stream (host only).
     *
     * Ends the stream for all participants. After calling this method,
     * the [MMStream] instance is no longer usable.
     *
     * @throws MMException.NotAuthorized if the caller is not the host.
     */
    suspend fun stop() {
        try {
            apiClient.endStream(streamId)
        } finally {
            bridge.disconnect()
        }
    }

    /**
     * Mute or unmute the audio output.
     *
     * For viewers, this mutes the incoming host audio. For hosts, this
     * mutes the outgoing microphone (other participants hear silence).
     *
     * @param muted `true` to mute, `false` to unmute.
     */
    fun setMuted(muted: Boolean) {
        bridge.setMuted(muted)
    }

    /**
     * Set the playback volume.
     *
     * @param volume Volume level from 0.0 (silent) to 1.0 (full volume).
     */
    fun setVolume(volume: Float) {
        require(volume in 0.0f..1.0f) { "Volume must be between 0.0 and 1.0, got $volume" }
        bridge.setVolume(volume)
    }

    /**
     * Enable the local camera and begin publishing video (host only).
     *
     * @throws MMException.NotAuthorized if the caller lacks publish permission.
     * @throws MMException.DeviceUnavailable if no camera is available.
     */
    suspend fun enableCamera() {
        bridge.enableCamera()
    }

    /**
     * Disable the local camera and stop publishing video.
     */
    suspend fun disableCamera() {
        bridge.disableCamera()
    }

    /**
     * Enable screen sharing and begin publishing the screen capture (host only).
     *
     * @throws MMException.NotAuthorized if the caller lacks publish permission.
     */
    suspend fun enableScreenShare() {
        bridge.enableScreenShare()
    }

    /**
     * Disable screen sharing.
     */
    suspend fun disableScreenShare() {
        bridge.disableScreenShare()
    }
}
