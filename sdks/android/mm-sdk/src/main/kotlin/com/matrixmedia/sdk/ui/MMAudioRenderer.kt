package com.matrixmedia.sdk.ui

import androidx.compose.foundation.layout.height
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import com.matrixmedia.sdk.MMStream

/**
 * Jetpack Compose component that renders a real-time audio visualization
 * for an [MMStream].
 *
 * Uses LiveKit's built-in audio level callbacks (not custom FFT) to drive
 * the visualization, as specified in the MatrixMedia plan.
 *
 * ## Usage
 *
 * ```kotlin
 * MMAudioRenderer(
 *     stream = activeStream,
 *     style = MMAudioRendererStyle.Bars,
 *     barColor = MaterialTheme.colorScheme.primary,
 *     modifier = Modifier.fillMaxWidth()
 * )
 * ```
 *
 * @param stream The [MMStream] to visualize.
 * @param modifier Compose [Modifier] applied to the canvas.
 * @param style Visual style for the audio renderer. Defaults to [MMAudioRendererStyle.Bars].
 * @param barColor Color used for the visualization elements. Defaults to the primary theme color.
 */
@Composable
fun MMAudioRenderer(
    stream: MMStream,
    modifier: Modifier = Modifier,
    style: MMAudioRendererStyle = MMAudioRendererStyle.Bars,
    barColor: Color = MaterialTheme.colorScheme.primary
) {
    val audioLevel by stream.audioLevel.collectAsState()

    AudioVisualizerCanvas(
        audioLevel = audioLevel,
        style = style,
        barColor = barColor,
        modifier = modifier.height(120.dp)
    )
}

/**
 * Visual style for [MMAudioRenderer].
 */
enum class MMAudioRendererStyle {
    /** Vertical frequency bars (default). */
    Bars,

    /** Continuous waveform line. */
    Waveform,

    /** Minimal single-bar indicator. */
    Minimal
}
