package tech.xvanturing.ext4android.provider

import android.os.HandlerThread
import android.os.ProxyFileDescriptorCallback
import android.system.ErrnoException
import android.system.OsConstants
import android.util.Log
import java.io.FileNotFoundException
import java.io.IOException
import tech.xvanturing.ext4android.jni.Ext4Exception
import tech.xvanturing.ext4android.jni.Native

/**
 * Serves a file descriptor handed to another app
 * (StorageManager.openProxyFileDescriptor) from a file of a mounted volume.
 * All calls come on its own [thread], which ends when the descriptor is
 * closed; closing commits what was written.
 *
 * A proxy descriptor only takes an access mode, so [append] (mode "wa") is
 * done here: every write goes to the current end of the file.
 */
class FileCallback(
    private val volume: Int,
    private val ino: Int,
    private val append: Boolean,
    private val thread: HandlerThread,
) : ProxyFileDescriptorCallback() {
    private var written = false

    override fun onGetSize(): Long = call("size") { Native.fileSize(volume, ino) }

    override fun onRead(offset: Long, size: Int, data: ByteArray): Int =
        call("read") { Native.read(volume, ino, offset, data, size) }

    override fun onWrite(offset: Long, size: Int, data: ByteArray): Int =
        call("write") {
            val at = if (append) Native.fileSize(volume, ino) else offset
            Native.write(volume, ino, at, data, size)
            written = true
            size
        }

    override fun onFsync() {
        call("fsync") { Native.syncVolume(volume) }
    }

    override fun onRelease() {
        try {
            Native.closeFile(volume, ino, written)
        } catch (e: IOException) {
            Log.w(TAG, "closing inode $ino failed", e)
        } finally {
            thread.quitSafely()
        }
    }

    private inline fun <T> call(op: String, block: () -> T): T =
        try {
            block()
        } catch (e: Ext4Exception) {
            Log.w(TAG, "$op of inode $ino failed", e)
            throw ErrnoException(op, e.errno)
        } catch (e: FileNotFoundException) {
            throw ErrnoException(op, OsConstants.ENOENT)
        } catch (e: IOException) {
            Log.w(TAG, "$op of inode $ino failed", e)
            throw ErrnoException(op, OsConstants.EIO)
        }

    private companion object {
        const val TAG = "ext4android"
    }
}
