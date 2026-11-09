package com.matrixmedia.sdk.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.matrixmedia.sdk.player.LiveCaptionSegment

/**
 * Live caption overlay: absolutely positioned caption box for the live
 * player. Live segments arrive as data (data channel / switch viewer
 * channel), not a file — so this is an HTML-overlay equivalent, never a
 * subtitle track.
 *
 * Final segments form a short scrollback; the interim segment renders below
 * them in a lighter style.
 */
@Composable
fun LiveCaptionsOverlay(
    finals: List<LiveCaptionSegment>,
    interim: LiveCaptionSegment?,
    visible: Boolean,
    modifier: Modifier = Modifier,
    scrollback: Int = 3
) {
    if (!visible) return
    val shownFinals = finals.takeLast(scrollback)
    if (shownFinals.isEmpty() && interim == null) return

    Box(
        modifier = modifier
            .fillMaxWidth()
            .padding(12.dp),
        contentAlignment = Alignment.BottomCenter
    ) {
        Column(
            modifier = Modifier
                .background(Color.Black.copy(alpha = 0.65f), RoundedCornerShape(8.dp))
                .padding(horizontal = 12.dp, vertical = 8.dp),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            shownFinals.forEach { segment ->
                Text(
                    text = segment.text,
                    color = Color.White,
                    fontSize = 15.sp
                )
            }
            interim?.let { segment ->
                Text(
                    text = segment.text,
                    color = Color.White.copy(alpha = 0.6f),
                    fontSize = 15.sp
                )
            }
        }
    }
}
