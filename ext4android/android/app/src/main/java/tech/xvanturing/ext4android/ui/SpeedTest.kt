package tech.xvanturing.ext4android.ui

import android.content.Context
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.os.ProxyFileDescriptorCallback
import android.os.storage.StorageManager
import android.provider.DocumentsContract
import android.text.format.Formatter
import java.io.FileInputStream
import java.io.FileOutputStream
import java.nio.ByteBuffer
import kotlin.concurrent.thread
import kotlin.random.Random
import tech.xvanturing.ext4android.R
import tech.xvanturing.ext4android.jni.Native
import tech.xvanturing.ext4android.provider.DocumentIds
import tech.xvanturing.ext4android.provider.FileCallback
import tech.xvanturing.ext4android.volumes.MountedVolume
import tech.xvanturing.ext4android.volumes.Volumes

private const val MIB = 1 shl 20
private const val FILE_MIB = 256
private const val MIN_FILE_MIB = 16
private const val DEVICE_MIB = 256
private const val FS_WRITE_MIB = 64
private const val PROXY_MIB = 256
/** Write sizes compared: the documents provider's requests, and bigger ones. */
private val FS_WRITE_KIB = intArrayOf(128, 1024, 4096)

/**
 * Speed of a mounted volume as other apps see it: a file written and read
 * back through the documents provider (ContentResolver streams on proxy
 * descriptors), checked, read again through the file system alone, and
 * deleted; writes of several sizes through the file system alone; then
 * the disk read directly, and the state of its USB transfers; last, the
 * system's proxy descriptors and a pipe with nothing behind them. A
 * read-only volume gets no file tests.
 */
fun speedTest(context: Context, volume: MountedVolume): String {
    val info = Volumes.info(volume)
    val out = StringBuilder()
    if (!info.readOnly) {
        // at most half the free space
        val mib = minOf(FILE_MIB.toLong(), info.availableBytes / MIB / 2).toInt()
        if (mib < MIN_FILE_MIB) {
            out.appendLine(
                context.getString(R.string.speed_no_space, Formatter.formatShortFileSize(context, 2L * MIN_FILE_MIB * MIB)),
            )
        } else {
            out.appendLine(fileSpeed(context, volume, mib))
            out.appendLine(context.getString(R.string.speed_fs_write, minOf(FS_WRITE_MIB, mib)))
            for (kib in FS_WRITE_KIB) {
                val size = if (kib >= 1024) "${kib / 1024} MiB" else "$kib KiB"
                val speed = Native.fileWriteSpeed(volume.id, minOf(FS_WRITE_MIB, mib), kib)
                out.appendLine(context.getString(R.string.speed_fs_write_item, size, speed))
            }
        }
    }
    val direct = Native.deviceReadSpeed(volume.id, DEVICE_MIB)
    out.append(context.getString(R.string.speed_device, DEVICE_MIB, direct))
    val usb = Native.usbTransfers(volume.id)
    if (usb.size == 2) {
        out.append('\n').append(context.getString(R.string.speed_usb, usb[0] / 1024, usb[1]))
    }
    out.append('\n').append(
        context.getString(R.string.speed_proxy, proxyWriteSpeed(context), proxyReadSpeed(context)),
    )
    out.append('\n').append(context.getString(R.string.speed_pipe, pipeWriteSpeed()))
    return out.toString()
}

/** A proxy descriptor with nothing behind it: writes are thrown away, reads give what the buffer holds. */
private class NullCallback(private val size: Long, private val thread: HandlerThread) : ProxyFileDescriptorCallback() {
    override fun onGetSize(): Long = size

    override fun onRead(offset: Long, size: Int, data: ByteArray): Int =
        (this.size - offset).coerceIn(0, size.toLong()).toInt()

    override fun onWrite(offset: Long, size: Int, data: ByteArray): Int = size

    override fun onRelease() {
        thread.quitSafely()
    }
}

/** A proxy descriptor of the system, opened as the documents provider opens them. */
private fun nullProxy(context: Context, mode: Int, size: Long): ParcelFileDescriptor {
    val thread = HandlerThread("ext4-null-proxy").apply { start() }
    val storage = context.getSystemService(StorageManager::class.java)
    return storage.openProxyFileDescriptor(mode, NullCallback(size, thread), Handler(thread.looper))
}

/** MB/s writing [PROXY_MIB] MiB in 1 MiB writes to a proxy descriptor that throws them away. */
private fun proxyWriteSpeed(context: Context): Double {
    val block = ByteArray(MIB)
    val start = System.nanoTime()
    // the ParcelFileDescriptor must stay referenced: collected, it closes the descriptor
    nullProxy(context, ParcelFileDescriptor.MODE_WRITE_ONLY, 0).use { pfd ->
        FileOutputStream(pfd.fileDescriptor).use { out ->
            repeat(PROXY_MIB) { out.write(block) }
        }
    }
    return PROXY_MIB.toLong() * MIB * 1e3 / (System.nanoTime() - start)
}

