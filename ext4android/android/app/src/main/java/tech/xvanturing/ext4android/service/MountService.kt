package tech.xvanturing.ext4android.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.IBinder
import android.text.format.Formatter
import android.util.Log
import android.widget.Toast
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import kotlinx.coroutines.launch
import tech.xvanturing.ext4android.MainActivity
import tech.xvanturing.ext4android.R
import tech.xvanturing.ext4android.usb.DiskState
import tech.xvanturing.ext4android.usb.UsbDisks
import tech.xvanturing.ext4android.usb.massStorageOf
import tech.xvanturing.ext4android.usb.usbDevice
import tech.xvanturing.ext4android.volumes.Volumes

/**
 * Runs in the foreground while USB disks are being mounted or are mounted,
 * so the system keeps the process (and the volumes in it) alive. Its
 * notification lists the volumes and offers safe removal; an unplugged
 * disk's volumes are dropped at once.
 */
class MountService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)

    /** Requests not finished yet; all on the main thread. */
    private var pending = 0

    private val detachReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val device = intent.usbDevice() ?: return
            request {
                UsbDisks.detached(this@MountService, device.deviceName)?.let(::warnRemoved)
            }
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_MOUNTED, getString(R.string.channel_mounted), NotificationManager.IMPORTANCE_LOW),
        )
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_REMOVED, getString(R.string.channel_removed), NotificationManager.IMPORTANCE_DEFAULT),
        )
        val filter = IntentFilter(UsbManager.ACTION_USB_DEVICE_DETACHED)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(detachReceiver, filter, RECEIVER_NOT_EXPORTED)
        } else {
            registerReceiver(detachReceiver, filter)
        }
        UsbDisks.state.onEach { updateNotification() }.launchIn(scope)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // first: a service started with startForegroundService has only seconds
        try {
            startForeground(NOTIFICATION_ID, buildNotification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        } catch (e: RuntimeException) {
            // e.g. no USB permission (a prerequisite of the connectedDevice type)
            Log.e(TAG, "cannot run in the foreground", e)
            stopIfIdle()
            return START_NOT_STICKY
        }
        when (intent?.action) {
            ACTION_MOUNT -> {
                val storage = intent.usbDevice()?.let(::massStorageOf)
                val readOnly = intent.getBooleanExtra(EXTRA_READ_ONLY, false)
                request { storage?.let { UsbDisks.mount(this, it, readOnly) } }
            }
            ACTION_EJECT -> {
                val device = intent.getStringExtra(EXTRA_DEVICE_NAME)
                request {
                    val name = device?.let { UsbDisks.eject(this, it) }
                    if (name != null && UsbDisks.state.value[device] == DiskState.Ejected) {
                        Toast.makeText(this, getString(R.string.toast_ejected, name), Toast.LENGTH_LONG).show()
                    }
                }
            }
            // restarted after the process ended: nothing is mounted any more
            else -> stopIfIdle()
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        unregisterReceiver(detachReceiver)
        scope.cancel()
        super.onDestroy()
    }

    /** Runs [work] (UsbDisks calls) and stops the service when nothing is left mounted. */
    private fun request(work: suspend () -> Unit) {
        pending++
        scope.launch {
            try {
                work()
            } finally {
                pending--
                stopIfIdle()
            }
        }
    }

    private fun stopIfIdle() {
        if (pending == 0 && !UsbDisks.anyMounted()) {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
    }

    private fun updateNotification() {
        if (pending > 0 || UsbDisks.anyMounted()) {
            getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, buildNotification())
        }
    }

    private fun buildNotification(): Notification {
        val disks = UsbDisks.state.value
        val mounted = disks.filterValues { it is DiskState.Mounted }.mapValues { it.value as DiskState.Mounted }
        val volumes = mounted.values.flatMap { it.volumes }
        val lines = volumes.map { volume ->
            val info = runCatching { Volumes.info(volume) }.getOrNull()
            if (info == null) {
                volume.source
            } else {
                getString(
                    R.string.notif_volume,
                    info.label.ifEmpty { getString(R.string.root_untitled) },
                    getString(if (info.readOnly) R.string.mode_read_only else R.string.mode_read_write),
                    Formatter.formatShortFileSize(this, info.availableBytes),
                )
            }
        }
        val title = if (volumes.isEmpty()) {
            getString(R.string.notif_mounting)
        } else {
            resources.getQuantityString(R.plurals.notif_mounted, volumes.size, volumes.size)
        }
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = Notification.Builder(this, CHANNEL_MOUNTED)
            .setSmallIcon(R.drawable.ic_root)
            .setContentTitle(title)
            .setContentText(lines.joinToString(" · "))
            .setStyle(Notification.BigTextStyle().bigText(lines.joinToString("\n")))
            .setContentIntent(open)
            .setOngoing(true)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // shown at once, not after the usual few seconds' delay
            builder.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
        }
        // one button per disk, up to the three a notification shows
        for ((index, entry) in mounted.entries.take(3).withIndex()) {
            val (device, disk) = entry
            val label = if (mounted.size == 1) {
                getString(R.string.usb_eject)
            } else {
                getString(R.string.notif_eject_named, disk.name)
            }
            val eject = PendingIntent.getService(
                this,
                index + 1,
                ejectIntent(this, device),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            builder.addAction(Notification.Action.Builder(null, label, eject).build())
        }
        return builder.build()
    }

    private fun warnRemoved(name: String) {
        val notification = Notification.Builder(this, CHANNEL_REMOVED)
            .setSmallIcon(R.drawable.ic_root)
            .setContentTitle(getString(R.string.notif_removed_title))
            .setContentText(getString(R.string.notif_removed_text, name))
            .setStyle(Notification.BigTextStyle().bigText(getString(R.string.notif_removed_text, name)))
            .setAutoCancel(true)
            .build()
        getSystemService(NotificationManager::class.java).notify(name.hashCode(), notification)
    }

    companion object {
        private const val TAG = "ext4android"
        private const val ACTION_MOUNT = "tech.xvanturing.ext4android.action.MOUNT"
        private const val ACTION_EJECT = "tech.xvanturing.ext4android.action.EJECT"
        private const val EXTRA_READ_ONLY = "read_only"
        private const val EXTRA_DEVICE_NAME = "device_name"
        private const val CHANNEL_MOUNTED = "mounted"
        private const val CHANNEL_REMOVED = "removed"
        private const val NOTIFICATION_ID = 1

        /** Mounts the ext4 volumes of [device]; the app must hold its USB permission. */
        fun mount(context: Context, device: UsbDevice, readOnly: Boolean) {
            val intent = Intent(context, MountService::class.java)
                .setAction(ACTION_MOUNT)
                .putExtra(UsbManager.EXTRA_DEVICE, device)
                .putExtra(EXTRA_READ_ONLY, readOnly)
            context.startForegroundService(intent)
        }

        /** Safe removal of the disk [device] (a UsbDevice's device name). */
        fun eject(context: Context, device: String) {
            context.startForegroundService(ejectIntent(context, device))
        }

        private fun ejectIntent(context: Context, device: String): Intent =
            Intent(context, MountService::class.java)
                .setAction(ACTION_EJECT)
                .putExtra(EXTRA_DEVICE_NAME, device)
    }
}
