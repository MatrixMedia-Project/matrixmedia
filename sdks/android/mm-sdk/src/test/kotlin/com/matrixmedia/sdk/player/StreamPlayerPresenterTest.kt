package com.matrixmedia.sdk.player

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class StreamPlayerPresenterTest {

    @Test
    fun `interim segment shows until replaced`() {
        val presenter = StreamPlayerPresenter()
        assertNull(presenter.interim.value)
        presenter.onSegment(LiveCaptionSegment("1", "hel", final = false))
        assertEquals("hel", presenter.interim.value?.text)
        presenter.onSegment(LiveCaptionSegment("1", "hello", final = false))
        assertEquals("hello", presenter.interim.value?.text)
        assertTrue(presenter.finals.value.isEmpty())
    }

    @Test
    fun `final segment appends to scrollback and clears matching interim`() {
        val presenter = StreamPlayerPresenter()
        presenter.onSegment(LiveCaptionSegment("1", "hello", final = false))
        presenter.onSegment(LiveCaptionSegment("1", "hello.", final = true))
        assertNull(presenter.interim.value)
        assertEquals(listOf("hello."), presenter.finals.value.map { it.text })
    }

    @Test
    fun `final keeps unrelated interim on screen`() {
        val presenter = StreamPlayerPresenter()
        presenter.onSegment(LiveCaptionSegment("1", "draft", final = false))
        presenter.onSegment(LiveCaptionSegment("2", "done.", final = true))
        assertEquals("draft", presenter.interim.value?.text)
        assertEquals(listOf("done."), presenter.finals.value.map { it.text })
    }

    @Test
    fun `final scrollback is bounded oldest-first`() {
        val presenter = StreamPlayerPresenter(maxRetainedFinals = 3)
        repeat(5) { i ->
            presenter.onSegment(LiveCaptionSegment("$i", "line $i", final = true))
        }
        assertEquals(listOf("line 2", "line 3", "line 4"), presenter.finals.value.map { it.text })
    }

    @Test
    fun `language filter drops other languages and keeps untagged`() {
        val presenter = StreamPlayerPresenter()
        presenter.selectLanguage("en")
        presenter.onSegment(LiveCaptionSegment("1", "hello", final = true, language = "en"))
        presenter.onSegment(LiveCaptionSegment("2", "hallo", final = true, language = "de"))
        presenter.onSegment(LiveCaptionSegment("3", "untagged", final = true, language = null))
        presenter.onSegment(LiveCaptionSegment("4", "hey", final = false, language = "en-US"))
        assertEquals(listOf("hello", "untagged"), presenter.finals.value.map { it.text })
        assertEquals("hey", presenter.interim.value?.text)
    }

    @Test
    fun `clearing filter accepts everything again`() {
        val presenter = StreamPlayerPresenter()
        presenter.selectLanguage("en")
        presenter.selectLanguage(null)
        presenter.onSegment(LiveCaptionSegment("1", "hallo", final = true, language = "de"))
        assertEquals(listOf("hallo"), presenter.finals.value.map { it.text })
    }

    @Test
    fun `cc toggle state is exposed`() {
        val presenter = StreamPlayerPresenter()
        assertTrue(presenter.captionsVisible.value)
        presenter.setCaptionsVisible(false)
        assertFalse(presenter.captionsVisible.value)
    }

    @Test
    fun `clear resets interim and finals`() {
        val presenter = StreamPlayerPresenter()
        presenter.onSegment(LiveCaptionSegment("1", "draft", final = false))
        presenter.onSegment(LiveCaptionSegment("2", "done.", final = true))
        presenter.clear()
        assertNull(presenter.interim.value)
        assertTrue(presenter.finals.value.isEmpty())
    }
}
