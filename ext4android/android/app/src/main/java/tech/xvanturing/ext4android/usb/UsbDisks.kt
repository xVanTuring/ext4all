package tech.xvanturing.ext4android.usb

import android.content.Context
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbManager
import android.util.Log
import java.util.concurrent.Executors
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.withContext
import tech.xvanturing.ext4android.R
import tech.xvanturing.ext4android.jni.Native
import tech.xvanturing.ext4android.volumes.MountedVolume
import tech.xvanturing.ext4android.volumes.Volumes

/** What the app is doing with a USB disk. */
sealed interface DiskState {
    data object Mounting : DiskState

    class Mounted(val name: String, val volumes: List<MountedVolume>) : DiskState

    data object Ejecting : DiskState

    /** Unmounted and released: it can be unplugged. */
    data object Ejected : DiskState

    class Failed(val message: String) : DiskState
}

/**
 * The USB disks with mounted volumes. A disk's connection stays open, its
 * interface claimed, while its volumes are mounted. Changes run one at a
 * time on their own thread, in the order they are asked for; MountService
 * asks for them and keeps the process in the foreground meanwhile.
 */
object UsbDisks {
    private const val TAG = "ext4android"

    private class Session(
        val storage: MassStorage,
        val connection: UsbDeviceConnection,
        val volumes: List<MountedVolume>,
    )

    private val worker = Executors.newSingleThreadExecutor { Thread(it, "usb-disks") }.asCoroutineDispatcher()

    /** Only used on [worker]. */
    private val sessions = HashMap<String, Session>()

    private val _state = MutableStateFlow<Map<String, DiskState>>(emptyMap())

    /** By device name (UsbDevice.getDeviceName). */
    val state: StateFlow<Map<String, DiskState>> = _state

    private fun set(device: String, s: DiskState?) {
        _state.update { if (s == null) it - device else it + (device to s) }
    }

    /** Whether any disk is mounted. */
    fun anyMounted(): Boolean = _state.value.values.any { it is DiskState.Mounted }

    /**
     * Claims the disk's interface and mounts every ext4 volume on it. The
     * app must hold the USB permission for it.
     */
    suspend fun mount(context: Context, storage: MassStorage, readOnly: Boolean) = withContext(worker) {
        val key = storage.device.deviceName
        if (key in sessions) {
            return@withContext
        }
        set(key, DiskState.Mounting)
        val manager = context.getSystemService(UsbManager::class.java)
        val connection = manager.openDevice(storage.device)
        if (connection == null) {
            set(key, DiskState.Failed(context.getString(R.string.usb_open_failed)))
            return@withContext
        }
        // force: detach the kernel's usb-storage driver from the interface
        if (!connection.claimInterface(storage.iface, true)) {
            connection.close()
            set(key, DiskState.Failed(context.getString(R.string.usb_claim_failed)))
            return@withContext
        }
        // alternate setting 0: a UAS enclosure may have been left on 1
        connection.setInterface(storage.iface)
        val ids = try {
            Native.mountUsb(
                connection.fileDescriptor,
                storage.iface.id,
                storage.endpointIn.address,
                storage.endpointOut.address,
                readOnly,
            )
        } catch (e: Exception) {
            Log.w(TAG, "mounting ${storage.name} failed", e)
            release(connection, storage)
            set(key, DiskState.Failed(e.message ?: e.javaClass.simpleName))
            return@withContext
        }
        val volumes = Volumes.register(context, ids, storage.name)
        if (volumes.isEmpty()) {
            release(connection, storage)
            set(key, DiskState.Failed(context.getString(R.string.usb_no_volume)))
            return@withContext
        }
        sessions[key] = Session(storage, connection, volumes)
        set(key, DiskState.Mounted(storage.name, volumes))
    }

    /**
     * Safe removal: commits and unmounts every volume of the disk, then
     * gives the device back to the system. The disk's name when it was
     * mounted.
     */
    suspend fun eject(context: Context, device: String): String? = withContext(worker) {
        val session = sessions.remove(device) ?: return@withContext null
        set(device, DiskState.Ejecting)
        var failure: Exception? = null
        for (volume in session.volumes) {
            try {
                Volumes.unmount(context, volume)
            } catch (e: Exception) {
                Log.w(TAG, "unmounting ${volume.rootId} failed", e)
                failure = failure ?: e
            }
        }
        release(session.connection, session.storage)
        set(
            device,
            failure?.let {
                DiskState.Failed(context.getString(R.string.usb_eject_failed, it.message ?: it.javaClass.simpleName))
            } ?: DiskState.Ejected,
        )
        session.storage.name
    }

    /**
     * The disk was unplugged: its volumes are forgotten without writing
     * anything more. Returns the disk's name if it had volumes mounted
     * read-write (their latest changes may be lost).
     */
    suspend fun detached(context: Context, device: String): String? = withContext(worker) {
        val session = sessions.remove(device)
        set(device, null)
        if (session == null) {
            return@withContext null
        }
        var writable = false
        for (volume in session.volumes) {
            writable = writable || runCatching { !Volumes.info(volume).readOnly }.getOrDefault(false)
            runCatching { Volumes.abandon(context, volume) }
                .onFailure { Log.w(TAG, "abandoning ${volume.rootId} failed", it) }
        }
        session.connection.close()
        if (writable) session.storage.name else null
    }

    /** Forgets the outcome (ejected, failed) shown for a disk that was unplugged. */
    fun forget(device: String) {
        _state.update { states ->
            when (states[device]) {
                DiskState.Ejected, is DiskState.Failed -> states - device
                else -> states
            }
        }
    }

    private fun release(connection: UsbDeviceConnection, storage: MassStorage) {
        connection.releaseInterface(storage.iface)
        connection.close()
    }
}
