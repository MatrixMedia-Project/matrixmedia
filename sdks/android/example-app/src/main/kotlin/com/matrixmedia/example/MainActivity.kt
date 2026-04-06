package com.matrixmedia.example

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.matrixmedia.sdk.MMAuthToken
import com.matrixmedia.sdk.MMClient
import com.matrixmedia.sdk.MMConnectionState
import com.matrixmedia.sdk.MMException
import com.matrixmedia.sdk.MMStream
import kotlinx.coroutines.launch

/**
 * Single-activity example app demonstrating MatrixMedia SDK usage.
 *
 * Provides a minimal UI with:
 * - Room ID text input
 * - "Join" button (viewer mode)
 * - "Start" button (host mode)
 * - Stream screen with audio visualizer when connected
 *
 * Uses hardcoded dev credentials from [Config] to avoid requiring
 * a full Matrix SDK dependency.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        setContent {
            MaterialTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background
                ) {
                    MainScreen()
                }
            }
        }
    }
}

@Composable
private fun MainScreen() {
    val scope = rememberCoroutineScope()
    var roomId by remember { mutableStateOf(Config.DEFAULT_ROOM_ID) }
    var activeStream by remember { mutableStateOf<MMStream?>(null) }
    var errorMessage by remember { mutableStateOf<String?>(null) }
    var isLoading by remember { mutableStateOf(false) }

    // Create the SDK client with a dev token provider
    val client = remember {
        MMClient(
            serverUrl = Config.MM_SERVER_URL,
            tokenProvider = {
                MMAuthToken(
                    accessToken = Config.DEV_TOKEN,
                    matrixServerName = Config.MATRIX_SERVER_NAME
                )
            }
        )
    }

    if (activeStream != null) {
        StreamScreen(
            stream = activeStream!!,
            onLeave = {
                scope.launch {
                    try {
                        activeStream?.leave()
                    } catch (_: Exception) {
                        // Best-effort leave
                    }
                    activeStream = null
                }
            }
        )
    } else {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(24.dp)
        ) {
            Text(
                text = "MatrixMedia SDK Example",
                style = MaterialTheme.typography.headlineMedium
            )

            Spacer(modifier = Modifier.height(24.dp))

            OutlinedTextField(
                value = roomId,
                onValueChange = { roomId = it },
                label = { Text("Room ID") },
                placeholder = { Text("!abc:example.org") },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true
            )

            Spacer(modifier = Modifier.height(16.dp))

            Button(
                onClick = {
                    isLoading = true
                    errorMessage = null
                    scope.launch {
                        try {
                            val stream = client.joinStream(roomId)
                            activeStream = stream
                        } catch (e: MMException) {
                            errorMessage = e.message
                        } catch (e: Exception) {
                            errorMessage = e.message ?: "Unknown error"
                        } finally {
                            isLoading = false
                        }
                    }
                },
                modifier = Modifier.fillMaxWidth(),
                enabled = !isLoading && roomId.isNotBlank()
            ) {
                Text(if (isLoading) "Connecting..." else "Join Stream")
            }

            Spacer(modifier = Modifier.height(8.dp))

            OutlinedButton(
                onClick = {
                    isLoading = true
                    errorMessage = null
                    scope.launch {
                        try {
                            val stream = client.startStream(roomId)
                            activeStream = stream
                        } catch (e: MMException) {
                            errorMessage = e.message
                        } catch (e: Exception) {
                            errorMessage = e.message ?: "Unknown error"
                        } finally {
                            isLoading = false
                        }
                    }
                },
                modifier = Modifier.fillMaxWidth(),
                enabled = !isLoading && roomId.isNotBlank()
            ) {
                Text(if (isLoading) "Starting..." else "Start Stream")
            }

            errorMessage?.let { error ->
                Spacer(modifier = Modifier.height(16.dp))
                Text(
                    text = "Error: $error",
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium
                )
            }
        }
    }
}
