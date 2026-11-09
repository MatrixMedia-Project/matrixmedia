package com.matrixmedia.sdk.captions

import com.matrixmedia.sdk.MMRecording
import java.util.Locale

/**
 * One WebVTT caption track: one file, one language.
 *
 * Mirrors the captions development guide terminology: [lang] is a BCP-47 tag
 * (`caption_urls` keys), [url] is the server-listed `text/vtt` URL.
 */
data class CaptionTrack(
    val lang: String,
    val url: String
)

/** A recording has usable captions when the server lists at least one track. */
val MMRecording.hasCaptions: Boolean
    get() = captionUrls.isNotEmpty()

/** Caption languages sorted for stable badge/picker display. */
fun MMRecording.sortedCaptionLanguages(): List<String> =
    captionUrls.keys.sorted()

/** Caption tracks sorted by language tag for stable picker display. */
fun captionTracksOf(captionUrls: Map<String, String>): List<CaptionTrack> =
    captionUrls.entries
        .map { CaptionTrack(lang = it.key, url = it.value) }
        .sortedBy { it.lang }

/**
 * Pick the caption track to play.
 *
 * Order: exact device-language match, primary-subtag match (either direction,
 * so device `en-US` finds track `en` and device `en` finds track `en-US`),
 * streamer [defaultLang] (same matching), `en`, then the first track by
 * sorted tag so the result is deterministic. Null when there are no tracks.
 *
 * @param captionUrls Raw `caption_urls` map from the recording response.
 * @param deviceTags Ordered device language preferences (BCP-47).
 * @param defaultLang Streamer-selected default language, if any.
 */
fun selectCaptionTrack(
    captionUrls: Map<String, String>,
    deviceTags: List<String>,
    defaultLang: String? = null
): CaptionTrack? {
    if (captionUrls.isEmpty()) return null
    val byTag = captionUrls.entries.associate { it.key.lowercase() to it }

    fun match(tag: String): CaptionTrack? {
        val lower = tag.lowercase()
        byTag[lower]?.let { return CaptionTrack(it.key, it.value) }
        val primary = lower.substringBefore('-')
        byTag[primary]?.let { return CaptionTrack(it.key, it.value) }
        byTag.values.firstOrNull { it.key.substringBefore('-').lowercase() == primary }
            ?.let { return CaptionTrack(it.key, it.value) }
        return null
    }

    deviceTags.forEach { match(it)?.let { return it } }
    defaultLang?.let { match(it)?.let { return it } }
    match("en")?.let { return it }
    val first = captionUrls.entries.sortedBy { it.key }.first()
    return CaptionTrack(first.key, first.value)
}

/** Human-readable picker label for a caption language tag. */
fun captionLanguageLabel(lang: String): String {
    val display = Locale.forLanguageTag(lang).getDisplayLanguage(Locale.ENGLISH)
    return display.replaceFirstChar { it.uppercase() }.ifEmpty { lang.uppercase() }
}
