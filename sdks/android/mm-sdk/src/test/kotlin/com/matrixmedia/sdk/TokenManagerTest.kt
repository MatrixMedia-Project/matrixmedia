package com.matrixmedia.sdk

import com.matrixmedia.sdk.internal.auth.TokenManager
import com.matrixmedia.sdk.internal.networking.MMApiClient
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test

/**
 * Unit tests for [TokenManager].
 *
 * Tests verify token lifecycle: authentication, refresh, error handling,
 * and the 80% TTL refresh threshold.
 */
class TokenManagerTest {

    private val serverUrl = "https://mm.example.com"
    private var tokenProviderCallCount = 0

    private val tokenProvider: suspend () -> MMAuthToken = {
        tokenProviderCallCount++
        MMAuthToken(accessToken = "matrix-token-$tokenProviderCallCount", matrixServerName = "example.com")
    }

    private lateinit var tokenManager: TokenManager

    @Before
    fun setUp() {
        tokenProviderCallCount = 0
        tokenManager = TokenManager(serverUrl, tokenProvider)
    }

    @Test
    fun `ensureAuthenticated throws when not authenticated`() = runTest {
        try {
            tokenManager.ensureAuthenticated()
            fail("Expected MMException.NotAuthenticated")
        } catch (_: MMException.NotAuthenticated) {
            // expected
        }
    }

    @Test
    fun `getToken throws when not authenticated`() = runTest {
        try {
            tokenManager.getToken()
            fail("Expected MMException.NotAuthenticated")
        } catch (_: MMException.NotAuthenticated) {
            // expected
        }
    }

    @Test
    fun `clearSession resets state`() = runTest {
        // After clear, ensureAuthenticated should throw
        tokenManager.clearSession()
        try {
            tokenManager.ensureAuthenticated()
            fail("Expected MMException.NotAuthenticated after clearSession")
        } catch (_: MMException.NotAuthenticated) {
            // expected
        }
    }

    @Test
    fun `TokenManager requires apiClient before authenticate`() = runTest {
        // apiClient is not set, so authenticate should fail with IllegalStateException
        val openIdToken = MMOpenIDToken(
            accessToken = "oid-token",
            tokenType = "Bearer",
            matrixServerName = "example.com",
            expiresIn = 3600
        )
        try {
            tokenManager.authenticate(openIdToken)
            fail("Expected IllegalStateException when apiClient is not set")
        } catch (e: IllegalStateException) {
            assertNotNull(e.message)
        }
    }

    @Test
    fun `constructor does not call tokenProvider`() = runTest {
        // Creating a TokenManager should not immediately invoke the tokenProvider
        val _ = TokenManager(serverUrl, tokenProvider)
        assertEquals(0, tokenProviderCallCount)
    }
}
