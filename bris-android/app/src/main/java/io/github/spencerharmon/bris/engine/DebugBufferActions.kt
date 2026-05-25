package io.github.spencerharmon.bris.engine

import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import androidx.documentfile.provider.DocumentFile
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File
import java.io.OutputStream
import java.util.UUID

/**
 * Operator-facing "save the debug buffer" action.
 *
 * Mirrors the entire on-device `<files>/debug-capture/` tree
 * (frames, index, pbris log) into a fresh `bris-debug-<id>/`
 * subdirectory under the operator's chosen Storage Access
 * Framework tree URI. Unlike the prior fix-gated path through
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
     * Copy the entire buffer to the chosen SAF tree URI.
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
                val bundle = tree.createDirectory(bundleName)
                    ?: return@withContext SaveResult.Failed("Could not create $bundleName.")

                val root = buffer.rootDir()
                val sources = enumerateSources(root)
                var frameCount = 0
                var bytesWritten = 0L

                if (sources.frameFiles.isNotEmpty()) {
                    val framesDir = bundle.createDirectory("frames")
                        ?: return@withContext SaveResult.Failed("Could not create frames/.")
                    for (f in sources.frameFiles) {
                        val mime = when (f.extension.lowercase()) {
                            "pgm" -> "image/x-portable-graymap"
                            "json" -> "application/json"
                            else -> "application/octet-stream"
                        }
                        val dest = framesDir.createFile(mime, f.name)
                            ?: return@withContext SaveResult.Failed("Could not create ${f.name}.")
                        bytesWritten += streamCopy(context, f, dest.uri)
                        if (f.extension.equals("pgm", ignoreCase = true)) frameCount++
                    }
                }

                for (src in sources.topLevelFiles) {
                    val mime = when (src.name) {
                        "index.jsonl" -> "application/json"
                        "pbris.log" -> "text/plain"
                        else -> "application/octet-stream"
                    }
                    val dest = bundle.createFile(mime, src.name)
                        ?: return@withContext SaveResult.Failed("Could not create ${src.name}.")
                    bytesWritten += streamCopy(context, src, dest.uri)
                }

                val display = destinationDisplayName(tree, savedTreeUri)
                SaveResult.Ok(
                    destinationDisplay = display,
                    frameCount = frameCount,
                    bytes = bytesWritten,
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

    private fun streamCopy(context: Context, src: File, destUri: Uri): Long {
        val resolver = context.contentResolver
        src.inputStream().use { input ->
            val out: OutputStream = resolver.openOutputStream(destUri, "w")
                ?: throw java.io.IOException("Cannot open $destUri")
            out.use { input.copyTo(it) }
        }
        return src.length()
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
        val topFiles = listOf("index.jsonl", "pbris.log")
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
