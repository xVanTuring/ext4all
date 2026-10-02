package tech.xvanturing.ext4android.jni

/** Entry points of libext4android.so (rust/crates/ext4-jni). */
object Native {
    init {
        System.loadLibrary("ext4android")
    }

    @JvmStatic
    external fun version(): String

    /** Formats, writes, remounts and reads back a small in-memory volume. */
    @JvmStatic
    external fun selfTest(): String

    /**
     * Experiment M0: drives a USB disk through usbdevfs on [fd] (a
     * UsbDeviceConnection with [iface] claimed); reads only. Returns a report.
     */
    @JvmStatic
    external fun usbProbe(fd: Int, iface: Int, endpointIn: Int, endpointOut: Int): String
}
