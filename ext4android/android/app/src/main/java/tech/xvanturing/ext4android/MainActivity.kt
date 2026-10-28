package tech.xvanturing.ext4android

import android.content.Intent
import android.hardware.usb.UsbManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import tech.xvanturing.ext4android.service.MountService
import tech.xvanturing.ext4android.ui.AppTheme
import tech.xvanturing.ext4android.ui.HomeScreen
import tech.xvanturing.ext4android.usb.massStorageOf
import tech.xvanturing.ext4android.usb.usbDevice

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            AppTheme {
                HomeScreen()
            }
        }
        if (savedInstanceState == null) {
            mountAttached(intent)
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        mountAttached(intent)
    }

    /**
     * Opened for a disk just plugged in (the USB_DEVICE_ATTACHED filter):
     * the user allowed this app to use it, so its volumes are mounted
     * read-write.
     */
    private fun mountAttached(intent: Intent?) {
        if (intent?.action != UsbManager.ACTION_USB_DEVICE_ATTACHED) {
            return
        }
        val device = intent.usbDevice() ?: return
        val manager = getSystemService(UsbManager::class.java)
        // a disk mounted already is left as it is
        if (massStorageOf(device) != null && manager.hasPermission(device)) {
            MountService.mount(this, device, readOnly = false)
        }
    }
}
