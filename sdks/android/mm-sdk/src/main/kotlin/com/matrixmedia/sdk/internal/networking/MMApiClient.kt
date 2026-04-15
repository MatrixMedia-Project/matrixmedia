package com.matrixmedia.sdk.internal.networking

import com.matrixmedia.sdk.MMException
import com.matrixmedia.sdk.MMCreateResult
import com.matrixmedia.sdk.MME2eeInfo
import com.matrixmedia.sdk.MMJoinResult
import com.matrixmedia.sdk.MMMediaType
import com.matrixmedia.sdk.MMRecording
import com.matrixmedia.sdk.MMRecordingStatus
import com.matrixmedia.sdk.MMStreamConfig
import com.matrixmedia.sdk.MMStreamInfo
import com.matrixmedia.sdk.internal.auth.TokenManager
import com.squareup.moshi.Json
import com.squareup.moshi.Moshi
import com.squareup.moshi.Types
import com.squareup.moshi.kotlin.reflect.KotlinJsonAdapterFactory
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import java.io.IOException
import java.util.concurrent.TimeUnit

/**
 * HTTP client for the MatrixMedia REST API.
 *
 * All requests use [tokenManager] to obtain a valid MM session JWT.
 * Responses are parsed with Moshi, using [MMDateAdapter] for ISO8601
 * date fields with fractional seconds (PTT bug #10 fix).
 *
 * Base URL follows the `/_mm/client/v1/` prefix convention.
 */
