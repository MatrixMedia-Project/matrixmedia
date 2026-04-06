package com.matrixmedia.example

/**
 * Development configuration for the example app.
 *
 * Uses hardcoded values to avoid requiring a full Matrix SDK dependency.
 * Replace these with real values from your MM server for testing.
 *
 * IMPORTANT: Use the host machine's LAN IP, not localhost.
 * Android emulators cannot reach `localhost` on the host machine.
 * See: PTT Knowledge Base, Gotcha #11.
 */
object Config {
    /**
     * MM server URL.
     *
     * For local development, use the host machine's LAN IP (e.g., 10.0.0.105).
     * The Android emulator's `10.0.2.2` maps to the host's localhost, but
     * using the actual LAN IP is more reliable across emulators and physical devices.
     */
    const val MM_SERVER_URL = "http://10.0.0.105:6167"

    /**
     * Pre-generated MM authentication token for development.
     *
     * In production, this would be obtained by exchanging a Matrix OpenID token
     * via the `/auth/token` endpoint. For the example app, we skip the Matrix
     * SDK dependency and use a hardcoded dev token.
     *
     * Generate one via:
     * ```
     * curl -X POST http://10.0.0.105:6167/_mm/client/v1/auth/token \
     *   -H "Content-Type: application/json" \
     *   -d '{"access_token":"dev","token_type":"Bearer","matrix_server_name":"localhost","expires_in":3600}'
     * ```
     */
    const val DEV_TOKEN = "dev-token-replace-me"

    /**
     * Matrix server name for the dev environment.
     */
    const val MATRIX_SERVER_NAME = "localhost"

    /**
     * Default room ID for quick testing.
     */
    const val DEFAULT_ROOM_ID = "!test:localhost"
}
