package com.matrixmedia.example

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.matrixmedia.sdk.MMClient
import kotlinx.coroutines.launch

/**
 * Creator self-service screen for the LUD-16 Lightning Address + an inline
 * "send a tip" form. Mirrors the iOS LightningView.
 */
@Composable
fun LightningScreen(client: MMClient, onBack: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()

    var address by remember { mutableStateOf("") }
    var loadedAddress by remember { mutableStateOf<String?>(null) }
    var status by remember { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var isLoading by remember { mutableStateOf(true) }
    var isSaving by remember { mutableStateOf(false) }

    var tipStreamId by remember { mutableStateOf("") }
    var tipSats by remember { mutableStateOf("1000") }
    var bolt11 by remember { mutableStateOf<String?>(null) }
    var isTipping by remember { mutableStateOf(false) }

    LaunchedEffect(Unit) {
        try {
            val p = client.getCreatorProfile()
            val addr = p?.get("lightning_address") as? String
            loadedAddress = addr
            address = addr ?: ""
        } catch (_: Throwable) {
            // No profile yet — leave the field blank.
        } finally {
            isLoading = false
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(24.dp)
            .verticalScroll(rememberScrollState()),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = onBack) { Text("← Back") }
            Spacer(Modifier.weight(1f))
        }

        Spacer(Modifier.height(8.dp))
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("⚡", fontSize = 24.sp)
            Spacer(Modifier.width(8.dp))
            Text("Lightning Address", style = MaterialTheme.typography.headlineSmall)
        }

        Spacer(Modifier.height(8.dp))
        Text(
            "Publish a LUD-16 Lightning Address so viewers can tip you. Donations " +
                "settle wallet-to-wallet — the operator never holds funds.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        Spacer(Modifier.height(16.dp))

        if (isLoading) {
            CircularProgressIndicator()
        } else {
            OutlinedTextField(
                value = address,
                onValueChange = { address = it.trim() },
                label = { Text("name@domain.tld") },
                placeholder = { Text("alice@phoenix.acinq.co") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
                enabled = !isSaving,
            )
            Spacer(Modifier.height(12.dp))
            Row {
                Button(
                    onClick = {
                        scope.launch {
                            isSaving = true
                            error = null
                            status = null
                            try {
                                val updated = client.updateLightningAddress(
                                    address.ifEmpty { null },
                                )
                                val addr = updated["lightning_address"] as? String
                                loadedAddress = addr
                                address = addr ?: ""
                                status = if (address.isEmpty()) {
                                    "Lightning Address cleared."
                                } else {
                                    "Saved — donations route directly to your wallet."
                                }
                            } catch (e: Throwable) {
                                error = "$e"
                            } finally {
                                isSaving = false
                            }
                        }
                    },
                    enabled = !isSaving,
                ) {
                    Text(if (isSaving) "Saving…" else "Save")
                }

                if (loadedAddress?.isNotEmpty() == true) {
                    Spacer(Modifier.width(8.dp))
                    OutlinedButton(
                        onClick = {
                            address = ""
                            scope.launch {
                                isSaving = true
                                error = null
                                try {
                                    client.updateLightningAddress(null)
                                    loadedAddress = null
                                    status = "Lightning Address cleared."
                                } catch (e: Throwable) {
                                    error = "$e"
                                } finally {
                                    isSaving = false
                                }
                            }
                        },
                        enabled = !isSaving,
                    ) { Text("Clear") }
                }
            }
        }

        status?.let {
            Spacer(Modifier.height(12.dp))
            Text(it, color = MaterialTheme.colorScheme.primary)
        }
        error?.let {
            Spacer(Modifier.height(12.dp))
            Text(it, color = MaterialTheme.colorScheme.error)
        }

        Spacer(Modifier.height(32.dp))
        HorizontalDivider()
        Spacer(Modifier.height(16.dp))

        Text("Send a Lightning tip", style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(12.dp))
        OutlinedTextField(
            value = tipStreamId,
            onValueChange = { tipStreamId = it },
            label = { Text("Stream ID") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = tipSats,
            onValueChange = { tipSats = it.filter { ch -> ch.isDigit() } },
            label = { Text("Amount (sats)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(12.dp))
        Button(
            onClick = {
                scope.launch {
                    isTipping = true
                    error = null
                    bolt11 = null
                    try {
                        val sats = tipSats.toIntOrNull() ?: 0
                        if (sats <= 0) {
                            error = "Amount must be > 0"
                        } else {
                            val res = client.sendLightningTip(tipStreamId, sats)
                            // M1 LNURL-pay: invoice.bolt11; legacy: checkout_url
                            @Suppress("UNCHECKED_CAST")
                            val invoice = res["invoice"] as? Map<String, Any?>
                            bolt11 = (invoice?.get("bolt11") as? String)
                                ?: (res["checkout_url"] as? String)?.takeIf { it.startsWith("ln") }
                            if (bolt11 == null) {
                                error = "Unexpected response: $res"
                            }
                        }
                    } catch (e: Throwable) {
                        error = "$e"
                    } finally {
                        isTipping = false
                    }
                }
            },
            enabled = !isTipping && tipStreamId.isNotBlank(),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(if (isTipping) "Generating…" else "⚡ Send tip")
        }

        bolt11?.let { invoice ->
            Spacer(Modifier.height(16.dp))
            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(8.dp))
                    .background(MaterialTheme.colorScheme.surfaceContainerHighest)
                    .padding(12.dp),
            ) {
                Text(
                    invoice,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 11.sp,
                    color = MaterialTheme.colorScheme.onSurface,
                    maxLines = 5,
                )
            }
            Spacer(Modifier.height(8.dp))
            Row {
                OutlinedButton(onClick = { copyToClipboard(ctx, invoice) }) {
                    Text("Copy")
                }
                Spacer(Modifier.width(8.dp))
                Button(onClick = { openInWallet(ctx, invoice) }) {
                    Text("Open in wallet")
                }
            }
        }

        Spacer(Modifier.height(32.dp))
    }
}

private fun copyToClipboard(context: Context, text: String) {
    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    cm.setPrimaryClip(ClipData.newPlainText("BOLT11", text))
    Toast.makeText(context, "Invoice copied", Toast.LENGTH_SHORT).show()
}

private fun openInWallet(context: Context, bolt11: String) {
    try {
        val intent = Intent(Intent.ACTION_VIEW, Uri.parse("lightning:$bolt11"))
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        context.startActivity(intent)
    } catch (_: Throwable) {
        Toast.makeText(
            context,
            "No Lightning wallet installed. Copy and paste the invoice manually.",
            Toast.LENGTH_LONG,
        ).show()
    }
}