internal class MMApiClient(
    private val serverUrl: String,
    private val tokenManager: TokenManager
) {
    private val baseUrl = serverUrl.trimEnd('/') + "/_mm/client/v1"
    private val jsonMediaType = "application/json; charset=utf-8".toMediaType()

    private val client = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(30, TimeUnit.SECONDS)
        .writeTimeout(30, TimeUnit.SECONDS)
        .build()

    private val moshi: Moshi = Moshi.Builder()
        .add(MMDateAdapter())
        .addLast(KotlinJsonAdapterFactory())
        .build()

    /**
     * GET /rooms/{roomId}/streams -- list streams in a room.
     */
    suspend fun getRoomStreams(roomId: String): List<MMStreamInfo> {
        val responseBody = authenticatedGet("$baseUrl/rooms/${encode(roomId)}/streams")
        val type = Types.newParameterizedType(List::class.java, MMStreamInfo::class.java)
        val adapter = moshi.adapter<List<MMStreamInfo>>(type)
        return adapter.fromJson(responseBody) ?: emptyList()
    }

    /**
     * GET /streams/{streamId} -- get stream details.
     */
    suspend fun getStream(streamId: String): MMStreamInfo {
        val responseBody = authenticatedGet("$baseUrl/streams/${encode(streamId)}")
        val adapter = moshi.adapter(MMStreamInfo::class.java)
        return adapter.fromJson(responseBody)
            ?: throw MMException.Server("MM_PARSE_ERROR", "Failed to parse stream response")
    }

    /**
     * POST /streams -- create a new stream.
     */
    suspend fun createStream(roomId: String, config: MMStreamConfig, e2ee: Boolean = false): MMCreateResult {
        val requestE2ee = e2ee || config.e2ee
        val body = buildString {
            append("{")
            append("\"room_id\":\"${escapeJson(roomId)}\"")
            append(",\"media_type\":\"${config.mediaType.name.lowercase()}\"")
            append(",\"e2ee\":$requestE2ee")
            config.title?.let { append(",\"title\":\"${escapeJson(it)}\"") }
            append("}")
        }
        val responseBody = authenticatedPost("$baseUrl/streams", body)
        val adapter = moshi.adapter(MMCreateResultDto::class.java)
        val dto = adapter.fromJson(responseBody)
            ?: throw MMException.Server("MM_PARSE_ERROR", "Failed to parse create response")
        return dto.toInternal()
    }

    /**
     * POST /streams/{streamId}/join -- join a stream as viewer.
     */
    suspend fun joinStream(streamId: String): MMJoinResult {
        val responseBody = authenticatedPost("$baseUrl/streams/${encode(streamId)}/join", "{}")
        val adapter = moshi.adapter(MMJoinResultDto::class.java)
        val dto = adapter.fromJson(responseBody)
            ?: throw MMException.Server("MM_PARSE_ERROR", "Failed to parse join response")
        return dto.toInternal()
    }

    /**
     * POST /streams/{streamId}/leave -- leave a stream.
     */
    suspend fun leaveStream(streamId: String) {
        authenticatedPost("$baseUrl/streams/${encode(streamId)}/leave", "{}")
    }

    /**
     * POST /streams/{streamId}/end -- end a stream (host only).
     */
    suspend fun endStream(streamId: String) {
        authenticatedPost("$baseUrl/streams/${encode(streamId)}/end", "{}")
    }

    // -- Recordings (VoD) --

    /**
     * GET /rooms/{roomId}/recordings -- list ready recordings in a room.
     */
    suspend fun getRoomRecordings(roomId: String, limit: Int): List<MMRecording> {
        val url = "$baseUrl/rooms/${encode(roomId)}/recordings?limit=$limit&status=ready"
        val responseBody = authenticatedGet(url)
        val adapter = moshi.adapter(MMRecordingsEnvelope::class.java)
        val envelope = adapter.fromJson(responseBody)
            ?: throw MMException.Server("MM_PARSE_ERROR", "Failed to parse recordings response")
        return envelope.recordings.map { it.toPublic() }
    }

    /**
     * GET /recordings/{id} -- get recording details.
     */
    suspend fun getRecording(recordingId: String): MMRecording {
        val responseBody = authenticatedGet("$baseUrl/recordings/${encode(recordingId)}")
        val adapter = moshi.adapter(MMRecordingDto::class.java)
        val dto = adapter.fromJson(responseBody)
            ?: throw MMException.Server("MM_PARSE_ERROR", "Failed to parse recording response")
        return dto.toPublic()
    }

    /**
     * DELETE /recordings/{id} -- delete a recording (host only).
     */
    suspend fun deleteRecording(recordingId: String) {
        authenticatedDelete("$baseUrl/recordings/${encode(recordingId)}")
    }

    // -- Token exchange (unauthenticated) --

    /**
     * POST /auth/token -- exchange Matrix OpenID for MM JWT.
     * This does NOT use the token manager since it is the authentication step itself.
     */
    suspend fun exchangeToken(body: String): String {
        return post("$baseUrl/auth/token", body, authHeader = null)
    }

    /**
     * POST /auth/refresh -- refresh an MM session token.
     */
    suspend fun refreshToken(refreshToken: String): String {
        val body = "{\"refresh_token\":\"${escapeJson(refreshToken)}\"}"
        return post("$baseUrl/auth/refresh", body, authHeader = null)
    }

    // -- Private helpers --

    private suspend fun authenticatedGet(url: String): String {
        val token = tokenManager.getToken()
        return get(url, "Bearer $token")
    }

    private suspend fun authenticatedPost(url: String, body: String): String {
        val token = tokenManager.getToken()
        return post(url, body, "Bearer $token")
    }

    private suspend fun authenticatedDelete(url: String): String {
        val token = tokenManager.getToken()
        return delete(url, "Bearer $token")
    }

    private suspend fun get(url: String, authHeader: String?): String = withContext(Dispatchers.IO) {
        val requestBuilder = Request.Builder().url(url).get()
        authHeader?.let { requestBuilder.header("Authorization", it) }
        executeRequest(requestBuilder.build())
    }

    private suspend fun post(url: String, body: String, authHeader: String?): String = withContext(Dispatchers.IO) {
        val requestBody = body.toRequestBody(jsonMediaType)
        val requestBuilder = Request.Builder().url(url).post(requestBody)
        authHeader?.let { requestBuilder.header("Authorization", it) }
        executeRequest(requestBuilder.build())
    }

    private suspend fun delete(url: String, authHeader: String?): String = withContext(Dispatchers.IO) {
        val requestBuilder = Request.Builder().url(url).delete()
        authHeader?.let { requestBuilder.header("Authorization", it) }
        executeRequest(requestBuilder.build())
    }

    private fun executeRequest(request: Request): String {
        try {
            val response = client.newCall(request).execute()
            val responseBody = response.body?.string() ?: ""

            if (!response.isSuccessful) {
                handleErrorResponse(response.code, responseBody)
            }

            return responseBody
        } catch (e: MMException) {
            throw e
        } catch (e: IOException) {
            throw MMException.Network(e)
        }
    }

    private fun handleErrorResponse(httpCode: Int, body: String) {
        // Try to parse the MM error envelope: { "error": "MM_*", "message": "...", "retry_after_ms": null }
        try {
            val errorAdapter = moshi.adapter(MMErrorEnvelope::class.java)
            val envelope = errorAdapter.fromJson(body)
            if (envelope != null) {
                throw mapServerError(httpCode, envelope)
            }
        } catch (e: MMException) {
            throw e
        } catch (_: Exception) {
            // Could not parse error envelope; fall through
        }

        // Generic HTTP error mapping
        when (httpCode) {
            401 -> throw MMException.NotAuthenticated()
            403 -> throw MMException.NotAuthorized()
            404 -> throw MMException.StreamNotFound()
            429 -> throw MMException.RateLimited(retryAfterMs = null)
            503 -> throw MMException.SfuUnavailable()
            else -> throw MMException.Server("HTTP_$httpCode", "HTTP $httpCode: $body")
        }
    }

    private fun mapServerError(httpCode: Int, envelope: MMErrorEnvelope): MMException {
        return when (envelope.error) {
            "MM_NOT_FOUND", "MM_STREAM_NOT_FOUND" -> MMException.StreamNotFound()
            "MM_ROOM_FULL" -> MMException.StreamFull()
            "MM_STREAM_ENDED" -> MMException.StreamEnded()
            "MM_FORBIDDEN" -> MMException.NotAuthorized()
            "MM_RATE_LIMITED" -> MMException.RateLimited(retryAfterMs = envelope.retryAfterMs)
            "MM_SFU_UNAVAILABLE" -> MMException.SfuUnavailable()
            else -> MMException.Server(envelope.error, envelope.message)
        }
    }

    private fun encode(value: String): String =
        java.net.URLEncoder.encode(value, "UTF-8")

    private fun escapeJson(value: String): String =
        value.replace("\\", "\\\\").replace("\"", "\\\"")
}

