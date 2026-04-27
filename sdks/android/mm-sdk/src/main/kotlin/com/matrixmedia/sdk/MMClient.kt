package com.matrixmedia.sdk

import com.matrixmedia.sdk.internal.auth.TokenManager
import com.matrixmedia.sdk.internal.livekit.LiveKitBridge
import com.matrixmedia.sdk.internal.networking.MMApiClient
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Entry point for the MatrixMedia Android SDK.
 *
 * Manages authentication, stream discovery, and stream lifecycle.
 *
 * ## Usage
 *
 * ```kotlin
 * val client = MMClient(
 *     serverUrl = "https://mm.example.com",
 *     tokenProvider = { getMatrixOpenIdToken() }
 * )
 *
 * val user = client.authenticate(openIdToken)
 * val stream = client.joinStream(roomId = "!abc:example.org")
 * ```
 *
 * ## Token Provider
 *
 * The [tokenProvider] is called fresh for every API request that requires
 * authentication. This prevents the stale-token bug discovered in PTT v1
 * where cached tokens expired mid-session. The provider should return
 * the current valid Matrix OpenID token. The SDK handles MM session token
 * management (refresh at 80% TTL) internally.
 *
 * @param serverUrl Base URL of the MM server (e.g., `https://mm.example.com`).
 * @param tokenProvider Suspend function that returns a fresh [MMAuthToken].
 *   Called on every API request. Never caches tokens internally beyond the
 *   managed MM session token with auto-refresh.
 */
