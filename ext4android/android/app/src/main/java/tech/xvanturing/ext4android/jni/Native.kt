package tech.xvanturing.ext4android.jni

/**
 * Entry points of libext4android.so (ext4android/crates/ext4-jni).
 *
 * Paths inside a volume are encoded strings: ext4 names are arbitrary
 * bytes, so `%`, control characters and bytes that are not UTF-8 appear as
 * `%XX`; components are joined with `/` and the root is "". File system
 * errors are thrown as [java.io.FileNotFoundException] (missing, not a
 * directory) or [java.io.IOException].
 */
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

    /** Creates or replaces the sample image (debug build). */
    @JvmStatic
    external fun createSampleImage(path: String, sizeMiB: Int)

    /** Copies the open file [fd] into the root of the unmounted image; returns the bytes copied. */
    @JvmStatic
    external fun importIntoImage(imagePath: String, fd: Int, name: String): Long

    /** Mounts the ext4 volume of an image file; returns its volume number. */
    @JvmStatic
    external fun mountImage(path: String, readOnly: Boolean): Int

    @JvmStatic
    external fun unmount(volume: Int)

    /** See [Records.volumeInfo]. */
    @JvmStatic
    external fun volumeInfo(volume: Int): ByteArray

    /** The document at [path], one record of [Records.entries]. */
    @JvmStatic
    external fun stat(volume: Int, path: String): ByteArray

    /** The documents in the directory at [path], see [Records.entries]. */
    @JvmStatic
    external fun list(volume: Int, path: String): ByteArray

    /** Inode and size of the regular file at [path]: `[ino, size]`. */
    @JvmStatic
    external fun openFile(volume: Int, path: String): LongArray

    /** Reads up to [len] bytes at [offset] of inode [ino] into [buf]; 0 at the end of the file. */
    @JvmStatic
    external fun read(volume: Int, ino: Int, offset: Long, buf: ByteArray, len: Int): Int
}
