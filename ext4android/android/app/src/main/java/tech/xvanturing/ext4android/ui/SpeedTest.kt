package tech.xvanturing.ext4android.ui

import android.content.Context
import android.provider.DocumentsContract
import android.text.format.Formatter
import java.io.FileInputStream
import java.nio.ByteBuffer
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

/**
 * Speed of a mounted volume as other apps see it: a file written and read
 * back through the documents provider (ContentResolver streams on proxy
 * descriptors), checked and deleted; then the disk read directly, without
 * the file system and the provider. A read-only volume only gets the
 * direct read.
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
        }
    }
    val direct = Native.deviceReadSpeed(volume.id, DEVICE_MIB)
    out.append(context.getString(R.string.speed_device, DEVICE_MIB, direct))
    return out.toString()
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
        var readNanos = 0L
        val buf = ByteArray(MIB)
        cr.openFileDescriptor(file, "r")!!.use { pfd ->
            FileInputStream(pfd.fileDescriptor).use { input ->
                for (i in 0 until mib) {
                    val start = System.nanoTime()
                    var n = 0
                    while (n < MIB) {
                        val r = input.read(buf, n, MIB - n)
                        if (r < 0) break
                        n += r
                    }
                    readNanos += System.nanoTime() - start
                    stamp(block, i)
                    if (n != MIB || !buf.contentEquals(block)) good = false
                }
                if (input.read() != -1) good = false
            }
        }
        val reads = requests.reads.get()
        val readBytes = requests.readBytes.get()
        val readNative = requests.readNanos.get()
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
        )
    } finally {
        runCatching { DocumentsContract.deleteDocument(cr, file) }
    }
}

private fun stamp(block: ByteArray, i: Int) {
    ByteBuffer.wrap(block).putInt(0, i)
}