class MMClient(
    private val serverUrl: String,
    private val tokenProvider: suspend () -> MMAuthToken
) {
    private val _connectionState = MutableStateFlow(MMConnectionState.Disconnected)

    /** Current connection state of this client. */
    val connectionState: StateFlow<MMConnectionState> = _connectionState.asStateFlow()

    internal val tokenManager = TokenManager(serverUrl, tokenProvider)
    internal val apiClient = MMApiClient(serverUrl, tokenManager)

    init {
        validateServerUrl(serverUrl)
        // Wire the circular dependency: TokenManager needs MMApiClient for /auth/token
        // and /auth/refresh calls, while MMApiClient needs TokenManager for Bearer tokens.
        tokenManager.apiClient = apiClient
    }

    /**
     * Authenticate with the MM server using a Matrix OpenID token.
     *
     * Exchanges the OpenID token for an MM session JWT. The session token
     * is managed internally with auto-refresh at 80% TTL (15-minute tokens,
     * refreshed at 12 minutes).
     *
     * @param openIdToken Matrix OpenID token obtained from the homeserver.
     * @return The authenticated [MMUser].
     * @throws MMException.Network on connectivity failure.
     * @throws MMException.NotAuthorized if the homeserver rejects the token.
     */
    suspend fun authenticate(openIdToken: MMOpenIDToken): MMUser {
        _connectionState.value = MMConnectionState.Connecting
        return try {
            val user = tokenManager.authenticate(openIdToken)
            _connectionState.value = MMConnectionState.Connected
            user
        } catch (e: Exception) {
            _connectionState.value = MMConnectionState.Disconnected
            throw e
        }
    }

    /**
     * Join an active stream in the given Matrix room as a viewer.
     *
     * Discovers the active stream in the room, obtains an SFU token,
     * and connects to the LiveKit SFU as a subscriber.
     *
     * @param roomId Matrix room ID (e.g., `!abc:example.org`).
     * @return An [MMStream] representing the joined stream.
     * @throws MMException.StreamNotFound if no active stream exists in the room.
     * @throws MMException.StreamFull if the stream is at capacity.
     * @throws MMException.NotAuthenticated if [authenticate] was not called.
     * @throws MMException.SfuUnavailable if the SFU cannot be reached.
     */
    suspend fun joinStream(roomId: String): MMStream {
        tokenManager.ensureAuthenticated()

        val streams = apiClient.getRoomStreams(roomId)
        val active = streams.firstOrNull { it.status == "active" }
            ?: throw MMException.StreamNotFound()

        val joinResult = apiClient.joinStream(active.streamId)
        // When mm-switch URL is present, the viewer can connect directly to
        // mm-switch via WebRTC for lower latency and server-side ad insertion.
        // NOTE: mm-switch direct WebRTC is not yet implemented in the Android SDK.
        // Falls back to LiveKit SFU for now. See: services/mm-switch/
        val bridge = LiveKitBridge(
            sfuUrl = joinResult.sfuUrl,
            sfuToken = joinResult.sfuToken,
            isHost = false,
            e2ee = joinResult.e2ee,
            switchUrl = joinResult.switchUrl,
            switchSourceId = joinResult.switchSourceId,
            switchViewerId = joinResult.switchViewerId
        )
        bridge.connect()

        return MMStream(
            bridge = bridge,
            apiClient = apiClient,
            streamId = active.streamId,
            e2ee = joinResult.e2ee
        )
    }

    /**
     * Start a new stream in the given Matrix room as the host.
     *
     * Creates the stream on the MM server, obtains an SFU token with
     * publish permissions, connects to the LiveKit SFU, and enables
     * the microphone for audio streaming.
     *
     * The caller must ensure `RECORD_AUDIO` permission is granted before
     * calling this method. On Android 13+ (API 33), `POST_NOTIFICATIONS`
     * must also be granted for the foreground service notification.
     *
     * @param roomId Matrix room ID (e.g., `!abc:example.org`).
     * @param config Stream configuration. Defaults to audio-only.
     * @return An [MMStream] representing the started stream.
     * @throws MMException.MicrophonePermissionDenied if audio permission is missing.
     * @throws MMException.NotAuthenticated if [authenticate] was not called.
     * @throws MMException.SfuUnavailable if the SFU cannot be reached.
     */
    suspend fun startStream(
        roomId: String,
        config: MMStreamConfig = MMStreamConfig()
    ): MMStream {
        tokenManager.ensureAuthenticated()

        val result = apiClient.createStream(roomId, config, e2ee = config.e2ee)
        // When mm-switch URL is present, the host can publish directly to
        // mm-switch via WebRTC (bypasses LiveKit for the streaming path).
        // NOTE: mm-switch direct publish is not yet implemented in the Android SDK.
        // Falls back to LiveKit SFU for now. See: services/mm-switch/
        val bridge = LiveKitBridge(
            sfuUrl = result.sfuUrl,
            sfuToken = result.sfuToken,
            isHost = true,
            e2ee = result.e2ee,
            switchUrl = result.switchUrl,
            switchSourceId = result.switchSourceId
        )
        bridge.connect()
        bridge.enableMicrophone()

        return MMStream(
            bridge = bridge,
            apiClient = apiClient,
            streamId = result.streamId,
            e2ee = result.e2ee
        )
    }

    /**
     * List ready recordings in a Matrix room.
     *
     * Returns recordings with status [MMRecordingStatus.Ready], ordered newest-first.
     *
     * @param roomId Matrix room ID (e.g., `!abc:example.org`).
     * @param limit Maximum number of recordings to return. Defaults to 20.
     * @return List of [MMRecording] available for playback.
     * @throws MMException.NotAuthenticated if [authenticate] was not called.
     */
    suspend fun listRoomRecordings(roomId: String, limit: Int = 20): List<MMRecording> {
        tokenManager.ensureAuthenticated()
        return apiClient.getRoomRecordings(roomId, limit)
    }

    /**
     * Fetch details for a specific recording.
     *
     * @param recordingId The recording identifier.
     * @return The [MMRecording].
     * @throws MMException.StreamNotFound if the recording does not exist.
     * @throws MMException.NotAuthenticated if [authenticate] was not called.
     */
    suspend fun getRecording(recordingId: String): MMRecording {
        tokenManager.ensureAuthenticated()
        return apiClient.getRecording(recordingId)
    }

    /**
     * Delete a recording. Only the host of the original stream may delete its recording.
     *
     * @param recordingId The recording identifier.
     * @throws MMException.NotAuthorized if the caller did not host the original stream.
     * @throws MMException.StreamNotFound if the recording does not exist.
     * @throws MMException.NotAuthenticated if [authenticate] was not called.
     */
    suspend fun deleteRecording(recordingId: String) {
        tokenManager.ensureAuthenticated()
        apiClient.deleteRecording(recordingId)
    }

    // -- Creator profile (M1 Lightning) --

    /** Get the authenticated user's creator profile, or null if not onboarded. */
    suspend fun getCreatorProfile(): Map<String, Any?>? {
        tokenManager.ensureAuthenticated()
        return apiClient.getCreatorProfile()
    }

    /**
     * Publish (or clear with `null` / empty string) the authenticated creator's
     * LUD-16 Lightning Address. The server validates the format; donations route
     * directly via LNURL-pay when an address is set.
     */
    suspend fun updateLightningAddress(address: String?): Map<String, Any?> {
        tokenManager.ensureAuthenticated()
        return apiClient.updateCreatorProfile(address)
    }

    // -- Donations (M1 Lightning) --

    /**
     * Send a Lightning tip in sats. Returns the raw response. For the LNURL-pay
     * path, look in `result["invoice"]["bolt11"]` for the BOLT11 to display.
     */
    suspend fun sendLightningTip(streamId: String, amountSats: Int, message: String? = null): Map<String, Any?> {
        tokenManager.ensureAuthenticated()
        return apiClient.donateLightning(streamId, amountSats, message)
    }

    /**
     * Disconnect from the MM server and release all resources.
     *
     * After calling this, the client is no longer usable. Create a new
     * [MMClient] instance to reconnect.
     */
    suspend fun disconnect() {
        _connectionState.value = MMConnectionState.Disconnected
        tokenManager.clearSession()
    }

    private fun validateServerUrl(url: String) {
        if (url.isBlank()) {
            throw MMException.InvalidServerUrl(url)
        }
        val lower = url.lowercase()
        if (!lower.startsWith("http://") && !lower.startsWith("https://")) {
            throw MMException.InvalidServerUrl(url)
        }
    }
}
