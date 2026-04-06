package com.matrixmedia.sdk.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import com.matrixmedia.sdk.MMMediaType
import com.matrixmedia.sdk.MMRecording

/**
 * Jetpack Compose recording player for [MMRecording] playback.
 *
 * Placeholder implementation. In production this would wrap Media3/ExoPlayer
 * wrapped in an [androidx.compose.ui.viewinterop.AndroidView] for the video
 * surface, and an ExoPlayer instance for audio-only streams.
 *
 * The player streams directly from [MMRecording.playbackUrl] (typically a
 * CDN URL) without downloading the entire recording.
 *
 * ## Usage
 *
 * ```kotlin
 * val recordings = client.listRoomRecordings(roomId = "!abc:example.org")
 * recordings.firstOrNull()?.let {
 *     MMRecordingPlayer(recording = it)
 * }
 * ```
 *
 * @param recording The [MMRecording] to play.
 * @param modifier Compose [Modifier] applied to the outer column.
 */
@Composable
fun MMRecordingPlayer(
    recording: MMRecording,
    modifier: Modifier = Modifier
) {
    var isPlaying by remember { mutableStateOf(false) }

    Column(modifier = modifier.padding(16.dp)) {
        if (recording.mediaType == MMMediaType.Video || recording.mediaType == MMMediaType.ScreenShare) {
            // Video placeholder -- real implementation would use
            // AndroidView { PlayerView(ctx).apply { player = exoPlayer } }
            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .aspectRatio(16f / 9f)
                    .background(Color.Black)
            ) {
                Text(
                    text = "Video Recording",
                    color = Color.White,
                    modifier = Modifier.align(Alignment.Center)
                )
            }
        } else {
            // Audio player UI
            Row(verticalAlignment = Alignment.CenterVertically) {
                IconButton(onClick = { isPlaying = !isPlaying }) {
                    Icon(
                        imageVector = if (isPlaying) Icons.Default.Pause else Icons.Default.PlayArrow,
                        contentDescription = if (isPlaying) "Pause" else "Play"
                    )
                }
                Column {
                    Text(
                        text = recording.title ?: "Recording",
                        style = MaterialTheme.typography.titleMedium
                    )
                    recording.durationMs?.let {
                        Text(
                            text = formatDuration(it),
                            style = MaterialTheme.typography.bodySmall
                        )
                    }
                }
            }
        }
    }
}

private fun formatDuration(ms: Long): String {
    val total = ms / 1000
    val h = total / 3600
    val m = (total % 3600) / 60
    val s = total % 60
    return if (h > 0) "%d:%02d:%02d".format(h, m, s) else "%d:%02d".format(m, s)
}
