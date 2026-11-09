package com.matrixmedia.example.features.streaming.vod

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Badge
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.matrixmedia.sdk.MMRecording
import com.matrixmedia.sdk.captions.hasCaptions
import com.matrixmedia.sdk.captions.sortedCaptionLanguages

/**
 * Past broadcasts sheet: room recordings list with a CC badge on every
 * recording whose `caption_urls` is present (i.e. captions finished
 * processing). Recordings without captions show no badge — never a badge
 * that leads nowhere.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PastBroadcastsSheet(
    recordings: List<MMRecording>,
    onSelect: (MMRecording) -> Unit,
    onDismiss: () -> Unit
) {
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Text(
            text = "Past broadcasts",
            style = MaterialTheme.typography.headlineSmall,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp)
        )
        LazyColumn {
            items(recordings, key = { it.id }) { recording ->
                ListItem(
                    headlineContent = { Text(recording.title ?: "Recording") },
                    supportingContent = {
                        Column {
                            Text(
                                text = recording.status.name,
                                style = MaterialTheme.typography.bodySmall
                            )
                            if (recording.hasCaptions) {
                                Text(
                                    text = recording.sortedCaptionLanguages().joinToString(", "),
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant
                                )
                            }
                        }
                    },
                    trailingContent = {
                        if (recording.hasCaptions) {
                            Row(verticalAlignment = Alignment.CenterVertically) {
                                Badge { Text("CC") }
                            }
                        }
                    },
                    modifier = Modifier
                        .fillMaxWidth()
                        .clickable { onSelect(recording) }
                )
            }
        }
    }
}
