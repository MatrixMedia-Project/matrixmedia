package com.matrixmedia.sdk.captions

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class CaptionTrackSelectorTest {

    private val tracks = mapOf(
        "en" to "https://cdn.example/r1.en.vtt",
        "de" to "https://cdn.example/r1.de.vtt"
    )

    @Test
    fun `exact device language match wins`() {
        val picked = selectCaptionTrack(tracks, listOf("de", "en"))
        assertEquals(CaptionTrack("de", "https://cdn.example/r1.de.vtt"), picked)
    }

    @Test
    fun `device region tag falls back to primary subtag track`() {
        val picked = selectCaptionTrack(tracks, listOf("en-US"))
        assertEquals("en", picked?.lang)
    }

    @Test
    fun `bare device tag matches regional track variant`() {
        val regional = mapOf("en-US" to "https://cdn.example/r1.en-US.vtt")
        val picked = selectCaptionTrack(regional, listOf("en"))
        assertEquals("en-US", picked?.lang)
    }

    @Test
    fun `matching is case-insensitive`() {
        val picked = selectCaptionTrack(tracks, listOf("DE"))
        assertEquals("de", picked?.lang)
    }

    @Test
    fun `streamer default applies when device matches nothing`() {
        val picked = selectCaptionTrack(tracks, listOf("fr"), defaultLang = "de")
        assertEquals("de", picked?.lang)
    }

    @Test
    fun `english is preferred over other unmatched tracks`() {
        val picked = selectCaptionTrack(tracks, listOf("fr"))
        assertEquals("en", picked?.lang)
    }

    @Test
    fun `first sorted track wins with no better candidate`() {
        val only = mapOf(
            "fr" to "https://cdn.example/r1.fr.vtt",
            "de" to "https://cdn.example/r1.de.vtt"
        )
        val picked = selectCaptionTrack(only, listOf("es"))
        assertEquals("de", picked?.lang)
    }

    @Test
    fun `empty map returns null`() {
        assertNull(selectCaptionTrack(emptyMap(), listOf("en"), defaultLang = "en"))
    }

    @Test
    fun `tracks are sorted by language tag`() {
        assertEquals(
            listOf(
                CaptionTrack("de", "https://cdn.example/r1.de.vtt"),
                CaptionTrack("en", "https://cdn.example/r1.en.vtt")
            ),
            captionTracksOf(tracks)
        )
    }

    @Test
    fun `language labels are human-readable`() {
        assertEquals("English", captionLanguageLabel("en"))
        assertEquals("German", captionLanguageLabel("de"))
    }
}