/**
 * MM server error response envelope.
 */
internal data class MMErrorEnvelope(
    val error: String,
    val message: String,
    val retryAfterMs: Long? = null
)

/**
 * Wire-format DTO for MME2eeInfo with snake_case JSON field mapping.
 */
internal data class MME2eeInfoDto(
    val enabled: Boolean,
    val algorithm: String,
    @Json(name = "key_id") val keyId: String,
    @Json(name = "key_generation") val keyGeneration: Int,
    @Json(name = "key_b64") val keyB64: String
) {
    fun toPublic(): MME2eeInfo = MME2eeInfo(
        enabled = enabled,
        algorithm = algorithm,
        keyId = keyId,
        keyGeneration = keyGeneration,
        keyB64 = keyB64
    )
}

/**
 * Wire-format DTO for the POST /streams (create) response.
 */
internal data class MMCreateResultDto(
    @Json(name = "stream_id") val streamId: String,
    @Json(name = "sfu_url") val sfuUrl: String,
    @Json(name = "sfu_token") val sfuToken: String,
    val e2ee: MME2eeInfoDto? = null
) {
    fun toInternal(): MMCreateResult = MMCreateResult(
        streamId = streamId,
        sfuUrl = sfuUrl,
        sfuToken = sfuToken,
        e2ee = e2ee?.toPublic()
    )
}

/**
 * Wire-format DTO for the POST /streams/{id}/join response.
 */
internal data class MMJoinResultDto(
    @Json(name = "stream_id") val streamId: String,
    @Json(name = "sfu_url") val sfuUrl: String,
    @Json(name = "sfu_token") val sfuToken: String,
    @Json(name = "participant_id") val participantId: String,
    val e2ee: MME2eeInfoDto? = null
) {
    fun toInternal(): MMJoinResult = MMJoinResult(
        streamId = streamId,
        sfuUrl = sfuUrl,
        sfuToken = sfuToken,
        participantId = participantId,
        e2ee = e2ee?.toPublic()
    )
}

/**
 * List envelope for GET /rooms/{roomId}/recordings.
 */
internal data class MMRecordingsEnvelope(
    val recordings: List<MMRecordingDto>
)

