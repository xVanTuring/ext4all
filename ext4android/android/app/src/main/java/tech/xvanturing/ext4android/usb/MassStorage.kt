package tech.xvanturing.ext4android.usb

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.os.Build
import kotlin.coroutines.resume
import kotlinx.coroutines.suspendCancellableCoroutine
import tech.xvanturing.ext4android.jni.Native

/** SCSI transparent command set, Bulk-Only Transport. */
private const val SUBCLASS_SCSI = 6
private const val PROTOCOL_BULK_ONLY = 0x50

private const val ACTION_USB_PERMISSION = "tech.xvanturing.ext4android.USB_PERMISSION"

/** The Bulk-Only interface of a USB mass storage device and its two bulk endpoints. */
class MassStorage(
    val device: UsbDevice,
    val iface: UsbInterface,
    val endpointIn: UsbEndpoint,
    val endpointOut: UsbEndpoint,
) {
    val name: String
        get() = listOfNotNull(device.manufacturerName, device.productName)
            .joinToString(" ")
            .ifBlank { device.deviceName }
}

/**
 * Connected mass storage devices. A UAS enclosure lists its Bulk-Only
 * interface as alternate setting 0; that one is picked.
 */
fun findMassStorage(manager: UsbManager): List<MassStorage> =
    manager.deviceList.values.mapNotNull { device ->
        (0 until device.interfaceCount).firstNotNullOfOrNull { i ->
            val iface = device.getInterface(i)
            if (iface.interfaceClass != UsbConstants.USB_CLASS_MASS_STORAGE ||
                iface.interfaceSubclass != SUBCLASS_SCSI ||
                iface.interfaceProtocol != PROTOCOL_BULK_ONLY
            ) {
                return@firstNotNullOfOrNull null
            }
            val bulk = (0 until iface.endpointCount)
                .map(iface::getEndpoint)
                .filter { it.type == UsbConstants.USB_ENDPOINT_XFER_BULK }
            val endpointIn = bulk.firstOrNull { it.direction == UsbConstants.USB_DIR_IN }
            val endpointOut = bulk.firstOrNull { it.direction == UsbConstants.USB_DIR_OUT }
            if (endpointIn != null && endpointOut != null) {
                MassStorage(device, iface, endpointIn, endpointOut)
            } else {
                null
            }
        }
    }

/** Ask the user for access to [device]; true when granted. */
suspend fun requestPermission(context: Context, manager: UsbManager, device: UsbDevice): Boolean {
    if (manager.hasPermission(device)) {
        return true
    }
    return suspendCancellableCoroutine { cont ->
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(c: Context, intent: Intent) {
                context.unregisterReceiver(this)
                if (cont.isActive) {
                    cont.resume(intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false))
                }
            }
        }
        val filter = IntentFilter(ACTION_USB_PERMISSION)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            context.registerReceiver(receiver, filter)
        }
        cont.invokeOnCancellation { runCatching { context.unregisterReceiver(receiver) } }
        // mutable: the system adds the device and the result as extras
        val intent = Intent(ACTION_USB_PERMISSION).setPackage(context.packageName)
        val pending = PendingIntent.getBroadcast(
            context,
            0,
            intent,
            PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        manager.requestPermission(device, pending)
    }
}

class ProbeResult(
    val opened: Boolean,
    val claimed: Boolean,
    /** Alternate setting 0 selected (a UAS enclosure may have been left on 1). */
    val bulkOnlySelected: Boolean,
    val report: String?,
)

/** Experiment M0: take the interface from the system and let Rust drive it, read-only. */
fun probe(manager: UsbManager, storage: MassStorage): ProbeResult {
    val connection = manager.openDevice(storage.device)
        ?: return ProbeResult(opened = false, claimed = false, bulkOnlySelected = false, report = null)
    try {
        // force: detach the kernel's usb-storage driver from the interface
        if (!connection.claimInterface(storage.iface, true)) {
            return ProbeResult(opened = true, claimed = false, bulkOnlySelected = false, report = null)
        }
        val selected = connection.setInterface(storage.iface)
        val report = Native.usbProbe(
            connection.fileDescriptor,
            storage.iface.id,
            storage.endpointIn.address,
            storage.endpointOut.address,
        )
        connection.releaseInterface(storage.iface)
        return ProbeResult(opened = true, claimed = true, bulkOnlySelected = selected, report = report)
    } finally {
        connection.close()
    }
}
