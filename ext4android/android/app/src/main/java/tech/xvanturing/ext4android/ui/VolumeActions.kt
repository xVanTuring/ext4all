package tech.xvanturing.ext4android.ui

import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import tech.xvanturing.ext4android.R
import tech.xvanturing.ext4android.provider.DocumentIds
import tech.xvanturing.ext4android.volumes.MountedVolume
import tech.xvanturing.ext4android.volumes.Volumes

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
 * Checks on a mounted volume as other apps see it: pick a file in the
 * system picker and open it with another app; read and write speed; on a
 * writable volume, every kind of change through the documents provider.
 * [run] runs a check and shows its text; [show] shows a text.
 */
@Composable
fun VolumeActions(
    volume: MountedVolume,
    enabled: Boolean,
    run: (suspend () -> String) -> Unit,
    show: (String) -> Unit,
) {
    val context = LocalContext.current
    val picker = remember { OpenDocumentAt() }
    val opener = rememberLauncherForActivityResult(picker) { uri ->
        if (uri != null) {
            show(openWithOtherApp(context, uri))
        }
    }
    Button(enabled = enabled, onClick = {
        picker.initial = DocumentsContract.buildDocumentUri(Volumes.AUTHORITY, DocumentIds.root(volume.rootId))
        opener.launch(arrayOf("*/*"))
    }) { Text(stringResource(R.string.image_pick_open)) }
    OutlinedButton(enabled = enabled, onClick = {
        run { withContext(Dispatchers.IO) { speedTest(context, volume) } }
    }) { Text(stringResource(R.string.volume_speed)) }
    val writable = remember(volume) { runCatching { !Volumes.info(volume).readOnly }.getOrDefault(false) }
    if (writable) {
        OutlinedButton(enabled = enabled, onClick = {
            run { withContext(Dispatchers.IO) { providerCheck(context, volume) } }
        }) { Text(stringResource(R.string.image_check_writes)) }
    }
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
