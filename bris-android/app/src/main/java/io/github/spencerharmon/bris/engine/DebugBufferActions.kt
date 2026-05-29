package io.github.spencerharmon.bris.engine

import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import androidx.documentfile.provider.DocumentFile
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.BufferedOutputStream
import java.io.File
import java.util.UUID
import java.util.zip.ZipEntry
import java.util.zip.ZipOutputStream

/**
 * Operator-facing "save the debug buffer" action.
 *
 * Writes the entire on-device `<files>/debug-capture/` tree
 * (frames, index, pbris log) as a single
 * `bris-debug-<id>.zip` under the operator's chosen Storage
 * Access Framework tree URI. The zip preserves the original
 * directory layout internally (`bris-debug-<id>/frames/...`,
 * `bris-debug-<id>/index.jsonl`, `bris-debug-<id>/pbris.log`).
 * Unlike the prior fix-gated path through
 * `Exporter.exportDebugCapture`, this works whether or not the
 * session produced a fix \u2014 the corpus-capture flow needs to
 * exfiltrate the buffer for scenes that *don't* solve, which
 * is the whole reason for the buffer existing.
 *
 * No cap is applied: the local-save path uploads nothing, so
 * the collector's MAX_FRAMES_PER_SUBMISSION limit does not
 * apply. The cap remains on the (stubbed) collector path.
 */
object DebugBufferActions {

    /**
     * Copy the entire buffer as a single zip into the chosen
     * SAF tree URI.
     *
     * If [savedTreeUri] is `null`, returns [SaveResult.NeedLocation]
     * so the caller can launch [android.content.Intent.ACTION_OPEN_DOCUMENT_TREE],
     * persist the URI via `takePersistableUriPermission`, and
     * retry.
     */
    suspend fun saveAll(
        context: Context,
        buffer: DebugCaptureBuffer,
        savedTreeUri: Uri?,
        prepareManifest: ((bundleDir: File, bundleId: String) -> Unit)? = null,
    ): SaveResult {
        if (savedTreeUri == null) return SaveResult.NeedLocation
        val tree = DocumentFile.fromTreeUri(context, savedTreeUri)
            ?: return SaveResult.Failed("Cannot open chosen folder.")
        // Intentionally no `tree.canWrite()` precheck: certain
        // Downloads / SD-card providers report false from the
        // tree-URI metadata probe even when writes succeed. The
        // real failure signal is `createDirectory` /
        // `createFile` returning null below.

        return withContext(Dispatchers.IO) {
            try {
                val bundleName = "bris-debug-${ulidLike()}"
                val zipName = "$bundleName.zip"
                val zipFile = tree.createFile("application/zip", zipName)
                    ?: return@withContext SaveResult.Failed("Could not create $zipName.")

                val root = buffer.rootDir()
                // Give the caller a chance to write `bundle.json`
                // (and any other top-level metadata) before we
                // enumerate the sources for the archive. Done
                // inside the IO dispatcher so manifest writing
                // does not block the UI thread.
                prepareManifest?.invoke(root, bundleName)
                val sources = enumerateSources(root)
                val resolver = context.contentResolver
                val out = resolver.openOutputStream(zipFile.uri, "w")
                    ?: return@withContext SaveResult.Failed("Cannot open ${zipFile.uri}")
                val frameCount = writeZipBundle(out, bundleName, sources)

                val zipSize = zipFile.length()
                val display = destinationDisplayName(tree, savedTreeUri)
                SaveResult.Ok(
                    destinationDisplay = display,
                    frameCount = frameCount,
                    bytes = zipSize,
                )
            } catch (e: Exception) {
                SaveResult.Failed(e.message ?: "Save failed.")
            }
        }
    }

    private fun destinationDisplayName(tree: DocumentFile, uri: Uri): String {
        tree.name?.takeIf { it.isNotBlank() }?.let { return it }
        // `DocumentsContract.getTreeDocumentId` typically
        // returns something like "primary:Download" for
        // standard providers, which is more meaningful to the
        // operator than the bundle ULID. Fall back to the
        // URI's last path segment if even that's null.
        return try {
            DocumentsContract.getTreeDocumentId(uri)
        } catch (_: IllegalArgumentException) {
            null
        } ?: uri.lastPathSegment ?: uri.toString()
    }

    /**
     * Stream [sources] into [out] as a zip with entries
     * prefixed by [bundleName]. Closes [out] when done.
     * Exposed (internal) for unit testing the archive layout
     * without SAF.
     */
    internal fun writeZipBundle(
        out: java.io.OutputStream,
        bundleName: String,
        sources: SourceList,
    ): Int {
        var frameCount = 0
        ZipOutputStream(BufferedOutputStream(out)).use { zip ->
            val buf = ByteArray(64 * 1024)
            for (f in sources.frameFiles) {
                writeEntry(zip, "$bundleName/frames/${f.name}", f, buf)
                if (f.extension.equals("pgm", ignoreCase = true)) frameCount++
            }
            for (src in sources.topLevelFiles) {
                writeEntry(zip, "$bundleName/${src.name}", src, buf)
            }
        }
        return frameCount
    }

    private fun writeEntry(zip: ZipOutputStream, name: String, src: File, buf: ByteArray) {
        zip.putNextEntry(ZipEntry(name))
        src.inputStream().use { input ->
            while (true) {
                val n = input.read(buf)
                if (n < 0) break
                zip.write(buf, 0, n)
            }
        }
        zip.closeEntry()
    }

    private fun ulidLike(): String {
        val ms = System.currentTimeMillis()
        val r = UUID.randomUUID().leastSignificantBits
        return "%013x%016x".format(ms, r)
    }

    /**
     * Enumerate everything the local-save path will copy out
     * of the on-device buffer root, ordered for stable
     * output. Exposed for unit testing the file-walking
     * contract independently of SAF.
     */
    fun enumerateSources(bufferRoot: File): SourceList {
        val framesDir = File(bufferRoot, "frames")
        val frameFiles = framesDir.takeIf { it.isDirectory }
            ?.listFiles().orEmpty()
            .filter { it.isFile }
            .sortedBy { it.name }
        val pgmCount = frameFiles.count { it.extension.equals("pgm", ignoreCase = true) }
        val topFiles = listOf("bundle.json", "index.jsonl", "pbris.log")
            .map { File(bufferRoot, it) }
            .filter { it.isFile }
        return SourceList(
            frameFiles = frameFiles,
            topLevelFiles = topFiles,
            pgmFrameCount = pgmCount,
            totalBytes = (frameFiles + topFiles).sumOf { it.length() },
        )
    }

    /** Result of [enumerateSources]; what the SAF copy will write. */
    data class SourceList(
        val frameFiles: List<File>,
        val topLevelFiles: List<File>,
        val pgmFrameCount: Int,
        val totalBytes: Long,
    )
}

/** Outcome of [DebugBufferActions.saveAll]. */
sealed interface SaveResult {
    /** No SAF URI configured; caller must launch the picker and retry. */
    object NeedLocation : SaveResult
    /** Success. Counts reflect what was actually written. */
    data class Ok(
        val destinationDisplay: String,
        val frameCount: Int,
        val bytes: Long,
    ) : SaveResult
    /** Soft failure; the caller surfaces [message] in a snackbar. */
    data class Failed(val message: String) : SaveResult
}
