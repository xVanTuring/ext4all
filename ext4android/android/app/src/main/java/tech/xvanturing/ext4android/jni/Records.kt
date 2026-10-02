package tech.xvanturing.ext4android.jni

import java.nio.ByteBuffer
import java.nio.ByteOrder

/** A file or directory of a volume (Rust `docs::Entry`). */
class Entry(
    /** Encoded name: the last component of the document's path. */
    val name: String,
    val displayName: String,
    val isDirectory: Boolean,
    val ino: Int,
    val size: Long,
    val modifiedMs: Long,
    val permissions: Int,
)

/** Facts about a mounted volume (Rust `volumes::info`). */
class VolumeInfo(
    val label: String,
    val uuid: ByteArray,
    val totalBytes: Long,
    val availableBytes: Long,
    val readOnly: Boolean,
) {
    /** As blkid and Linux show it, e.g. 0f3c5a0e-4d1b-4c8a-9a07-2b1d4c5e6f70. */
    val uuidString: String
        get() = uuid.joinToString("") { "%02x".format(it) }
            .let { "${it.substring(0, 8)}-${it.substring(8, 12)}-${it.substring(12, 16)}-${it.substring(16, 20)}-${it.substring(20)}" }
}

/** Decoding of the little-endian records libext4android returns. */
object Records {
    fun entries(bytes: ByteArray): List<Entry> {
        val b = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
        val out = ArrayList<Entry>()
        while (b.hasRemaining()) {
            val name = b.string()
            val displayName = b.string()
            val kind = b.get().toInt()
            val ino = b.int
            val size = b.long
            val modified = b.long
            val permissions = b.short.toInt() and 0xFFFF
            out += Entry(name, displayName, kind == KIND_DIR, ino, size, modified, permissions)
        }
        return out
    }

    fun volumeInfo(bytes: ByteArray): VolumeInfo {
        val b = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
        val label = b.string()
        val uuid = ByteArray(16).also { b.get(it) }
        val total = b.long
        val available = b.long
        val readOnly = b.get().toInt() != 0
        return VolumeInfo(label, uuid, total, available, readOnly)
    }

    private const val KIND_DIR = 2

    private fun ByteBuffer.string(): String {
        val n = short.toInt() and 0xFFFF
        val a = ByteArray(n)
        get(a)
        return String(a, Charsets.UTF_8)
    }
}
