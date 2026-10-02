package tech.xvanturing.ext4android.ui

import android.content.Context
import android.hardware.usb.UsbManager
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import tech.xvanturing.ext4android.R
import tech.xvanturing.ext4android.usb.ProbeResult
import tech.xvanturing.ext4android.usb.findMassStorage
import tech.xvanturing.ext4android.usb.probe
import tech.xvanturing.ext4android.usb.requestPermission

/** Experiment M0: list USB disks and probe one through the app's own USB access. */
@Composable
fun UsbSection() {
    val context = LocalContext.current
    val manager = remember { context.getSystemService(UsbManager::class.java) }
    val scope = rememberCoroutineScope()
    var devices by remember { mutableStateOf(findMassStorage(manager)) }
    var probing by remember { mutableStateOf<String?>(null) }
    var results by remember { mutableStateOf(mapOf<String, String>()) }

    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text(stringResource(R.string.usb_title), style = MaterialTheme.typography.titleMedium)
        OutlinedButton(onClick = { devices = findMassStorage(manager) }) {
            Text(stringResource(R.string.refresh))
        }
        if (devices.isEmpty()) {
            Text(stringResource(R.string.usb_none))
        }
        for (storage in devices) {
            val key = storage.device.deviceName
            Card(modifier = Modifier.fillMaxWidth()) {
                Column(
                    modifier = Modifier.padding(12.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    Text(storage.name, style = MaterialTheme.typography.titleSmall)
                    Text(
                        stringResource(
                            R.string.usb_ids,
                            "%04X".format(storage.device.vendorId),
                            "%04X".format(storage.device.productId),
                            key,
                        ),
                        style = MaterialTheme.typography.bodySmall,
                    )
                    Button(
                        enabled = probing == null,
                        onClick = {
                            probing = key
                            scope.launch {
                                val text = if (requestPermission(context, manager, storage.device)) {
                                    withContext(Dispatchers.IO) { describe(context, probe(manager, storage)) }
                                } else {
                                    context.getString(R.string.usb_permission_denied)
                                }
                                results = results + (key to text)
                                probing = null
                            }
                        },
                    ) {
                        Text(stringResource(if (probing == key) R.string.usb_probing else R.string.usb_probe))
                    }
                    results[key]?.let {
                        SelectionContainer {
                            Text(it, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                        }
                    }
                }
            }
        }
    }
}

private fun describe(context: Context, r: ProbeResult): String = when {
    !r.opened -> context.getString(R.string.usb_open_failed)
    !r.claimed -> context.getString(R.string.usb_claim_failed)
    else -> buildString {
        appendLine(context.getString(R.string.usb_claimed))
        if (!r.bulkOnlySelected) {
            appendLine(context.getString(R.string.usb_set_interface_failed))
        }
        append(r.report)
    }
}
