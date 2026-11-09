package com.matrixmedia.sdk.player

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

/**
 * One live caption segment, mirroring the LiveKit `TranscriptionSegment`
 * wire shape (`id, text, start_time, end_time, final, language`).
 *
 * Only `final=true` segments persist to VOD; interim segments are
 * display-only. Times are milliseconds from the broadcast start, null when
 * the publisher does not provide them.
 */
data class LiveCaptionSegment(
    val id: String,
    val text: String,
    val startMs: Long? = null,
    val endMs: Long? = null,
    val final: Boolean,
    val language: String? = null
)

/**
 * Presenter for the live caption overlay.
 *
 * Pure Kotlin (no Android framework): the Compose overlay in `ui` and the
 * demo `RoomStreamPlayerScreen` observe [interim] / [finals] /
 * [captionsVisible]. Interim segments render in a lighter style; final
 * segments append to a bounded scrollback.
 */
class StreamPlayerPresenter(
    private val maxRetainedFinals: Int = 50
) {
    private val _interim = MutableStateFlow<LiveCaptionSegment?>(null)
    /** Latest interim (non-final) segment, null when none is on screen. */
    val interim: StateFlow<LiveCaptionSegment?> = _interim.asStateFlow()

    private val _finals = MutableStateFlow<List<LiveCaptionSegment>>(emptyList())
    /** Final segments, oldest-first, bounded to [maxRetainedFinals]. */
    val finals: StateFlow<List<LiveCaptionSegment>> = _finals.asStateFlow()

    private val _selectedLanguage = MutableStateFlow<String?>(null)
    /**
     * BCP-47 language filter, null for all languages. Segments carrying a
     * different language are dropped; segments with no language tag always
     * pass (publishers may omit it).
     */
    val selectedLanguage: StateFlow<String?> = _selectedLanguage.asStateFlow()

    private val _captionsVisible = MutableStateFlow(true)
    /** CC toggle state. */
    val captionsVisible: StateFlow<Boolean> = _captionsVisible.asStateFlow()

    /** Feed one segment from the data channel / switch viewer channel. */
    fun onSegment(segment: LiveCaptionSegment) {
        if (!acceptsLanguage(segment.language)) return
        if (segment.final) {
            _finals.update { (it + segment).takeLast(maxRetainedFinals) }
            if (_interim.value?.id == segment.id) _interim.value = null
        } else {
            _interim.value = segment
        }
    }

    fun selectLanguage(tag: String?) {
        _selectedLanguage.value = tag
    }

    fun setCaptionsVisible(visible: Boolean) {
        _captionsVisible.value = visible
    }

    /** Clear on-screen state (leave, stream end, source switch). */
    fun clear() {
        _interim.value = null
        _finals.value = emptyList()
    }

    private fun acceptsLanguage(tag: String?): Boolean {
        val selected = _selectedLanguage.value ?: return true
        if (tag == null) return true
        return tag.equals(selected, ignoreCase = true) ||
            tag.substringBefore('-').equals(selected.substringBefore('-'), ignoreCase = true)
    }
}
