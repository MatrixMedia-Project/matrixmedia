package com.matrixmedia.sdk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.fail
import org.junit.Test

/**
 * Unit tests for [MMClient].
 *
 * Tests focus on input validation and initialization logic that does not
 * require Android context or network access.
 */
class MMClientTest {

    private val validServerUrl = "https://mm.example.com"
    private val dummyTokenProvider: suspend () -> MMAuthToken = {
        MMAuthToken(accessToken = "test-token", matrixServerName = "example.com")
    }

    @Test
    fun `constructor accepts valid HTTPS URL`() {
        val client = MMClient(serverUrl = validServerUrl, tokenProvider = dummyTokenProvider)
        assertNotNull(client)
    }

    @Test
    fun `constructor accepts valid HTTP URL`() {
        // HTTP is valid for local development (e.g., http://10.0.0.105:8080)
        val client = MMClient(serverUrl = "http://localhost:8080", tokenProvider = dummyTokenProvider)
        assertNotNull(client)
    }

    @Test
    fun `constructor rejects blank URL`() {
        try {
            MMClient(serverUrl = "", tokenProvider = dummyTokenProvider)
            fail("Expected MMException.InvalidServerUrl")
        } catch (e: MMException.InvalidServerUrl) {
            assertEquals("", e.url)
        }
    }

    @Test
    fun `constructor rejects URL without scheme`() {
        try {
            MMClient(serverUrl = "mm.example.com", tokenProvider = dummyTokenProvider)
            fail("Expected MMException.InvalidServerUrl")
        } catch (e: MMException.InvalidServerUrl) {
            assertEquals("mm.example.com", e.url)
        }
    }

    @Test
    fun `constructor rejects non-HTTP scheme`() {
        try {
            MMClient(serverUrl = "ftp://mm.example.com", tokenProvider = dummyTokenProvider)
            fail("Expected MMException.InvalidServerUrl")
        } catch (e: MMException.InvalidServerUrl) {
            assertEquals("ftp://mm.example.com", e.url)
        }
    }

    @Test
    fun `initial connection state is Disconnected`() {
        val client = MMClient(serverUrl = validServerUrl, tokenProvider = dummyTokenProvider)
        assertEquals(MMConnectionState.Disconnected, client.connectionState.value)
    }

    @Test
    fun `constructor handles trailing slash in URL`() {
        // Should not throw -- trailing slash is stripped internally
        val client = MMClient(serverUrl = "https://mm.example.com/", tokenProvider = dummyTokenProvider)
        assertNotNull(client)
    }

    @Test
    fun `constructor is case-insensitive for scheme`() {
        val client = MMClient(serverUrl = "HTTPS://mm.example.com", tokenProvider = dummyTokenProvider)
        assertNotNull(client)
    }
}
