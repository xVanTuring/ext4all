package tech.xvanturing.ext4android.provider

/**
 * Document IDs are `<root ID>:<encoded path>`; the root directory of a
 * volume is `<root ID>:`. Paths are the encoded form of libext4android, so
 * a rename or move gives a document a new ID (as in the system's
 * ExternalStorageProvider).
 */
object DocumentIds {
    fun root(rootId: String): String = "$rootId:"

    fun child(parentId: String, name: String): String =
        if (parentId.endsWith(':')) parentId + name else "$parentId/$name"

    /** Root ID and encoded path, or null if [documentId] is not one of ours. */
    fun parse(documentId: String): Pair<String, String>? {
        val i = documentId.indexOf(':')
        if (i <= 0) {
            return null
        }
        return documentId.substring(0, i) to documentId.substring(i + 1)
    }

    /** Whether [documentId] is inside [parentId], at any depth. */
    fun isDescendant(parentId: String, documentId: String): Boolean {
        val (parentRoot, parentPath) = parse(parentId) ?: return false
        val (root, path) = parse(documentId) ?: return false
        return root == parentRoot && path != parentPath &&
            (parentPath.isEmpty() || path.startsWith("$parentPath/"))
    }
}
