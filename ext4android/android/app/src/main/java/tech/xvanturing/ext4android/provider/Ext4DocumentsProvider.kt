package tech.xvanturing.ext4android.provider

import android.database.Cursor
import android.database.MatrixCursor
import android.os.CancellationSignal
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.os.storage.StorageManager
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.text.format.Formatter
import android.webkit.MimeTypeMap
import java.io.FileNotFoundException
import java.io.IOException
import tech.xvanturing.ext4android.R
import tech.xvanturing.ext4android.jni.Entry
import tech.xvanturing.ext4android.jni.Native
import tech.xvanturing.ext4android.jni.Records
import tech.xvanturing.ext4android.volumes.MountedVolume
import tech.xvanturing.ext4android.volumes.Volumes

/**
 * The mounted ext4 volumes for other apps, through the system file picker
 * and the Storage Access Framework. Read-only for now (milestone M1).
 */
class Ext4DocumentsProvider : DocumentsProvider() {
    override fun onCreate(): Boolean = true

    private val ctx get() = requireNotNull(context)

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val cursor = MatrixCursor(projection ?: ROOT_COLUMNS)
        for (volume in Volumes.all()) {
            val info = runCatching { Volumes.info(volume) }.getOrNull() ?: continue
            cursor.newRow().apply {
                add(Root.COLUMN_ROOT_ID, volume.rootId)
                add(Root.COLUMN_DOCUMENT_ID, DocumentIds.root(volume.rootId))
                add(Root.COLUMN_TITLE, info.label.ifEmpty { ctx.getString(R.string.root_untitled) })
                add(
                    Root.COLUMN_SUMMARY,
                    ctx.getString(R.string.root_summary, Formatter.formatShortFileSize(ctx, info.availableBytes)),
                )
                add(Root.COLUMN_FLAGS, Root.FLAG_LOCAL_ONLY or Root.FLAG_SUPPORTS_IS_CHILD)
                add(Root.COLUMN_ICON, R.drawable.ic_root)
                add(Root.COLUMN_AVAILABLE_BYTES, info.availableBytes)
                add(Root.COLUMN_CAPACITY_BYTES, info.totalBytes)
            }
        }
        cursor.setNotificationUri(ctx.contentResolver, DocumentsContract.buildRootsUri(Volumes.AUTHORITY))
        return cursor
    }

    override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor {
        val (volume, path) = locate(documentId)
        val entry = fs { Records.entries(Native.stat(volume.id, path)).single() }
        val cursor = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        addRow(cursor, documentId, entry, volume, isRoot = path.isEmpty())
        return cursor
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val (volume, path) = locate(parentDocumentId)
        val entries = fs { Records.entries(Native.list(volume.id, path)) }
        val cursor = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        for (entry in entries) {
            addRow(cursor, DocumentIds.child(parentDocumentId, entry.name), entry, volume, isRoot = false)
        }
        cursor.setNotificationUri(
            ctx.contentResolver,
            DocumentsContract.buildChildDocumentsUri(Volumes.AUTHORITY, parentDocumentId),
        )
        return cursor
    }

    override fun openDocument(documentId: String, mode: String, signal: CancellationSignal?): ParcelFileDescriptor {
        if (mode != "r") {
            throw FileNotFoundException("read-only for now: $documentId opened with \"$mode\"")
        }
        val (volume, path) = locate(documentId)
        val (ino, size) = fs { Native.openFile(volume.id, path) }.let { it[0].toInt() to it[1] }
        val thread = HandlerThread("ext4-read-$ino").apply { start() }
        val storage = ctx.getSystemService(StorageManager::class.java)
        return try {
            storage.openProxyFileDescriptor(
                ParcelFileDescriptor.MODE_READ_ONLY,
                ReadCallback(volume.id, ino, size, thread),
                Handler(thread.looper),
            )
        } catch (e: IOException) {
            thread.quitSafely()
            throw FileNotFoundException("cannot open $documentId: ${e.message}")
        }
    }

    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean =
        DocumentIds.isDescendant(parentDocumentId, documentId)

    private fun locate(documentId: String): Pair<MountedVolume, String> {
        val (rootId, path) = DocumentIds.parse(documentId)
            ?: throw FileNotFoundException("not a document of this provider: $documentId")
        val volume = Volumes.byRoot(rootId) ?: throw FileNotFoundException("volume not mounted: $rootId")
        return volume to path
    }

    private fun addRow(cursor: MatrixCursor, documentId: String, entry: Entry, volume: MountedVolume, isRoot: Boolean) {
        cursor.newRow().apply {
            add(Document.COLUMN_DOCUMENT_ID, documentId)
            add(
                Document.COLUMN_DISPLAY_NAME,
                if (isRoot) rootTitle(volume) else entry.displayName,
            )
            add(Document.COLUMN_MIME_TYPE, mimeType(entry))
            add(Document.COLUMN_SIZE, if (entry.isDirectory) null else entry.size)
            add(Document.COLUMN_LAST_MODIFIED, entry.modifiedMs)
            add(Document.COLUMN_FLAGS, 0)
        }
    }

    private fun rootTitle(volume: MountedVolume): String =
        runCatching { Volumes.info(volume).label }.getOrNull()?.ifEmpty { null }
            ?: ctx.getString(R.string.root_untitled)

    private companion object {
        val ROOT_COLUMNS = arrayOf(
            Root.COLUMN_ROOT_ID,
            Root.COLUMN_DOCUMENT_ID,
            Root.COLUMN_TITLE,
            Root.COLUMN_SUMMARY,
            Root.COLUMN_FLAGS,
            Root.COLUMN_ICON,
            Root.COLUMN_AVAILABLE_BYTES,
            Root.COLUMN_CAPACITY_BYTES,
        )
        val DOCUMENT_COLUMNS = arrayOf(
            Document.COLUMN_DOCUMENT_ID,
            Document.COLUMN_DISPLAY_NAME,
            Document.COLUMN_MIME_TYPE,
            Document.COLUMN_SIZE,
            Document.COLUMN_LAST_MODIFIED,
            Document.COLUMN_FLAGS,
        )

        fun mimeType(entry: Entry): String {
            if (entry.isDirectory) {
                return Document.MIME_TYPE_DIR
            }
            val ext = entry.displayName.substringAfterLast('.', "").lowercase()
            return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
        }

        /**
         * Only FileNotFoundException may leave a DocumentsProvider method:
         * other checked exceptions would end the process on the binder thread.
         */
        inline fun <T> fs(block: () -> T): T =
            try {
                block()
            } catch (e: FileNotFoundException) {
                throw e
            } catch (e: IOException) {
                throw FileNotFoundException(e.message).apply { initCause(e) }
            }
    }
}
