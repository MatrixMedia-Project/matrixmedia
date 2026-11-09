package com.matrixmedia.example.features.streaming.vod

import android.net.Uri
import android.view.ViewGroup
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.core.os.LocaleListCompat
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.MimeTypes
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DefaultDataSource
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.MergingMediaSource
import androidx.media3.exoplayer.source.ProgressiveMediaSource
import androidx.media3.exoplayer.source.SingleSampleMediaSource
import androidx.media3.exoplayer.trackselection.DefaultTrackSelector
import androidx.media3.ui.PlayerView
import com.matrixmedia.sdk.MMRecording
import com.matrixmedia.sdk.captions.captionLanguageLabel
import com.matrixmedia.sdk.captions.captionTracksOf
import com.matrixmedia.sdk.captions.hasCaptions
import com.matrixmedia.sdk.captions.selectCaptionTrack

/**
 * VOD player dialog: ExoPlayer in a [PlayerView] with WebVTT sidecar caption
 * tracks merged into the media.
 *
 * Each entry of the recording's `caption_urls` becomes a
 * [SingleSampleMediaSource] (`text/vtt`, tagged with its language); the
 * sidecars are merged with the programme video via [MergingMediaSource].
 * Language selection goes through [DefaultTrackSelector]'s preferred text
 * language, seeded from the device language via [selectCaptionTrack].
 */
@UnstableApi
@Composable
fun VodPlayerDialog(
    recording: MMRecording,
    onDismiss: () -> Unit
) {
    val context = LocalContext.current
    val tracks = remember(recording) { captionTracksOf(recording.captionUrls) }
    val deviceTags = remember {
        listOf(LocaleListCompat.getDefault()[0]?.toLanguageTag() ?: "en")
    }
    var ccEnabled by remember { mutableStateOf(recording.hasCaptions) }
    var selectedLang by remember(recording) {
        mutableStateOf(
            selectCaptionTrack(recording.captionUrls, deviceTags)?.lang
                ?: tracks.firstOrNull()?.lang
        )
    }

    val trackSelector = remember { DefaultTrackSelector(context) }
    val player = remember(recording) {
        ExoPlayer.Builder(context).setTrackSelector(trackSelector).build().apply {
            val videoUrl = recording.playbackUrl ?: return@apply
            val dataSourceFactory = DefaultDataSource.Factory(context)
            val videoSource = ProgressiveMediaSource.Factory(dataSourceFactory)
                .createMediaSource(MediaItem.fromUri(Uri.parse(videoUrl)))
            val subtitleSources = tracks.map { track ->
                SingleSampleMediaSource.Factory(dataSourceFactory).createMediaSource(
                    MediaItem.SubtitleConfiguration.Builder(Uri.parse(track.url))
                        .setMimeType(MimeTypes.TEXT_VTT)
                        .setLanguage(track.lang)
                        .setLabel(captionLanguageLabel(track.lang))
                        .build(),
                    C.TIME_UNSET
                )
            }
            setMediaSource(MergingMediaSource(true, videoSource, *subtitleSources.toTypedArray()))
            prepare()
            playWhenReady = true
        }
    }

    LaunchedEffect(ccEnabled, selectedLang) {
        trackSelector.parameters = trackSelector.buildUponParameters()
            .setPreferredTextLanguage(if (ccEnabled) selectedLang else null)
            .build()
    }
    DisposableEffect(player) {
        onDispose { player.release() }
    }

    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false)
    ) {
        Surface(shape = MaterialTheme.shapes.medium) {
            Column(modifier = Modifier.padding(16.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        text = recording.title ?: "Recording",
                        style = MaterialTheme.typography.titleMedium,
                        modifier = Modifier.weight(1f)
                    )
                    TextButton(onClick = onDismiss) { Text("Close") }
                }
                Spacer(modifier = Modifier.height(8.dp))
                AndroidView(
                    factory = { ctx ->
                        PlayerView(ctx).apply {
                            this.player = player
                            useController = true
                            layoutParams = ViewGroup.LayoutParams(
                                ViewGroup.LayoutParams.MATCH_PARENT,
                                ViewGroup.LayoutParams.MATCH_PARENT
                            )
                        }
                    },
                    modifier = Modifier
                        .fillMaxWidth()
                        .aspectRatio(16f / 9f)
                )
                if (recording.hasCaptions) {
                    Spacer(modifier = Modifier.height(8.dp))
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        FilterChip(
                            selected = ccEnabled,
                            onClick = { ccEnabled = !ccEnabled },
                            label = { Text("CC") }
                        )
                    }
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .horizontalScroll(rememberScrollState())
                    ) {
                        tracks.forEach { track ->
                            FilterChip(
                                selected = ccEnabled && selectedLang == track.lang,
                                onClick = {
                                    selectedLang = track.lang
                                    ccEnabled = true
                                },
                                label = { Text(captionLanguageLabel(track.lang)) },
                                modifier = Modifier.padding(end = 8.dp)
                            )
                        }
                    }
                }
                recording.summary?.let { summary ->
                    Spacer(modifier = Modifier.height(8.dp))
                    Text(text = summary, style = MaterialTheme.typography.bodySmall)
                }
            }
        }
    }
}
