package io.github.spencerharmon.bris.engine

import android.content.Context
import android.net.Uri
import androidx.documentfile.provider.DocumentFile
import java.io.File
import java.io.OutputStream
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.zip.ZipEntry
import java.util.zip.ZipOutputStream

/**
 * Settings **Share sessions** action.
 *
 * Zips the entire `<external-files>/sessions/` tree into a
 * single archive written to a SAF tree URI the operator picked.
 *
 * Internal layout of the resulting zip is canonical, so a
 * recipient runs `unzip -n <zip> -d <corpus-root>/` and gets
 * a corpus tree merged in idempotently. No file-name
 * mangling, no flat layout, no per-capture nesting.
 *
 * Pure-JVM where the SAF doesn't intervene: the only Android
 * dependency is creating the destination `OutputStream` via
 * the SAF tree URI. The zip-build helper itself takes a
 * source directory and an [OutputStream] and is testable.
 */
object ShareSessionsAction {

    /**
     * Write the share-zip to a fresh file under [treeUri].
     * Filename is `bris-sessions-<UTC-yyyymmdd-hhmmss>.zip`.
     * Returns the chosen filename for status display.
     *
     * @throws IllegalStateException if the tree URI isn't
     *   writable or sessions root is missing.
     */
    fun shareTo(context: Context, treeUri: Uri): String {
        val sessionsRoot = sessionsRoot(context)
        check(sessionsRoot.isDirectory) {
            "No sessions/ directory at ${sessionsRoot.absolutePath}"
        }
        val tree = DocumentFile.fromTreeUri(context, treeUri)
            ?: error("Couldn't open tree URI $treeUri")
        val name = "bris-sessions-${TS.format(Instant.now())}.zip"
        val dest = tree.createFile("application/zip", name)
            ?: error("Couldn't create $name under the chosen tree")
        val out = context.contentResolver.openOutputStream(dest.uri)
            ?: error("Couldn't open output stream for $name")
        out.use { writeZip(sessionsRoot, it) }
        return name
    }

    /** The root the share action zips. */
    fun sessionsRoot(context: Context): File {
        val externalRoot = context.getExternalFilesDir(null) ?: context.filesDir
        return File(externalRoot, "sessions")
    }

    /**
     * Zip-build helper. Pure-JVM. Walks [sourceRoot] and
     * writes entries to [out] with internal paths rooted at
     * `sessions/`.
     */
    fun writeZip(sourceRoot: File, out: OutputStream) {
        val zip = ZipOutputStream(out)
        try {
            val rootName = "sessions"
            zip.putNextEntry(ZipEntry("$rootName/"))
            zip.closeEntry()
            sourceRoot.walkTopDown()
                .filter { it != sourceRoot }
                .forEach { f ->
                    val rel = f.relativeTo(sourceRoot).path.replace(File.separatorChar, '/')
                    val entryName = if (f.isDirectory) "$rootName/$rel/" else "$rootName/$rel"
                    zip.putNextEntry(ZipEntry(entryName))
                    if (!f.isDirectory) {
                        f.inputStream().use { it.copyTo(zip) }
                    }
                    zip.closeEntry()
                }
        } finally {
            zip.finish()
            zip.close()
        }
    }

    private val TS: DateTimeFormatter =
        DateTimeFormatter.ofPattern("yyyyMMdd-HHmmss").withZone(ZoneId.of("UTC"))
}
