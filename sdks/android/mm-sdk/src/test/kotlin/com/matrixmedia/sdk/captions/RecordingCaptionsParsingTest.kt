package com.matrixmedia.sdk.captions

import com.matrixmedia.sdk.MMMediaType
import com.matrixmedia.sdk.MMRecording
import com.matrixmedia.sdk.MMRecordingStatus
import com.squareup.moshi.Json
import com.squareup.moshi.Moshi
import com.squareup.moshi.kotlin.reflect.KotlinJsonAdapterFactory
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Contract tests for the recording caption fields: the server lists only
 * ready tracks in `caption_urls` (same gating rule as `mp4_url`), and older
 * servers omit both fields — parsing must default, never fail.
 */
class RecordingCaptionsParsingTest {

    /** Local mirror of the internal DTO shape (field names are the contract). */
    private data class RecordingDto(
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
        @Json(name = "created_at") val createdAt: String,
        @Json(name = "caption_urls") val captionUrls: Map<String, String> = emptyMap(),
        @Json(name = "summary") val summary: String? = null
    )

    private val moshi: Moshi = Moshi.Builder()
        .addLast(KotlinJsonAdapterFactory())
        .build()

    @Test
    fun `caption_urls and summary parse when present`() {
        val json = """
            {
              "id": "rec1", "stream_id": "s1", "host_user_id": "@a:x.y",
              "media_type": "video", "title": "Show", "status": "ready",
              "duration_ms": 30000, "size_bytes": 1024,
              "playback_url": "https://cdn.example/r1.mp4", "mxc_url": null,
              "created_at": "2026-11-09T00:00:00Z",
              "caption_urls": {"en": "https://cdn.example/r1.en.vtt", "de": "https://cdn.example/r1.de.vtt"},
              "summary": "A show about testing."
            }
        """.trimIndent()
        val dto = moshi.adapter(RecordingDto::class.java).fromJson(json)!!
        assertEquals(
            mapOf(
                "en" to "https://cdn.example/r1.en.vtt",
                "de" to "https://cdn.example/r1.de.vtt"
            ),
            dto.captionUrls
        )
        assertEquals("A show about testing.", dto.summary)
    }

    @Test
    fun `missing caption fields default to empty and null`() {
        val json = """
            {
              "id": "rec1", "stream_id": "s1", "host_user_id": "@a:x.y",
              "media_type": "video", "title": null, "status": "ready",
              "duration_ms": null, "size_bytes": null,
              "playback_url": null, "mxc_url": null,
              "created_at": "2026-11-09T00:00:00Z"
            }
        """.trimIndent()
        val dto = moshi.adapter(RecordingDto::class.java).fromJson(json)!!
        assertEquals(emptyMap<String, String>(), dto.captionUrls)
        assertNull(dto.summary)
    }

    @Test
    fun `hasCaptions reflects caption_urls presence`() {
        assertTrue(recordingWith(mapOf("en" to "https://cdn.example/r1.en.vtt")).hasCaptions)
        assertFalse(recordingWith(emptyMap()).hasCaptions)
    }

    @Test
    fun `sortedCaptionLanguages is stable`() {
        val recording = recordingWith(
            mapOf("de" to "d", "en" to "e", "fr" to "f")
        )
        assertEquals(listOf("de", "en", "fr"), recording.sortedCaptionLanguages())
    }

    private fun recordingWith(captionUrls: Map<String, String>) = MMRecording(
        id = "rec1",
        streamId = "s1",
        hostUserId = "@a:x.y",
        mediaType = MMMediaType.Video,
        title = "Show",
        status = MMRecordingStatus.Ready,
        durationMs = 30000,
        sizeBytes = 1024,
        playbackUrl = "https://cdn.example/r1.mp4",
        mxcUrl = null,
        createdAt = "2026-11-09T00:00:00Z",
        captionUrls = captionUrls,
        summary = null
    )
}
