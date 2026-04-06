package com.matrixmedia.sdk.internal.reconnect

import kotlin.math.min
import kotlin.math.pow
import kotlin.random.Random

/**
 * Exponential backoff with jitter for reconnection attempts.
 *
 * Used by [LiveKitBridge] to handle SFU disconnects and by
 * [ConnectivityManager.NetworkCallback] to trigger reconnection
 * when the device's network state changes.
 *
 * ## Parameters
 *
 * - Base delay: 1 second
 * - Maximum delay: 30 seconds
 * - Maximum retries: 10
 * - Jitter: +/- 25% randomization to prevent thundering herd
 *
 * ## Usage
 *
 * ```kotlin
 * val policy = ReconnectPolicy()
 * while (true) {
 *     val delayMs = policy.nextRetryDelayMs() ?: break // null = max retries exceeded
 *     delay(delayMs)
 *     if (tryConnect()) {
 *         policy.reset()
 *         break
 *     }
 * }
 * ```
 *
 * @param baseDelayMs Initial delay in milliseconds. Default 1000ms.
 * @param maxDelayMs Maximum delay cap in milliseconds. Default 30000ms.
 * @param maxRetries Maximum number of retry attempts. Default 10.
 * @param jitterFactor Randomization factor (0.0 to 1.0). Default 0.25.
 */
internal class ReconnectPolicy(
    private val baseDelayMs: Long = 1_000L,
    private val maxDelayMs: Long = 30_000L,
    private val maxRetries: Int = 10,
    private val jitterFactor: Double = 0.25
) {
    private var attempt = 0

    /**
     * Calculate the delay for the next retry attempt.
     *
     * @return Delay in milliseconds, or `null` if max retries exceeded.
     */
    fun nextRetryDelayMs(): Long? {
        if (attempt >= maxRetries) return null

        val exponentialDelay = baseDelayMs * 2.0.pow(attempt.toDouble())
        val cappedDelay = min(exponentialDelay, maxDelayMs.toDouble())

        // Apply jitter: delay * (1 +/- jitterFactor)
        val jitterRange = cappedDelay * jitterFactor
        val jitter = Random.nextDouble(-jitterRange, jitterRange)
        val finalDelay = (cappedDelay + jitter).toLong().coerceAtLeast(0L)

        attempt++
        return finalDelay
    }

    /**
     * Reset the retry counter after a successful connection.
     */
    fun reset() {
        attempt = 0
    }

    /**
     * Current retry attempt number (0-based).
     */
    val currentAttempt: Int get() = attempt
}