/**
 * Wire-format DTO for MMRecording. Maps snake_case JSON fields to the
 * camelCase public [MMRecording] via [toPublic].
 */
    // -----------------------------------------------------------------------
    // Participants & Key Rotation
    // -----------------------------------------------------------------------

    /** List participants in a stream. */
    suspend fun listParticipants(streamId: String): List<Map<String, Any?>> {
        val json = get("$baseUrl/streams/$streamId/participants")
        val map = parseJsonMap(json)
        @Suppress("UNCHECKED_CAST")
        return (map["participants"] as? List<Map<String, Any?>>) ?: emptyList()
    }

    /** Rotate E2EE key for a stream (host only). */
    suspend fun rotateKey(streamId: String) {
        post("$baseUrl/streams/$streamId/rotate-key", "{}")
    }

    // -----------------------------------------------------------------------
    // Donations
    // -----------------------------------------------------------------------

    /** Send a donation to a stream. */
    suspend fun donate(streamId: String, amountCents: Int, message: String? = null): Map<String, Any?> {
        val body = buildString {
            append("""{"stream_id":"$streamId","amount_cents":$amountCents""")
            if (!message.isNullOrEmpty()) append(""","message":"$message"""")
            append("}")
        }
        val json = post("$baseUrl/donations", body)
        return parseJsonMap(json)
    }

    /** Get donation feed for a stream. */
    suspend fun getDonationFeed(streamId: String): List<Map<String, Any?>> {
        val json = get("$baseUrl/streams/$streamId/donations")
        val map = parseJsonMap(json)
        @Suppress("UNCHECKED_CAST")
        return (map["donations"] as? List<Map<String, Any?>>) ?: emptyList()
    }

    // -----------------------------------------------------------------------
    // Creator & Tiers
    // -----------------------------------------------------------------------

    /** Onboard as a creator. */
    suspend fun onboardCreator(displayName: String): Map<String, Any?> {
        val json = post("$baseUrl/creator/onboard", """{"display_name":"$displayName"}""")
        return parseJsonMap(json)
    }

    /** Get creator profile (returns null if not onboarded). */
    suspend fun getCreatorProfile(): Map<String, Any?>? {
        return try {
            val json = get("$baseUrl/creator/profile")
            parseJsonMap(json)
        } catch (e: MMException) {
            if (e.code == "MM_NOT_FOUND" || e.code == "HTTP_404" || e.code == "HTTP_412") null else throw e
        }
    }

    /** Create a subscription tier. */
    suspend fun createTier(name: String, tierLevel: Int, priceCents: Int, perks: List<String> = emptyList()): Map<String, Any?> {
        val perksJson = perks.joinToString(",") { "\"$it\"" }
        val body = """{"name":"$name","tier_level":$tierLevel,"price_cents":$priceCents,"perks":[$perksJson]}"""
        val json = post("$baseUrl/creator/tiers", body)
        return parseJsonMap(json)
    }

    /** List tiers for a creator. */
    suspend fun listCreatorTiers(creatorUserId: String): List<Map<String, Any?>> {
        val encoded = java.net.URLEncoder.encode(creatorUserId, "UTF-8")
        val json = get("$baseUrl/creators/$encoded/tiers")
        val map = parseJsonMap(json)
        @Suppress("UNCHECKED_CAST")
        return (map["tiers"] as? List<Map<String, Any?>>) ?: emptyList()
    }

    // -----------------------------------------------------------------------
    // Subscriptions
    // -----------------------------------------------------------------------

    /** Subscribe to a tier. */
    suspend fun subscribe(tierId: String): Map<String, Any?> {
        val json = post("$baseUrl/subscriptions", """{"tier_id":"$tierId"}""")
        return parseJsonMap(json)
    }

    /** Check entitlement for a creator. */
    suspend fun checkEntitlement(creatorUserId: String): Map<String, Any?> {
        val encoded = java.net.URLEncoder.encode(creatorUserId, "UTF-8")
        val json = get("$baseUrl/subscriptions/check?creator_user_id=$encoded")
        return parseJsonMap(json)
    }

    // -----------------------------------------------------------------------
    // Advertising
    // -----------------------------------------------------------------------

    /** Get an ad decision for a stream (pre-roll, mid-roll, etc.). */
    suspend fun getAdDecision(streamId: String, slot: String = "pre_roll"): Map<String, Any?> {
        val json = get("$baseUrl/streams/$streamId/ad-decision?slot=$slot")
        return parseJsonMap(json)
    }

    /** Submit ad completion proof (HMAC challenge-response). */
    suspend fun submitAdComplete(
        streamId: String,
        impressionToken: String,
        challengeResponse: String,
        timestamp: Long
    ) {
        val body = buildString {
            append("{")
            append("\"impression_token\":\"${escapeJson(impressionToken)}\"")
            append(",\"challenge_response\":\"${escapeJson(challengeResponse)}\"")
            append(",\"timestamp\":$timestamp")
            append("}")
        }
        post("$baseUrl/streams/$streamId/ad-complete", body)
    }

    /** Report an ad event (quartile progress, click, skip, error). */
    suspend fun reportAdEvent(impressionToken: String, event: String, positionSecs: Int? = null) {
        val body = buildString {
            append("{")
            append("\"impression_token\":\"${escapeJson(impressionToken)}\"")
            append(",\"event\":\"${escapeJson(event)}\"")
            if (positionSecs != null) append(",\"position_secs\":$positionSecs")
            append("}")
        }
        post("$baseUrl/ads/events", body)
    }

    /** Check if the viewer is currently in an ad break. */
    suspend fun getAdStatus(streamId: String): Boolean {
        val json = get("$baseUrl/streams/$streamId/ad-status")
        val map = parseJsonMap(json)
        return map["in_ad_break"] == true
    }

    /** Upload an ad creative. */
    suspend fun uploadAd(title: String, placement: String, durationSecs: Int = 15): Map<String, Any?> {
        val body = buildString {
            append("{")
            append("\"title\":\"${escapeJson(title)}\"")
            append(",\"placement\":\"${escapeJson(placement)}\"")
            append(",\"duration_secs\":$durationSecs")
            append("}")
        }
        val json = post("$baseUrl/ads", body)
        return parseJsonMap(json)
    }

    /** List my ads. */
    suspend fun listMyAds(): List<Map<String, Any?>> {
        val json = get("$baseUrl/ads")
        val map = parseJsonMap(json)
        @Suppress("UNCHECKED_CAST")
        return (map["ads"] as? List<Map<String, Any?>>) ?: emptyList()
    }

    /** Delete an ad. */
    suspend fun deleteAd(adId: String) {
        delete("$baseUrl/ads/$adId")
    }

    /** Get ad statistics. */
    suspend fun getAdStats(adId: String): Map<String, Any?> {
        val json = get("$baseUrl/ads/$adId/stats")
        return parseJsonMap(json)
    }

    /** Trigger mid-roll ad break (host only). */
    suspend fun triggerAdBreak(streamId: String) {
        post("$baseUrl/streams/$streamId/ad-break", "{}")
    }

    // -----------------------------------------------------------------------
    // JSON helpers
    // -----------------------------------------------------------------------

    @Suppress("UNCHECKED_CAST")
    private fun parseJsonMap(json: String): Map<String, Any?> {
        val type = Types.newParameterizedType(Map::class.java, String::class.java, Any::class.java)
        val adapter = moshi.adapter<Map<String, Any?>>(type)
        return adapter.fromJson(json) ?: emptyMap()
    }
}

