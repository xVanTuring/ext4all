package tech.xvanturing.ext4android.jni

/**
 * Entry points of libext4android.so (ext4android/crates/ext4-jni).
 *
 * Paths inside a volume are encoded strings: ext4 names are arbitrary
 * bytes, so `%`, control characters and bytes that are not UTF-8 appear as
 * `%XX`; components are joined with `/` and the root is "". File system
 * errors are thrown as [java.io.FileNotFoundException] (missing, not a
 * directory) or [Ext4Exception] (with the Linux errno).
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

    /**
     * Opens the regular file at [path] for a descriptor, emptied first with
     * [truncate]: `[ino, size]`. Every open is paired with [closeFile].
     */
    @JvmStatic
    external fun openFile(volume: Int, path: String, truncate: Boolean): LongArray

    /** A descriptor from [openFile] was closed; [written] commits the changes. */
    @JvmStatic
    external fun closeFile(volume: Int, ino: Int, written: Boolean)

    /** Reads up to [len] bytes at [offset] of inode [ino] into [buf]; 0 at the end of the file. */
    @JvmStatic
    external fun read(volume: Int, ino: Int, offset: Long, buf: ByteArray, len: Int): Int

    /** Writes [len] bytes of [buf] at [offset] of inode [ino]. */
    @JvmStatic
    external fun write(volume: Int, ino: Int, offset: Long, buf: ByteArray, len: Int)

    @JvmStatic
    external fun fileSize(volume: Int, ino: Int): Long

    /** fsync: commits what was written so far. */
    @JvmStatic
    external fun syncVolume(volume: Int)

    /**
     * Creates a file or directory named [name] in the directory at [parent];
     * a taken name gets a number. One record of [Records.entries].
     */
    @JvmStatic
    external fun createDocument(volume: Int, parent: String, name: String, directory: Boolean): ByteArray

    /** Deletes a document; a directory with everything in it. */
    @JvmStatic
    external fun deleteDocument(volume: Int, path: String)

    /** Renames in place; returns the new path. */
    @JvmStatic
    external fun renameDocument(volume: Int, path: String, name: String): String

    /** Moves into the directory at [target]; returns the new path. */
    @JvmStatic
    external fun moveDocument(volume: Int, path: String, target: String): String

    /** Copies into the directory at [target] (a taken name gets a number); returns the copy's path. */
    @JvmStatic
    external fun copyDocument(volume: Int, path: String, target: String): String
}
