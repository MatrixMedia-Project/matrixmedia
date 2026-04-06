package com.matrixmedia.sdk

/**
 * Sealed exception hierarchy for all MatrixMedia SDK errors.
 *
 * Consumers should catch [MMException] to handle all SDK errors, or catch
 * specific subclasses for fine-grained handling.
 *
 * Error codes from the server follow the `MM_*` prefix convention
 * (e.g., `MM_STREAM_NOT_FOUND`, `MM_RATE_LIMITED`).
 */
sealed class MMException(
    message: String,
    cause: Throwable? = null
) : Exception(message, cause) {

    /** The client has not authenticated with the MM server. */
    class NotAuthenticated : MMException("Not authenticated")

    /** The requested stream does not exist or has ended. */
    class StreamNotFound : MMException("Stream not found")

    /** The stream has reached its maximum participant capacity. */
    class StreamFull : MMException("Stream is full")

    /** The stream has already ended. */
    class StreamEnded : MMException("Stream has ended")

    /** The user does not have permission for this operation. */
    class NotAuthorized : MMException("Not authorized")

    /** Microphone permission was not granted by the user. */
    class MicrophonePermissionDenied : MMException("Microphone permission denied")

    /** A network error occurred during communication. */
    class Network(cause: Throwable) : MMException("Network error: ${cause.message}", cause)

    /** The server returned an error response. */
    class Server(
        val code: String,
        override val message: String
    ) : MMException("Server error [$code]: $message")

    /** The server URL is invalid or unreachable. */
    class InvalidServerUrl(val url: String) : MMException("Invalid server URL: $url")

    /** Rate limited by the server. */
    class RateLimited(
        val retryAfterMs: Long?
    ) : MMException("Rate limited${retryAfterMs?.let { ", retry after ${it}ms" } ?: ""}")

    /** The SFU is unavailable. */
    class SfuUnavailable : MMException("SFU unavailable")

    /** Token refresh failed. */
    class TokenRefreshFailed(cause: Throwable? = null) : MMException("Token refresh failed", cause)
}
