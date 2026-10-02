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
import tech.xvanturing.ext4android.jni.Ext4Exception
import tech.xvanturing.ext4android.jni.Native
import tech.xvanturing.ext4android.jni.Records
import tech.xvanturing.ext4android.volumes.MountedVolume
import tech.xvanturing.ext4android.volumes.Volumes

/**
 * The mounted ext4 volumes for other apps, through the system file picker
 * and the Storage Access Framework.
 */
class Ext4DocumentsProvider : DocumentsProvider() {
    override fun onCreate(): Boolean = true

    private val ctx get() = requireNotNull(context)

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val cursor = MatrixCursor(projection ?: ROOT_COLUMNS)
        for (volume in Volumes.all()) {
            val info = runCatching { Volumes.info(volume) }.getOrNull() ?: continue
            var flags = Root.FLAG_LOCAL_ONLY or Root.FLAG_SUPPORTS_IS_CHILD
            if (!info.readOnly) {
                flags = flags or Root.FLAG_SUPPORTS_CREATE
            }
            cursor.newRow().apply {
                add(Root.COLUMN_ROOT_ID, volume.rootId)
                add(Root.COLUMN_DOCUMENT_ID, DocumentIds.root(volume.rootId))
                add(Root.COLUMN_TITLE, info.label.ifEmpty { ctx.getString(R.string.root_untitled) })
                add(
                    Root.COLUMN_SUMMARY,
                    ctx.getString(R.string.root_summary, Formatter.formatShortFileSize(ctx, info.availableBytes)),
                )
                add(Root.COLUMN_FLAGS, flags)
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
        val entry = read { Records.entries(Native.stat(volume.id, path)).single() }
        val cursor = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        addRow(cursor, documentId, entry, volume, writable(volume), isRoot = path.isEmpty())
        return cursor
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val (volume, path) = locate(parentDocumentId)
        val entries = read { Records.entries(Native.list(volume.id, path)) }
        val writable = writable(volume)
        val cursor = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        for (entry in entries) {
            addRow(cursor, DocumentIds.child(parentDocumentId, entry.name), entry, volume, writable, isRoot = false)
        }
        cursor.setNotificationUri(
            ctx.contentResolver,
            DocumentsContract.buildChildDocumentsUri(Volumes.AUTHORITY, parentDocumentId),
        )
        return cursor
    }

    override fun openDocument(documentId: String, mode: String, signal: CancellationSignal?): ParcelFileDescriptor {
        val flags = ParcelFileDescriptor.parseMode(mode)
        val access = flags and ParcelFileDescriptor.MODE_READ_WRITE
        val writing = access != ParcelFileDescriptor.MODE_READ_ONLY
        val (volume, path) = locate(documentId)
        if (writing && !writable(volume)) {
            throw FileNotFoundException("read-only volume: $documentId opened with \"$mode\"")
        }
        val truncate = writing && flags and ParcelFileDescriptor.MODE_TRUNCATE != 0
        val ino = read { Native.openFile(volume.id, path, truncate) }[0].toInt()
        val thread = HandlerThread("ext4-fd-$ino").apply { start() }
        val storage = ctx.getSystemService(StorageManager::class.java)
        return try {
            storage.openProxyFileDescriptor(access, FileCallback(volume.id, ino, thread), Handler(thread.looper))
        } catch (e: IOException) {
            thread.quitSafely()
            runCatching { Native.closeFile(volume.id, ino, false) }
            throw FileNotFoundException("cannot open $documentId: ${e.message}")
        }
    }

    override fun createDocument(parentDocumentId: String, mimeType: String, displayName: String): String {
        val (volume, path) = locate(parentDocumentId)
        val directory = mimeType == Document.MIME_TYPE_DIR
        val name = if (directory) displayName else withExtension(displayName, mimeType)
        val entry = change { Records.entries(Native.createDocument(volume.id, path, name, directory)).single() }
        notifyChildren(parentDocumentId)
        return DocumentIds.child(parentDocumentId, entry.name)
    }

    override fun deleteDocument(documentId: String) {
        val (volume, path) = locate(documentId)
        change { Native.deleteDocument(volume.id, path) }
        DocumentIds.parent(documentId)?.let(::notifyChildren)
    }

    override fun renameDocument(documentId: String, displayName: String): String? {
        val (volume, path) = locate(documentId)
        val newPath = change { Native.renameDocument(volume.id, path, displayName) }
        DocumentIds.parent(documentId)?.let(::notifyChildren)
        return if (newPath == path) null else "${volume.rootId}:$newPath"
    }

    override fun moveDocument(
        sourceDocumentId: String,
        sourceParentDocumentId: String,
        targetParentDocumentId: String,
    ): String {
        val (volume, path) = locate(sourceDocumentId)
        val (target, targetPath) = locate(targetParentDocumentId)
        if (target.id != volume.id) {
            // the system file manager falls back to copying the bytes
            throw UnsupportedOperationException("moving between volumes")
        }
        val newPath = change { Native.moveDocument(volume.id, path, targetPath) }
        notifyChildren(sourceParentDocumentId)
        notifyChildren(targetParentDocumentId)
        return "${volume.rootId}:$newPath"
    }

    override fun copyDocument(sourceDocumentId: String, targetParentDocumentId: String): String {
        val (volume, path) = locate(sourceDocumentId)
        val (target, targetPath) = locate(targetParentDocumentId)
        if (target.id != volume.id) {
            throw UnsupportedOperationException("copying between volumes")
        }
        val newPath = change { Native.copyDocument(volume.id, path, targetPath) }
        notifyChildren(targetParentDocumentId)
        return "${volume.rootId}:$newPath"
    }

    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean =
        DocumentIds.isDescendant(parentDocumentId, documentId)

    private fun locate(documentId: String): Pair<MountedVolume, String> {
        val (rootId, path) = DocumentIds.parse(documentId)
            ?: throw FileNotFoundException("not a document of this provider: $documentId")
        val volume = Volumes.byRoot(rootId) ?: throw FileNotFoundException("volume not mounted: $rootId")
        return volume to path
    }

    private fun writable(volume: MountedVolume): Boolean =
        runCatching { !Volumes.info(volume).readOnly }.getOrDefault(false)

    private fun notifyChildren(parentDocumentId: String) {
        ctx.contentResolver.notifyChange(
            DocumentsContract.buildChildDocumentsUri(Volumes.AUTHORITY, parentDocumentId),
            null,
        )
    }

    private fun addRow(
        cursor: MatrixCursor,
        documentId: String,
        entry: Entry,
        volume: MountedVolume,
        writable: Boolean,
        isRoot: Boolean,
    ) {
        var flags = 0
        if (writable) {
            flags = if (entry.isDirectory) Document.FLAG_DIR_SUPPORTS_CREATE else Document.FLAG_SUPPORTS_WRITE
            if (!isRoot) {
                flags = flags or Document.FLAG_SUPPORTS_DELETE or Document.FLAG_SUPPORTS_RENAME or
                    Document.FLAG_SUPPORTS_MOVE or Document.FLAG_SUPPORTS_COPY
            }
        }
        cursor.newRow().apply {
            add(Document.COLUMN_DOCUMENT_ID, documentId)
            add(Document.COLUMN_DISPLAY_NAME, if (isRoot) rootTitle(volume) else entry.displayName)
            add(Document.COLUMN_MIME_TYPE, mimeType(entry))
            add(Document.COLUMN_SIZE, if (entry.isDirectory) null else entry.size)
            add(Document.COLUMN_LAST_MODIFIED, entry.modifiedMs)
            add(Document.COLUMN_FLAGS, flags)
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
         * The name for a new file of [mimeType]: as given if its extension
         * already fits, else with the type's extension added (as the system's
         * own storage provider does).
         */
        fun withExtension(name: String, mimeType: String): String {
            if (mimeType == "application/octet-stream") {
                return name
            }
            val map = MimeTypeMap.getSingleton()
            val ext = name.substringAfterLast('.', "")
            if (ext.isNotEmpty() && map.getMimeTypeFromExtension(ext.lowercase()) == mimeType) {
                return name
            }
            val wanted = map.getExtensionFromMimeType(mimeType) ?: return name
            return "$name.$wanted"
        }

        /**
         * For lookups: only FileNotFoundException may leave a DocumentsProvider
         * method; other checked exceptions would end the process on the binder
         * thread.
         */
        inline fun <T> read(block: () -> T): T =
            try {
                block()
            } catch (e: FileNotFoundException) {
                throw e
            } catch (e: IOException) {
                throw FileNotFoundException(e.message).apply { initCause(e) }
            }

        /**
         * For changes: a missing document stays FileNotFoundException, other
         * failures (name taken, no space, read-only) become
         * IllegalStateException, which reaches the calling app (as with the
         * system's own storage provider).
         */
        inline fun <T> change(block: () -> T): T =
            try {
                block()
            } catch (e: FileNotFoundException) {
                throw e
            } catch (e: Ext4Exception) {
                throw IllegalStateException(e.message, e)
            } catch (e: IOException) {
                throw IllegalStateException(e.message, e)
            }
    }
}
