package tech.xvanturing.ext4android.ui

import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import tech.xvanturing.ext4android.provider.DocumentIds
import tech.xvanturing.ext4android.volumes.MountedVolume
import tech.xvanturing.ext4android.volumes.Volumes

private const val BIG = 3 shl 20

/** Contents of the check's file at [offset]. */
private fun pattern(offset: Int): Byte = ((offset * 7) % 253).toByte()

/**
 * Milestone M2 on a device: changes through the documents provider with the
 * same calls other apps make (DocumentsContract, ContentResolver streams),
 * each step checked. Returns one line per step; stops at the first failure.
 */
fun providerCheck(context: Context, volume: MountedVolume): String {
    val cr = context.contentResolver
    val root = DocumentsContract.buildDocumentUri(Volumes.AUTHORITY, DocumentIds.root(volume.rootId))
    val log = StringBuilder()
    fun step(name: String, block: () -> String) {
        val start = System.nanoTime()
        val detail = block()
        log.append("✓ ").append(name)
        log.append(" (").append((System.nanoTime() - start) / 1_000_000).append(" ms)")
        if (detail.isNotEmpty()) log.append(": ").append(detail)
        log.append('\n')
    }
    var dir: Uri? = null
    try {
        lateinit var file: Uri
        step("create folder") {
            dir = DocumentsContract.createDocument(cr, root, Document.MIME_TYPE_DIR, "检查 check")
                ?: error("no folder created")
            DocumentsContract.getDocumentId(dir)
        }
        val folder = dir!!
        step("create file") {
            file = DocumentsContract.createDocument(cr, folder, "text/plain", "hello") ?: error("no file created")
            DocumentsContract.getDocumentId(file)
        }
        step("write 3 MiB (wt)") {
            val data = ByteArray(BIG) { pattern(it) }
            cr.openOutputStream(file, "wt")!!.use { it.write(data) }
            ""
        }
        step("read back") {
            val back = cr.openInputStream(file)!!.use { it.readBytes() }
            check(back.size == BIG) { "read ${back.size} bytes" }
            check(back.indices.all { back[it] == pattern(it) }) { "data differs" }
            "${back.size} bytes match"
        }
        step("overwrite shorter (w truncates too)") {
            cr.openOutputStream(file, "w")!!.use { it.write("shorter".toByteArray()) }
            val back = cr.openInputStream(file)!!.use { it.readBytes() }
            check(String(back) == "shorter") { "now ${back.size} bytes" }
            ""
        }
        step("overwrite in place (rw keeps the rest)") {
            cr.openFileDescriptor(file, "rw")!!.use { pfd ->
                java.io.FileOutputStream(pfd.fileDescriptor).use { it.write("SH".toByteArray()) }
            }
            val back = String(cr.openInputStream(file)!!.use { it.readBytes() })
            check(back == "SHorter") { "now \"$back\"" }
            ""
        }
        step("overwrite shorter (wt)") {
            cr.openOutputStream(file, "wt")!!.use { it.write("short".toByteArray()) }
            val back = cr.openInputStream(file)!!.use { it.readBytes() }
            check(String(back) == "short") { "now ${back.size} bytes" }
            ""
        }
        step("append (wa)") {
            cr.openOutputStream(file, "wa")!!.use { it.write(" + more".toByteArray()) }
            val back = String(cr.openInputStream(file)!!.use { it.readBytes() })
            check(back == "short + more") { "now \"$back\"" }
            ""
        }
        step("rename") {
            file = DocumentsContract.renameDocument(cr, file, "改名 renamed.txt") ?: file
            DocumentsContract.getDocumentId(file)
        }
        lateinit var copy: Uri
        step("copy to the root") {
            copy = DocumentsContract.copyDocument(cr, file, root) ?: error("no copy")
            DocumentsContract.getDocumentId(copy)
        }
        lateinit var sub: Uri
        step("move the copy into a new folder") {
            sub = DocumentsContract.createDocument(cr, folder, Document.MIME_TYPE_DIR, "sub") ?: error("no folder")
            copy = DocumentsContract.moveDocument(cr, copy, root, sub) ?: error("not moved")
            val back = String(cr.openInputStream(copy)!!.use { it.readBytes() })
            check(back == "short + more") { "moved copy holds \"$back\"" }
            DocumentsContract.getDocumentId(copy)
        }
        step("list the folder") {
            val names = children(cr, folder)
            check(names == listOf("sub", "改名 renamed.txt")) { "$names" }
            names.joinToString()
        }
        step("delete the folder with its contents") {
            check(DocumentsContract.deleteDocument(cr, folder)) { "not deleted" }
            dir = null
            check("检查 check" !in children(cr, root)) { "still listed" }
            ""
        }
        log.append("all steps passed")
    } catch (e: Exception) {
        log.append("✗ ").append(e.javaClass.simpleName).append(": ").append(e.message)
        dir?.let { runCatching { DocumentsContract.deleteDocument(cr, it) } }
    }
    return log.toString()
}

private fun children(cr: ContentResolver, parent: Uri): List<String> {
    val uri = DocumentsContract.buildChildDocumentsUri(parent.authority, DocumentsContract.getDocumentId(parent))
    return cr.query(uri, arrayOf(Document.COLUMN_DISPLAY_NAME), null, null, null)?.use { c ->
        buildList { while (c.moveToNext()) add(c.getString(0)) }.sorted()
    } ?: emptyList()
}