internal data class MMRecordingDto(
    val id: String,
    @Json(name = "stream_id") val streamId: String,
    @Json(name = "host_user_id") val hostUserId: String,
    @Json(name = "media_type") val mediaType: String,
    val title: String?,
    val status: String,
    @Json(name = "duration_ms") val durationMs: Long?,
    @Json(name = "size_bytes") val sizeBytes: Long?,
    @Json(name = "playback_url") val playbackUrl: String?,
    @Json(name = "mxc_url") val mxcUrl: String?,
    @Json(name = "created_at") val createdAt: String
) {
    fun toPublic(): MMRecording {
        val parsedMediaType = when (mediaType.lowercase()) {
            "audio" -> MMMediaType.Audio
            "video" -> MMMediaType.Video
            "screen_share", "screenshare" -> MMMediaType.ScreenShare
            else -> MMMediaType.Audio
        }
        val parsedStatus = when (status.lowercase()) {
            "recording" -> MMRecordingStatus.Recording
            "processing" -> MMRecordingStatus.Processing
            "ready" -> MMRecordingStatus.Ready
            "failed" -> MMRecordingStatus.Failed
            "deleted" -> MMRecordingStatus.Deleted
            else -> MMRecordingStatus.Processing
        }
        return MMRecording(
            id = id,
            streamId = streamId,
            hostUserId = hostUserId,
            mediaType = parsedMediaType,
            title = title,
            status = parsedStatus,
            durationMs = durationMs,
            sizeBytes = sizeBytes,
            playbackUrl = playbackUrl,
            mxcUrl = mxcUrl,
            createdAt = createdAt
        )
    }
}
