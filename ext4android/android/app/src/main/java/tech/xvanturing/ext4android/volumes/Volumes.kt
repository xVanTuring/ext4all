package tech.xvanturing.ext4android.volumes

import android.content.Context
import android.provider.DocumentsContract
import java.io.File
import tech.xvanturing.ext4android.jni.Native
import tech.xvanturing.ext4android.jni.Records
import tech.xvanturing.ext4android.jni.VolumeInfo

/** A mounted volume: its number in libext4android and its root in the documents provider. */
class MountedVolume(
    val id: Int,
    /** Root ID of the documents provider: the file system UUID. */
    val rootId: String,
    /** What it is mounted from, for people (an image path, a USB disk). */
    val source: String,
)

/** The volumes mounted in this process. Changes notify the documents provider's roots. */
object Volumes {
    const val AUTHORITY = "tech.xvanturing.ext4android.documents"

    private val mounted = LinkedHashMap<String, MountedVolume>()

    fun all(): List<MountedVolume> = synchronized(mounted) { mounted.values.toList() }

    fun byRoot(rootId: String): MountedVolume? = synchronized(mounted) { mounted[rootId] }

    fun info(volume: MountedVolume): VolumeInfo = Records.volumeInfo(Native.volumeInfo(volume.id))

    /** Mounts the ext4 volume of an image file; throws IOException. */
    fun mountImage(context: Context, image: File, readOnly: Boolean): MountedVolume {
        val id = Native.mountImage(image.path, readOnly)
        val uuid = runCatching { Records.volumeInfo(Native.volumeInfo(id)).uuidString }
            .getOrElse { e ->
                Native.unmount(id)
                throw e
            }
        val volume = synchronized(mounted) {
            // the same image or a copy of a disk mounted twice
            var rootId = uuid
            var n = 2
            while (rootId in mounted) {
                rootId = "$uuid-${n++}"
            }
            MountedVolume(id, rootId, image.path).also { mounted[rootId] = it }
        }
        notifyRoots(context)
        return volume
    }

    fun unmount(context: Context, volume: MountedVolume) {
        synchronized(mounted) { mounted.remove(volume.rootId) }
        notifyRoots(context)
        Native.unmount(volume.id)
    }

    private fun notifyRoots(context: Context) {
        context.contentResolver.notifyChange(DocumentsContract.buildRootsUri(AUTHORITY), null)
    }
}
