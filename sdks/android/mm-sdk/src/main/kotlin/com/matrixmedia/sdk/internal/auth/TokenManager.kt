package com.matrixmedia.sdk.internal.auth

import com.matrixmedia.sdk.MMAuthToken
import com.matrixmedia.sdk.MMException
import com.matrixmedia.sdk.MMOpenIDToken
import com.matrixmedia.sdk.MMRefreshResponse
import com.matrixmedia.sdk.MMTokenResponse
import com.matrixmedia.sdk.MMUser
import com.matrixmedia.sdk.internal.networking.MMApiClient
import com.squareup.moshi.Moshi
import com.squareup.moshi.kotlin.reflect.KotlinJsonAdapterFactory
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/**
 * Manages MM session token lifecycle: authentication, storage, and automatic refresh.
 *
 * ## Token Strategy (PTT lesson: never cache stale tokens)
 *
 * The [tokenProvider] is the external token source (Matrix OpenID) and is called
 * during initial authentication. The MM session token (JWT with 15-minute TTL)
 * is managed internally with auto-refresh at 80% TTL (i.e., refresh at 12 minutes).
 *
 * The refresh token (24-hour TTL) is used to obtain a new session token without
 * requiring a fresh Matrix OpenID exchange.
 *
 * ## Concurrency
 *
 * A [Mutex] guards token refresh to prevent thundering-herd on concurrent API calls.
 * Multiple callers to [getToken] will all await the same refresh operation.
 *
 * @param serverUrl MM server base URL.
 * @param tokenProvider External suspend function providing Matrix OpenID tokens.
 */
internal class TokenManager(
    private val serverUrl: String,
    private val tokenProvider: suspend () -> MMAuthToken
) {
    private val mutex = Mutex()

    private var sessionToken: String? = null
    private var refreshToken: String? = null
    private var expiresAtMs: Long = 0L
    private var authenticatedUser: MMUser? = null

    // Lazy initialization -- set by MMClient after construction
    internal var apiClient: MMApiClient? = null

    private val moshi: Moshi = Moshi.Builder()
        .addLast(KotlinJsonAdapterFactory())
        .build()

    /**
     * Authenticate with the MM server by exchanging a Matrix OpenID token.
     *
     * POST /_mm/client/v1/auth/token with the OpenID token. The server validates
     * against the homeserver and returns an MM session JWT + refresh token.
     *
     * @param openIdToken Matrix OpenID token from the homeserver.
     * @return The authenticated [MMUser].
     */
    suspend fun authenticate(openIdToken: MMOpenIDToken): MMUser = mutex.withLock {
        val client = requireApiClient()

        val body = buildString {
            append("{")
            append("\"access_token\":\"${openIdToken.accessToken}\"")
            append(",\"token_type\":\"${openIdToken.tokenType}\"")
            append(",\"matrix_server_name\":\"${openIdToken.matrixServerName}\"")
            append(",\"expires_in\":${openIdToken.expiresIn}")
            append("}")
        }

        val responseBody = client.exchangeToken(body)
        val adapter = moshi.adapter(MMTokenResponse::class.java)
        val response = adapter.fromJson(responseBody)
            ?: throw MMException.Server("MM_PARSE_ERROR", "Failed to parse auth response")

        sessionToken = response.accessToken
        refreshToken = response.refreshToken
        expiresAtMs = System.currentTimeMillis() + (response.expiresInSeconds * 1000L)

        val user = MMUser(
            userId = response.userId,
            displayName = response.displayName,
            avatarUrl = response.avatarUrl
        )
        authenticatedUser = user
        user
    }

    /**
     * Get a valid session token, refreshing if necessary.
     *
     * Refreshes automatically when the token is at 80% of its TTL or expired.
     * Uses a mutex to prevent concurrent refresh storms.
     *
     * @return A valid MM session JWT.
     * @throws MMException.NotAuthenticated if [authenticate] has not been called.
     * @throws MMException.TokenRefreshFailed if refresh fails.
     */
    suspend fun getToken(): String = mutex.withLock {
        val token = sessionToken ?: throw MMException.NotAuthenticated()

        // Refresh at 80% TTL to avoid using tokens close to expiry
        val refreshThresholdMs = expiresAtMs - ((expiresAtMs - (expiresAtMs - 900_000L)) * 0.2).toLong()
        val now = System.currentTimeMillis()

        if (now < refreshThresholdMs) {
            return token
        }

        // Token needs refresh
        return refreshSessionToken()
    }

    /**
     * Ensure the client is authenticated. Throws if not.
     */
    fun ensureAuthenticated() {
        if (sessionToken == null) {
            throw MMException.NotAuthenticated()
        }
    }

    /**
     * Clear the session, invalidating all tokens.
     */
    fun clearSession() {
        sessionToken = null
        refreshToken = null
        expiresAtMs = 0L
        authenticatedUser = null
    }

    /**
     * Refresh the session token using the refresh token.
     */
    private suspend fun refreshSessionToken(): String {
        val refresh = refreshToken
            ?: throw MMException.TokenRefreshFailed(IllegalStateException("No refresh token"))

        val client = requireApiClient()

        return try {
            val responseBody = client.refreshToken(refresh)
            val adapter = moshi.adapter(MMRefreshResponse::class.java)
            val response = adapter.fromJson(responseBody)
                ?: throw MMException.TokenRefreshFailed(IllegalStateException("Failed to parse refresh response"))

            sessionToken = response.accessToken
            refreshToken = response.refreshToken
            expiresAtMs = System.currentTimeMillis() + (response.expiresInSeconds * 1000L)

            response.accessToken
        } catch (e: MMException) {
            throw e
        } catch (e: Exception) {
            // Refresh failed -- clear session so the caller can re-authenticate
            clearSession()
            throw MMException.TokenRefreshFailed(e)
        }
    }

    private fun requireApiClient(): MMApiClient {
        return apiClient ?: throw IllegalStateException(
            "TokenManager.apiClient not set. This is an SDK internal error."
        )
    }
}
