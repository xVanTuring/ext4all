package tech.xvanturing.ext4android.ui

import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.text.format.Formatter
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import java.io.File
import java.io.FileInputStream
import java.nio.ByteBuffer
import kotlin.random.Random
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import tech.xvanturing.ext4android.R
import tech.xvanturing.ext4android.jni.Native
import tech.xvanturing.ext4android.provider.DocumentIds
import tech.xvanturing.ext4android.volumes.MountedVolume
import tech.xvanturing.ext4android.volumes.Volumes

private const val SAMPLE_MIB = 128
private const val RANDOM_READS = 200
private const val RANDOM_READ_BYTES = 4096

/**
 * OpenDocument that starts in a given folder, in the system's own picker
 * (DocumentsUI): some vendors answer ACTION_OPEN_DOCUMENT with a picker of
 * their own that leaves out other apps' storage. vivo OriginOS 6 does so
 * even for an intent limited to the DocumentsUI package; only naming the
 * activity reaches DocumentsUI.
 */
private class OpenDocumentAt : ActivityResultContracts.OpenDocument() {
    var initial: Uri? = null

    override fun createIntent(context: Context, input: Array<String>): Intent =
        super.createIntent(context, input).apply {
            initial?.let { putExtra(DocumentsContract.EXTRA_INITIAL_URI, it) }
            documentsUi(context)?.let { component = it }
        }
}

/**
 * The picker activity of the system's DocumentsUI: the one answering
 * ACTION_OPEN_DOCUMENT_TREE also answers ACTION_OPEN_DOCUMENT.
 */
private fun documentsUi(context: Context): ComponentName? =
    Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).resolveActivity(context.packageManager)

/**
 * Debug tools for milestone M1 and experiment 3: a sample image, mounted
 * read-only and shown to other apps by the documents provider.
 */
