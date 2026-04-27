package com.matrixmedia.sdk.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.matrixmedia.sdk.MMStream

@Composable
fun MMVideoRenderer(
    stream: MMStream,
    modifier: Modifier = Modifier,
    fit: MMVideoFit = MMVideoFit.Contain
) {
    val hasVideo by stream.hasVideo.collectAsState()
    val isScreenShare by stream.isScreenShare.collectAsState()

    if (hasVideo) {
        Box(
            modifier = modifier
                .aspectRatio(16f / 9f)
                .background(Color.Black),
            contentAlignment = Alignment.Center
        ) {
            // Placeholder for LiveKit VideoRenderer integration. Material Icons
            // dep is intentionally not pulled in — keep the placeholder text-only
            // so the SDK doesn't grow a 12 MB icon font transitive.
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                Text(
                    if (isScreenShare) "Screen Share" else "Camera",
                    color = Color.White.copy(alpha = 0.7f),
                    fontSize = 12.sp
                )
            }
        }
    } else {
        MMAudioRenderer(stream = stream, modifier = modifier)
    }
}

enum class MMVideoFit { Contain, Cover, Fill }
