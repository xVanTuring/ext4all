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
        val volume = add(Native.mountImage(image.path, readOnly), image.path)
        notifyRoots(context)
        return volume
    }

    /**
     * Lists volumes mounted by libext4android (from [source]) in the
     * documents provider. A volume whose facts cannot be read is unmounted
     * and left out.
     */
    fun register(context: Context, ids: IntArray, source: String): List<MountedVolume> {
        val volumes = ids.toList().mapNotNull { id -> runCatching { add(id, source) }.getOrNull() }
        notifyRoots(context)
        return volumes
    }

    private fun add(id: Int, source: String): MountedVolume {
        val uuid = runCatching { Records.volumeInfo(Native.volumeInfo(id)).uuidString }
            .getOrElse { e ->
                runCatching { Native.unmount(id) }
                throw e
            }
        return synchronized(mounted) {
            // the same image or a copy of a disk mounted twice
            var rootId = uuid
            var n = 2
            while (rootId in mounted) {
                rootId = "$uuid-${n++}"
            }
            MountedVolume(id, rootId, source).also { mounted[rootId] = it }
        }
    }

    fun unmount(context: Context, volume: MountedVolume) {
        synchronized(mounted) { mounted.remove(volume.rootId) }
        notifyRoots(context)
        Native.unmount(volume.id)
    }

    /** The volume's disk is gone: forget it without writing anything more. */
    fun abandon(context: Context, volume: MountedVolume) {
        synchronized(mounted) { mounted.remove(volume.rootId) }
        notifyRoots(context)
        Native.abandon(volume.id)
    }

    private fun notifyRoots(context: Context) {
        context.contentResolver.notifyChange(DocumentsContract.buildRootsUri(AUTHORITY), null)
    }
}
