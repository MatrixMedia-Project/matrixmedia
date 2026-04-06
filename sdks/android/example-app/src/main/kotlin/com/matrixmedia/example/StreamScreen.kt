package com.matrixmedia.example

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.matrixmedia.sdk.MMStream
import com.matrixmedia.sdk.MMStreamState
import com.matrixmedia.sdk.ui.MMAudioRenderer
import com.matrixmedia.sdk.ui.MMAudioRendererStyle

/**
 * Stream screen showing audio visualizer and controls.
 *
 * Displays:
 * - [MMAudioRenderer] for real-time audio visualization
 * - Connection state indicator
 * - Viewer count
 * - Volume slider
 * - Mute toggle
 * - Leave button
 *
 * Uses `DisposableEffect` to ensure the stream is cleaned up if the
 * composable is removed from the composition tree unexpectedly.
 */
@Composable
fun StreamScreen(
    stream: MMStream,
    onLeave: () -> Unit
) {
    val streamState by stream.state.collectAsState()
    val viewerCount by stream.viewerCount.collectAsState()
    val host by stream.host.collectAsState()
    val isMuted by stream.isMuted.collectAsState()
    var volume by remember { mutableFloatStateOf(1.0f) }

    // Clean up on removal from composition
    DisposableEffect(stream) {
        onDispose {
            // Best-effort cleanup if the composable is removed unexpectedly
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(24.dp),
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        // Stream state header
        Text(
            text = when (streamState) {
                is MMStreamState.Connecting -> "Connecting..."
                is MMStreamState.Connected -> "Live"
                is MMStreamState.Reconnecting -> "Reconnecting..."
                is MMStreamState.Disconnected -> "Disconnected"
                is MMStreamState.Error -> "Error"
            },
            style = MaterialTheme.typography.headlineSmall,
            color = when (streamState) {
                is MMStreamState.Connected -> MaterialTheme.colorScheme.primary
                is MMStreamState.Error -> MaterialTheme.colorScheme.error
                else -> MaterialTheme.colorScheme.onSurface
            }
        )

        Spacer(modifier = Modifier.height(8.dp))

        // Host + viewer info
        host?.let { hostParticipant ->
            Text(
                text = "Host: ${hostParticipant.displayName ?: hostParticipant.userId}",
                style = MaterialTheme.typography.bodyMedium
            )
        }

        Text(
            text = "$viewerCount viewer${if (viewerCount != 1) "s" else ""}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )

        Spacer(modifier = Modifier.height(32.dp))

        // Audio visualizer
        MMAudioRenderer(
            stream = stream,
            modifier = Modifier
                .fillMaxWidth()
                .height(160.dp),
            style = MMAudioRendererStyle.Bars,
            barColor = MaterialTheme.colorScheme.primary
        )

        Spacer(modifier = Modifier.height(32.dp))

        // Volume slider
        Text(
            text = "Volume: ${(volume * 100).toInt()}%",
            style = MaterialTheme.typography.bodyMedium
        )
        Slider(
            value = volume,
            onValueChange = {
                volume = it
                stream.setVolume(it)
            },
            modifier = Modifier.fillMaxWidth()
        )

        Spacer(modifier = Modifier.height(16.dp))

        // Controls row
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            // Mute toggle
            Button(
                onClick = { stream.setMuted(!isMuted) },
                modifier = Modifier.weight(1f),
                colors = if (isMuted) {
                    ButtonDefaults.buttonColors(
                        containerColor = MaterialTheme.colorScheme.errorContainer,
                        contentColor = MaterialTheme.colorScheme.onErrorContainer
                    )
                } else {
                    ButtonDefaults.buttonColors()
                }
            ) {
                Text(if (isMuted) "Unmute" else "Mute")
            }

            // Leave button
            Button(
                onClick = onLeave,
                modifier = Modifier.weight(1f),
                colors = ButtonDefaults.buttonColors(
                    containerColor = MaterialTheme.colorScheme.error,
                    contentColor = MaterialTheme.colorScheme.onError
                )
            ) {
                Text("Leave")
            }
        }
    }
}
