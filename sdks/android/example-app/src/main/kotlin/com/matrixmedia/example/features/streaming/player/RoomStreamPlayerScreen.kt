package com.matrixmedia.example.features.streaming.player

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import com.matrixmedia.sdk.player.StreamPlayerPresenter
import com.matrixmedia.sdk.ui.LiveCaptionsOverlay

/**
 * Live room stream player screen with the caption overlay.
 *
 * Segments arrive as data (LiveKit data channel / mm-switch viewer channel)
 * and are fed to [presenter]; the overlay renders them over the video
 * surface — interim segments lighter, final segments as scrollback. The
 * `MMType.caption` font style used elsewhere is unrelated; this is caption
 * *tracks*.
 */
@Composable
fun RoomStreamPlayerScreen(
    modifier: Modifier = Modifier,
    presenter: StreamPlayerPresenter = remember { StreamPlayerPresenter() }
) {
    val interim by presenter.interim.collectAsState()
    val finals by presenter.finals.collectAsState()
    val visible by presenter.captionsVisible.collectAsState()

    Box(modifier = modifier.fillMaxWidth()) {
        // Video surface: the LiveKit / mm-switch video view attaches here.
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .aspectRatio(16f / 9f)
                .background(Color.Black),
            contentAlignment = Alignment.Center
        ) {
            Text(text = "Live video", color = Color.White)
        }
        LiveCaptionsOverlay(
            finals = finals,
            interim = interim,
            visible = visible,
            modifier = Modifier.align(Alignment.BottomCenter)
        )
        TextButton(
            onClick = { presenter.setCaptionsVisible(!visible) },
            modifier = Modifier
                .align(Alignment.TopEnd)
                .padding(8.dp)
        ) {
            Text(
                text = "CC",
                color = if (visible) MaterialTheme.colorScheme.primary
                else Color.White.copy(alpha = 0.5f)
            )
        }
    }
}