/** MB/s reading [PROXY_MIB] MiB in 1 MiB reads from a proxy descriptor with nothing behind it. */
private fun proxyReadSpeed(context: Context): Double {
    val total = PROXY_MIB.toLong() * MIB
    val buf = ByteArray(MIB)
    val start = System.nanoTime()
    nullProxy(context, ParcelFileDescriptor.MODE_READ_ONLY, total).use { pfd ->
        FileInputStream(pfd.fileDescriptor).use { input ->
            while (input.read(buf) >= 0) {
                // until the end
            }
        }
    }
    return total * 1e3 / (System.nanoTime() - start)
}

/**
 * MB/s writing [PROXY_MIB] MiB in 1 MiB writes into a pipe whose other
 * end is read on another thread: what a documents provider handing out a
 * pipe for writing in order would see.
 */
private fun pipeWriteSpeed(): Double {
    val (readEnd, writeEnd) = ParcelFileDescriptor.createPipe()
    val reader = thread(name = "ext4-pipe-reader") {
        ParcelFileDescriptor.AutoCloseInputStream(readEnd).use { input ->
            val buf = ByteArray(MIB)
            while (input.read(buf) >= 0) {
                // thrown away
            }
        }
    }
    val block = ByteArray(MIB)
    val start = System.nanoTime()
    ParcelFileDescriptor.AutoCloseOutputStream(writeEnd).use { out ->
        repeat(PROXY_MIB) { out.write(block) }
    }
    reader.join()
    return PROXY_MIB.toLong() * MIB * 1e3 / (System.nanoTime() - start)
}

private fun fileSpeed(context: Context, volume: MountedVolume, mib: Int): String {
    val cr = context.contentResolver
    val root = DocumentsContract.buildDocumentUri(Volumes.AUTHORITY, DocumentIds.root(volume.rootId))
    val file = DocumentsContract.createDocument(cr, root, "application/octet-stream", "ext4android-speed-test.bin")
        ?: error("no file created")
    val requests = FileCallback.Requests
    try {
        // random bytes; every MiB starts with its number, so data read
        // from the wrong place shows
        val block = ByteArray(MIB).also { Random(7).nextBytes(it) }
        requests.reset()
        val writeStart = System.nanoTime()
        cr.openOutputStream(file, "w")!!.use { out ->
            for (i in 0 until mib) {
                stamp(block, i)
                out.write(block)
            }
        } // closing commits and flushes the disk's cache
        val writeNanos = System.nanoTime() - writeStart
        val writes = requests.writes.get()
        val writeBytes = requests.writeBytes.get()
        val writeNative = requests.writeNanos.get()

        requests.reset()
        var good = true
        val buf = ByteArray(MIB)
        // the whole loop is timed: between reads the kernel and read-ahead
        // go on reading, so timing only the reads would count too little;
        // checking a MiB costs next to nothing
        val readStart = System.nanoTime()
        cr.openFileDescriptor(file, "r")!!.use { pfd ->
            FileInputStream(pfd.fileDescriptor).use { input ->
                for (i in 0 until mib) {
                    var n = 0
                    while (n < MIB) {
                        val r = input.read(buf, n, MIB - n)
                        if (r < 0) break
                        n += r
                    }
                    stamp(block, i)
                    if (n != MIB || !buf.contentEquals(block)) good = false
                }
                if (input.read() != -1) good = false
            }
        }
        val readNanos = System.nanoTime() - readStart
        val reads = requests.reads.get()
        val readBytes = requests.readBytes.get()
        val readNative = requests.readNanos.get()
        // the same data again, without the documents provider
        val path = DocumentIds.parse(DocumentsContract.getDocumentId(file))!!.second
        val fsRead = Native.fileReadSpeed(volume.id, path)
        val total = mib.toLong() * MIB
        return context.getString(
            R.string.speed_file,
            Formatter.formatShortFileSize(context, total),
            total * 1e3 / writeNanos,
            writes,
            Formatter.formatShortFileSize(context, if (writes > 0) writeBytes / writes else 0),
            100.0 * writeNative / writeNanos,
            total * 1e3 / readNanos,
            reads,
            Formatter.formatShortFileSize(context, if (reads > 0) readBytes / reads else 0),
            100.0 * readNative / readNanos,
            context.getString(if (good) R.string.data_ok else R.string.data_bad),
        ) + "\n" + context.getString(R.string.speed_fs_read, fsRead)
    } finally {
        runCatching { DocumentsContract.deleteDocument(cr, file) }
    }
}

private fun stamp(block: ByteArray, i: Int) {
    ByteBuffer.wrap(block).putInt(0, i)
}
