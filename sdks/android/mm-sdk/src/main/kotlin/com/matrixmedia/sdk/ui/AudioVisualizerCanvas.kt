package com.matrixmedia.sdk.ui

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.DrawScope
import kotlin.math.cos
import kotlin.math.sin

/**
 * Canvas-based audio visualization driven by a single audio level value.
 *
 * Simulates a frequency distribution from the aggregate level and renders
 * it using the selected [MMAudioRendererStyle]. Each bar has a slightly
 * different phase offset to create visual variety even from a single input.
 *
 * Animation uses Compose's `animateFloatAsState` for smooth transitions
 * between level updates (~50ms intervals from LiveKit callbacks).
 */
@Composable
internal fun AudioVisualizerCanvas(
    audioLevel: Float,
    style: MMAudioRendererStyle,
    barColor: Color,
    modifier: Modifier = Modifier
) {
    val animatedLevel by animateFloatAsState(
        targetValue = audioLevel.coerceIn(0f, 1f),
        animationSpec = tween(durationMillis = 80),
        label = "audioLevel"
    )

    // Simulated per-bar distribution using deterministic phase offsets
    val barCount = when (style) {
        MMAudioRendererStyle.Bars -> 24
        MMAudioRendererStyle.Waveform -> 48
        MMAudioRendererStyle.Minimal -> 1
    }

    val barHeights = remember(barCount) { FloatArray(barCount) }

    // Compute per-bar heights from the single audio level
    for (i in 0 until barCount) {
        val phase = i.toFloat() / barCount.toFloat() * Math.PI.toFloat() * 2f
        val distribution = (sin(phase * 2.3f + 0.7f) * 0.3f + 0.7f).coerceIn(0.2f, 1f)
        barHeights[i] = animatedLevel * distribution
    }

    Canvas(modifier = modifier) {
        when (style) {
            MMAudioRendererStyle.Bars -> drawBars(barHeights, barColor)
            MMAudioRendererStyle.Waveform -> drawWaveform(barHeights, barColor)
            MMAudioRendererStyle.Minimal -> drawMinimal(animatedLevel, barColor)
        }
    }
}

private fun DrawScope.drawBars(heights: FloatArray, color: Color) {
    val barCount = heights.size
    val totalSpacing = size.width * 0.3f
    val barSpacing = totalSpacing / (barCount + 1)
    val barWidth = (size.width - totalSpacing) / barCount
    val cornerRadius = CornerRadius(barWidth / 2f, barWidth / 2f)

    for (i in heights.indices) {
        val barHeight = (heights[i] * size.height * 0.9f).coerceAtLeast(barWidth)
        val x = barSpacing + i * (barWidth + barSpacing)
        val y = (size.height - barHeight) / 2f

        drawRoundRect(
            color = color.copy(alpha = 0.6f + heights[i] * 0.4f),
            topLeft = Offset(x, y),
            size = Size(barWidth, barHeight),
            cornerRadius = cornerRadius
        )
    }
}

private fun DrawScope.drawWaveform(heights: FloatArray, color: Color) {
    if (heights.isEmpty()) return

    val path = Path()
    val segmentWidth = size.width / (heights.size - 1).coerceAtLeast(1)
    val centerY = size.height / 2f

    path.moveTo(0f, centerY)

    for (i in heights.indices) {
        val x = i * segmentWidth
        val amplitude = heights[i] * size.height * 0.4f
        val y = centerY - amplitude * cos(i.toFloat() * 0.5f)

        if (i == 0) {
            path.moveTo(x, y)
        } else {
            val prevX = (i - 1) * segmentWidth
            val cpX = (prevX + x) / 2f
            path.cubicTo(cpX, path.getBounds().bottom, cpX, y, x, y)
        }
    }

    drawPath(
        path = path,
        color = color,
        style = androidx.compose.ui.graphics.drawscope.Stroke(width = 3f)
    )
}

private fun DrawScope.drawMinimal(level: Float, color: Color) {
    val barWidth = size.width * 0.6f
    val barHeight = (level * size.height * 0.8f).coerceAtLeast(4f)
    val x = (size.width - barWidth) / 2f
    val y = (size.height - barHeight) / 2f

    drawRoundRect(
        color = color,
        topLeft = Offset(x, y),
        size = Size(barWidth, barHeight),
        cornerRadius = CornerRadius(barHeight / 2f, barHeight / 2f)
    )
}
