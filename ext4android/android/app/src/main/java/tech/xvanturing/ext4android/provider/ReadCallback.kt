package tech.xvanturing.ext4android.provider

import android.os.HandlerThread
import android.os.ProxyFileDescriptorCallback
import android.system.ErrnoException
import android.system.OsConstants
import android.util.Log
import java.io.IOException
import tech.xvanturing.ext4android.jni.Native

/**
 * Serves reads of a file descriptor handed to another app
 * (StorageManager.openProxyFileDescriptor) from a file of a mounted volume.
 * Runs on its own [thread], which ends when the descriptor is closed.
 */
class ReadCallback(
    private val volume: Int,
    private val ino: Int,
    private val size: Long,
    private val thread: HandlerThread,
) : ProxyFileDescriptorCallback() {
    override fun onGetSize(): Long = size

    override fun onRead(offset: Long, size: Int, data: ByteArray): Int =
        try {
            Native.read(volume, ino, offset, data, size)
        } catch (e: IOException) {
            Log.w(TAG, "read of inode $ino at $offset failed", e)
            throw ErrnoException("read", OsConstants.EIO)
        }

    override fun onRelease() {
        thread.quitSafely()
    }

    private companion object {
        const val TAG = "ext4android"
    }
}
