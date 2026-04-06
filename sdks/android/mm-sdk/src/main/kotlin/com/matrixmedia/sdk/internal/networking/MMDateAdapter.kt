package com.matrixmedia.sdk.internal.networking

import com.squareup.moshi.FromJson
import com.squareup.moshi.ToJson
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.TimeZone

/**
 * Moshi adapter for parsing ISO8601 dates with optional fractional seconds.
 *
 * The MM server (Rust) serializes timestamps with microsecond precision:
 *   `2026-04-03T12:34:56.789012Z`
 *
 * The PTT server (Go) used `time.Time.MarshalJSON` which emits nanosecond
 * fractional seconds. This caused parsing failures on both iOS and Android
 * (PTT bug #10). This adapter handles both formats by trying the fractional
 * format first, then falling back to plain ISO8601.
 *
 * Supported formats:
 * - `yyyy-MM-dd'T'HH:mm:ss.SSSSSS'Z'` (fractional seconds, 1-6 digits)
 * - `yyyy-MM-dd'T'HH:mm:ss'Z'` (no fractional seconds)
 */
internal class MMDateAdapter {
    private val utc = TimeZone.getTimeZone("UTC")

    private val fractionalFormat = SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss.SSSSSS'Z'", Locale.US).apply {
        timeZone = utc
    }

    private val plainFormat = SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss'Z'", Locale.US).apply {
        timeZone = utc
    }

    private val outputFormat = SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss.SSS'Z'", Locale.US).apply {
        timeZone = utc
    }

    @FromJson
    fun fromJson(dateString: String): Date {
        // Try fractional seconds first (most common from server)
        return try {
            synchronized(fractionalFormat) { fractionalFormat.parse(dateString) }
                ?: throw IllegalArgumentException("Failed to parse date: $dateString")
        } catch (_: Exception) {
            try {
                synchronized(plainFormat) { plainFormat.parse(dateString) }
                    ?: throw IllegalArgumentException("Failed to parse date: $dateString")
            } catch (e: Exception) {
                throw IllegalArgumentException("Cannot parse date '$dateString': ${e.message}", e)
            }
        }
    }

    @ToJson
    fun toJson(date: Date): String {
        return synchronized(outputFormat) { outputFormat.format(date) }
    }
}
