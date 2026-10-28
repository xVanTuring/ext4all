package tech.xvanturing.ext4android.ui

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbManager
import android.os.Build
import android.text.format.Formatter
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
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.collectAsState
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
import tech.xvanturing.ext4android.service.MountService
import tech.xvanturing.ext4android.usb.DiskState
import tech.xvanturing.ext4android.usb.MassStorage
import tech.xvanturing.ext4android.usb.ProbeResult
import tech.xvanturing.ext4android.usb.UsbDisks
import tech.xvanturing.ext4android.usb.findMassStorage
import tech.xvanturing.ext4android.usb.probe
import tech.xvanturing.ext4android.usb.requestPermission
import tech.xvanturing.ext4android.usb.usbDevice
import tech.xvanturing.ext4android.volumes.MountedVolume
import tech.xvanturing.ext4android.volumes.Volumes

/** The connected USB disks: mount, safe removal, and checks on their volumes. */
@Composable
fun UsbSection() {
    val context = LocalContext.current
    val manager = remember { context.getSystemService(UsbManager::class.java) }
    var devices by remember { mutableStateOf(findMassStorage(manager)) }
    val states by UsbDisks.state.collectAsState()

    // plugging and unplugging update the list
    DisposableEffect(Unit) {
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(c: Context, intent: Intent) {
                if (intent.action == UsbManager.ACTION_USB_DEVICE_DETACHED) {
                    intent.usbDevice()?.let { UsbDisks.forget(it.deviceName) }
                }
                devices = findMassStorage(manager)
            }
        }
        val filter = IntentFilter().apply {
            addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED)
            addAction(UsbManager.ACTION_USB_DEVICE_DETACHED)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            context.registerReceiver(receiver, filter)
        }
        onDispose { context.unregisterReceiver(receiver) }
    }

    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text(stringResource(R.string.usb_title), style = MaterialTheme.typography.titleMedium)
        if (devices.isEmpty()) {
            Text(stringResource(R.string.usb_none))
        }
        for (storage in devices) {
            DiskCard(storage, states[storage.device.deviceName])
        }
    }
}

@Composable
private fun DiskCard(storage: MassStorage, state: DiskState?) {
    val context = LocalContext.current
    val manager = remember { context.getSystemService(UsbManager::class.java) }
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }
    val key = storage.device.deviceName

    fun run(work: suspend () -> String) {
        busy = true
        scope.launch {
            message = try {
                work()
            } catch (e: Exception) {
                context.getString(R.string.failed, e.message ?: e.javaClass.simpleName)
            }
            busy = false
        }
    }

    fun mount(readOnly: Boolean) {
        busy = true
        message = null
        scope.launch {
            if (requestPermission(context, manager, storage.device)) {
                MountService.mount(context, storage.device, readOnly)
            } else {
                message = context.getString(R.string.usb_permission_denied)
            }
            busy = false
        }
    }

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
            when (state) {
                DiskState.Mounting -> Text(stringResource(R.string.usb_mounting))
                DiskState.Ejecting -> Text(stringResource(R.string.usb_ejecting))
                is DiskState.Mounted -> {
                    for (volume in state.volumes) {
                        Text(volumeText(context, volume))
                        VolumeActions(volume, enabled = !busy, run = ::run, show = { message = it })
                    }
                    Button(enabled = !busy, onClick = { MountService.eject(context, key) }) {
                        Text(stringResource(R.string.usb_eject))
                    }
                }
                else -> {
                    when (state) {
                        DiskState.Ejected -> Text(stringResource(R.string.usb_ejected))
                        is DiskState.Failed -> Text(stringResource(R.string.usb_mount_failed, state.message))
                        else -> {}
                    }
                    Button(enabled = !busy, onClick = { mount(readOnly = false) }) {
                        Text(stringResource(R.string.usb_mount_rw))
                    }
                    OutlinedButton(enabled = !busy, onClick = { mount(readOnly = true) }) {
                        Text(stringResource(R.string.usb_mount_ro))
                    }
                    // experiment M0: identify, list and measure without mounting
                    OutlinedButton(enabled = !busy, onClick = {
                        run {
                            if (requestPermission(context, manager, storage.device)) {
                                withContext(Dispatchers.IO) { describe(context, probe(manager, storage)) }
                            } else {
                                context.getString(R.string.usb_permission_denied)
                            }
                        }
                    }) { Text(stringResource(if (busy) R.string.usb_probing else R.string.usb_probe)) }
                }
            }
            message?.let {
                SelectionContainer {
                    Text(it, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                }
            }
        }
    }
}

private fun volumeText(context: Context, volume: MountedVolume): String {
    val info = runCatching { Volumes.info(volume) }.getOrNull() ?: return volume.rootId
    return context.getString(
        R.string.usb_volume,
        info.label.ifEmpty { context.getString(R.string.root_untitled) },
        context.getString(if (info.readOnly) R.string.mode_read_only else R.string.mode_read_write),
        Formatter.formatShortFileSize(context, info.availableBytes),
    )
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