@Composable
fun ImageSection() {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val image = remember { File(context.filesDir, "sample.img") }
    var exists by remember { mutableStateOf(image.exists()) }
    var mounted by remember { mutableStateOf(Volumes.all().firstOrNull { it.source == image.path }) }
    var busy by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }

    fun run(work: suspend () -> String) {
        busy = true
        scope.launch {
            message = try {
                work()
            } catch (e: Exception) {
                context.getString(R.string.failed, e.message ?: e.javaClass.simpleName)
            }
            exists = image.exists()
            busy = false
        }
    }

    val importer = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) {
            run { withContext(Dispatchers.IO) { importFile(context, image, uri) } }
        }
    }
    val picker = remember { OpenDocumentAt() }
    val opener = rememberLauncherForActivityResult(picker) { uri ->
        if (uri != null) {
            message = openWithOtherApp(context, uri)
        }
    }

    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text(stringResource(R.string.image_title), style = MaterialTheme.typography.titleMedium)
        val volume = mounted
        Text(
            when {
                volume != null -> mountedText(context, volume)
                exists -> stringResource(R.string.image_ready, Formatter.formatShortFileSize(context, image.length()))
                else -> stringResource(R.string.image_none)
            },
        )
        if (volume == null) {
            OutlinedButton(enabled = !busy, onClick = {
                run {
                    withContext(Dispatchers.IO) { Native.createSampleImage(image.path, SAMPLE_MIB) }
                    context.getString(R.string.image_created)
                }
            }) { Text(stringResource(R.string.image_create, SAMPLE_MIB)) }
            OutlinedButton(enabled = !busy && exists, onClick = { importer.launch(arrayOf("*/*")) }) {
                Text(stringResource(R.string.image_import))
            }
            for (readOnly in listOf(true, false)) {
                Button(enabled = !busy && exists, onClick = {
                    run {
                        mounted = withContext(Dispatchers.IO) { Volumes.mountImage(context, image, readOnly) }
                        context.getString(R.string.image_mounted_done)
                    }
                }) { Text(stringResource(if (readOnly) R.string.image_mount else R.string.image_mount_rw)) }
            }
        } else {
            Button(enabled = !busy, onClick = {
                picker.initial = DocumentsContract.buildDocumentUri(Volumes.AUTHORITY, DocumentIds.root(volume.rootId))
                opener.launch(arrayOf("*/*"))
            }) { Text(stringResource(R.string.image_pick_open)) }
            OutlinedButton(enabled = !busy, onClick = {
                run { withContext(Dispatchers.IO) { measureReads(context, volume) } }
            }) { Text(stringResource(R.string.image_speed)) }
            val writable = remember(volume) { runCatching { !Volumes.info(volume).readOnly }.getOrDefault(false) }
            if (writable) {
                OutlinedButton(enabled = !busy, onClick = {
                    run { withContext(Dispatchers.IO) { providerCheck(context, volume) } }
                }) { Text(stringResource(R.string.image_check_writes)) }
            }
            OutlinedButton(enabled = !busy, onClick = {
                run {
                    withContext(Dispatchers.IO) { Volumes.unmount(context, volume) }
                    mounted = null
                    context.getString(R.string.image_unmounted_done)
                }
            }) { Text(stringResource(R.string.image_unmount)) }
        }
        if (busy) {
            Text(stringResource(R.string.running))
        }
        message?.let {
            SelectionContainer {
                Text(it, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

private fun mountedText(context: Context, volume: MountedVolume): String {
    val info = runCatching { Volumes.info(volume) }.getOrNull()
        ?: return context.getString(R.string.image_mounted, "?", "?", "?")
    return context.getString(
        R.string.image_mounted,
        context.getString(if (info.readOnly) R.string.mode_read_only else R.string.mode_read_write),
        info.label.ifEmpty { context.getString(R.string.root_untitled) },
        Formatter.formatShortFileSize(context, info.availableBytes),
    )
}

private fun displayName(context: Context, uri: Uri): String =
    context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
        if (c.moveToFirst()) c.getString(0) else null
    } ?: uri.lastPathSegment ?: "imported"

private fun importFile(context: Context, image: File, uri: Uri): String {
    val name = displayName(context, uri)
    val pfd = context.contentResolver.openFileDescriptor(uri, "r")
        ?: return context.getString(R.string.failed, uri.toString())
    val bytes = pfd.use { Native.importIntoImage(image.path, it.fd, name) }
    return context.getString(R.string.image_imported, name, Formatter.formatShortFileSize(context, bytes))
}

private fun openWithOtherApp(context: Context, uri: Uri): String {
    val intent = Intent(Intent.ACTION_VIEW)
        .setDataAndType(uri, context.contentResolver.getType(uri))
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    return try {
        context.startActivity(Intent.createChooser(intent, null))
        uri.toString()
    } catch (e: ActivityNotFoundException) {
        context.getString(R.string.failed, e.message ?: uri.toString())
    }
}

/** `big.bin` of the sample holds `offset % 251` at every offset. */
private fun pattern(offset: Long): Byte = (offset % 251).toByte()

/**
 * Experiment 3: read `big.bin` through the documents provider (a proxy file
 * descriptor), first in one pass, then at random offsets, and check the data.
 */
private fun measureReads(context: Context, volume: MountedVolume): String {
    val uri = DocumentsContract.buildDocumentUri(Volumes.AUTHORITY, DocumentIds.child(DocumentIds.root(volume.rootId), "big.bin"))
    val pfd = context.contentResolver.openFileDescriptor(uri, "r")
        ?: return context.getString(R.string.failed, uri.toString())
    pfd.use {
        FileInputStream(it.fileDescriptor).use { input ->
            var good = true
            val buf = ByteArray(1 shl 20)
            var total = 0L
            val start = System.nanoTime()
            while (true) {
                val n = input.read(buf)
                if (n < 0) break
                for (i in 0 until n) {
                    if (buf[i] != pattern(total + i)) good = false
                }
                total += n
            }
            val sequentialMs = (System.nanoTime() - start) / 1e6

            val channel = input.channel
            val small = ByteBuffer.allocate(RANDOM_READ_BYTES)
            val random = Random(1)
            val randomStart = System.nanoTime()
            repeat(RANDOM_READS) {
                val offset = random.nextLong(0, total - RANDOM_READ_BYTES)
                small.clear()
                while (small.hasRemaining() && channel.read(small, offset + small.position()) > 0) {
                    // until the 4 KiB are in
                }
                for (i in 0 until small.position()) {
                    if (small.get(i) != pattern(offset + i)) good = false
                }
            }
            val randomMs = (System.nanoTime() - randomStart) / 1e6 / RANDOM_READS

            return context.getString(
                R.string.image_speed_result,
                Formatter.formatShortFileSize(context, total),
                total / 1e6 / (sequentialMs / 1000),
                RANDOM_READS,
                randomMs,
                context.getString(if (good) R.string.data_ok else R.string.data_bad),
            )
        }
    }
}
