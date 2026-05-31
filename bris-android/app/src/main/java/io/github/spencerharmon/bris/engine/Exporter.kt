package io.github.spencerharmon.bris.engine

import android.content.Context
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * Consolidated export directory for everything the operator
 * wants to pull off the device with `adb pull` (or MTP, or
 * the system Files app).
 *
 * Lives at:
 *
 *   /sdcard/Android/data/io.github.spencerharmon.bris/files/exports/
 *     <yyyy>/<mm>/<dd>/<kind>-<ulid>/
 *       … same on-disk layout as the collector's submission
 *         directory: manifest.json + media/ + per-kind extras.
 *
 * Source data already lives in per-feature directories
 * (sights/, calibration/, debug-capture/) — this class just
 * **copies** the relevant slice into a single transfer-friendly
 * tree so the operator doesn't have to know where each kind
 * was originally stored.
 *
 * Why mirror instead of moving / symlinking:
 * - Mirroring is simple, atomic per-file, and survives the
 *   source being further-edited or evicted.
 * - Android's symlink semantics on external storage are not
 *   reliable across Android versions and storage modes.
 *
 * Naming: `<kind>-<ulid>` (e.g. `fix-01HXYZ...`,
 * `calibration-01HXYZ...`, `debug-01HXYZ...`). The ULID is
 * generated at export time, not inherited from the source —
 * the operator might export the same source twice (e.g.
 * after capturing more frames), and we want each export to
 * be its own self-contained directory.
 *
 * Cleanup: never auto-deletes. Operator manages via a future
 * "Clear all exports" affordance in settings (not yet wired).
 */
class Exporter(private val rootDir: File) {

    init {
        rootDir.mkdirs()
    }

    /** Where exports live; surface for the UI to display. */
    fun rootPath(): String = rootDir.absolutePath

    /**
     * Mirror a sight-log entry into the exports tree.
     *
     * Source: `<external-files>/sights/<capture-id>/`
     * Destination: `<exports>/<yyyy>/<mm>/<dd>/fix-<new-ulid>/`
     *
     * Returns the destination directory. Throws on I/O error.
     */
    fun exportSightEntry(sightDir: File): File {
        require(sightDir.isDirectory) { "sightDir must be a directory: $sightDir" }
        val dest = newExportDir("fix")
        copyTree(sightDir, dest)
        return dest
    }

    /**
     * Mirror a calibration session into the exports tree.
     *
     * Source: `<external-files>/calibration/<session-ulid>/`
     * Destination: `<exports>/<yyyy>/<mm>/<dd>/calibration-<new-ulid>/`
     *
     * Returns the destination directory.
     */
    fun exportCalibrationSession(sessionDir: File): File {
        require(sessionDir.isDirectory) { "sessionDir must be a directory: $sessionDir" }
        val dest = newExportDir("calibration")
        copyTree(sessionDir, dest)
        return dest
    }

    /**
     * Snapshot the rolling debug-capture buffer's most-recent
     * `limit` entries into a self-contained export.
     *
     * Source: `<external-files>/debug-capture/`
     * Destination: `<exports>/<yyyy>/<mm>/<dd>/debug-<new-ulid>/`
     *
     * The export is copy-on-snapshot: the rolling buffer's LRU
     * eviction continues independently after the export
     * completes.
     */
    fun exportDebugCapture(buffer: DebugCaptureBuffer, limit: Int): File {
        val dest = newExportDir("debug")
        File(dest, "media").mkdirs()
        val entries = buffer.recentEntries(limit)
        for (e in entries) {
            copyFile(e.framePath, File(dest, "media/${e.framePath.name}"))
            copyFile(e.snapshotPath, File(dest, "media/${e.snapshotPath.name}"))
        }
        // Pull pbris.log if the buffer's parent dir has one.
        val pbris = File(buffer.let { rootField(it) }, "pbris.log")
        if (pbris.exists()) copyFile(pbris, File(dest, "media/pbris.log"))
        return dest
    }

    private fun newExportDir(kind: String): File {
        val now = Date()
        val day = SimpleDateFormat("yyyy/MM/dd", Locale.US).format(now)
        val id = ulidLike()
        val dir = File(rootDir, "$day/$kind-$id")
        dir.mkdirs()
        return dir
    }

    private fun copyTree(src: File, dst: File) {
        if (!src.exists()) return
        if (src.isFile) {
            copyFile(src, dst)
            return
        }
        dst.mkdirs()
        for (child in src.listFiles().orEmpty()) {
            copyTree(child, File(dst, child.name))
        }
    }

    private fun copyFile(src: File, dst: File) {
        dst.parentFile?.mkdirs()
        src.inputStream().use { input ->
            dst.outputStream().use { out -> input.copyTo(out) }
        }
    }

    /** Reach into the buffer for its rootDir; the field is
     *  private but the buffer doesn't expose a getter today.
     *  Keeping this scoped to Exporter so consumers don't need
     *  to know the layout. */
    @Suppress("SwallowedException")
    private fun rootField(buffer: DebugCaptureBuffer): File = try {
        val f = buffer.javaClass.getDeclaredField("rootDir")
        f.isAccessible = true
        f.get(buffer) as File
    } catch (_: Exception) {
        // Buffer's rootDir is conventionally `<files>/debug-capture/`
        // but we'd rather not hardcode if reflection fails.
        // Returning a non-existent file makes the pbris.log copy a no-op.
        File("/dev/null")
    }

    companion object {
        /** Construct rooted at `<external-files>/exports/`. */
        fun forApp(context: Context): Exporter {
            val root = context.getExternalFilesDir(null) ?: context.filesDir
            return Exporter(File(root, "exports"))
        }
    }
}

private fun ulidLike(): String {
    val ms = System.currentTimeMillis()
    val r = java.util.UUID.randomUUID().leastSignificantBits
    return "%013x%016x".format(ms, r)
}
