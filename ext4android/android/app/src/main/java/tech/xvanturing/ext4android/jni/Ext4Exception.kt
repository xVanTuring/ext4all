package tech.xvanturing.ext4android.jni

import android.system.OsConstants
import java.io.IOException

/**
 * A file system error from libext4android with its Linux errno. Thrown by
 * the native code with the message "<errno> <description>".
 */
class Ext4Exception(raw: String) : IOException(raw.substringAfter(' ', raw)) {
    val errno: Int = raw.substringBefore(' ').toIntOrNull() ?: OsConstants.EIO
}
