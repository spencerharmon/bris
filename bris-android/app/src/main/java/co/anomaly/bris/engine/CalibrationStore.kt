package co.anomaly.bris.engine

import android.content.Context
import org.json.JSONObject
import java.io.File
import uniffi.bris_ffi.FfiCalibrationResult

/**
 * On-device calibration session storage.
 *
 * One session = one camera + one resolution + N captured
 * checkerboard frames + (after solve) the intrinsics + the
 * solver diagnostics.
 *
 * **Lens-aware layout** (current):
 *
 * ```
 * <app-files>/calibration/<lens-id>/<width>x<height>/<session-ulid>/
 *   frames/<seq>.jpg
 *   intrinsics.json
 *   target.json
 * ```
 *
 * Calibration intrinsics depend on the *physical* lens and on
 * the pixel grid they were solved against. A wide-lens
 * calibration applied to telephoto frames produces silently
 * wrong altitudes; a 1280×720 calibration applied to 1920×1080
 * frames does the same. Keying the on-disk layout by both
 * dimensions makes the right thing happen by construction:
 * `latestIntrinsicsFor(lensId, w, h)` returns either an
 * exact match or `null`, and the caller falls back to
 * placeholder intrinsics with the diagnostic overlay flagging
 * "calib: PLACEHOLDER (run calibration)".
 *
 * **Legacy layout.** Pre-lens-selection sessions live directly
 * at `<root>/<session-ulid>/` without a lens prefix. They are
 * still listable via [`latestSession`] and readable via
 * [`latestIntrinsics`] for debug/inspection, but
 * [`latestIntrinsicsFor`] (the lens-aware lookup the live
 * pipeline uses) ignores them. Operators with legacy data
 * simply re-run calibration once.
 *
 * Sessions are append-only; the current session for a given
 * `(lens, resolution)` is "the most recently created", which
 * sorts correctly because session ids are time-prefixed.
 *
 * Submission flow: `PreUploadReviewScreen` reads the latest
 * session's frames + intrinsics, includes them as media in the
 * `submission_kind = "calibration"` manifest, posts to the
 * collector.
 */
class CalibrationStore(private val rootDir: File) {

    init {
        rootDir.mkdirs()
    }

    /**
     * Begin a new lens-aware session for the given lens id and
     * pixel grid. Returns the session directory; the caller
     * passes this into [`writeFrame`], [`writeTarget`], and
     * [`writeIntrinsics`].
     *
     * The directory layout encodes lens + resolution into the
     * path so accidentally cross-applying intrinsics is
     * impossible — there is no shared "latest" pointer that a
     * future load can resolve to the wrong key.
     */
    fun newSession(lensId: String, width: Int, height: Int): File {
        val id = ulid()
        val dir = File(lensDir(lensId, width, height), id).apply { mkdirs() }
        File(dir, "frames").mkdirs()
        return dir
    }

    /**
     * Legacy entry point (no lens id, no resolution). Kept
     * only so callers that haven't yet been updated continue
     * to compile; new code should use the lens-aware overload.
     */
    @Deprecated("Use newSession(lensId, width, height)")
    fun newSession(): File = newSession(LensCatalog.FALLBACK_LENS_ID, 0, 0)

    /** Write one captured frame as JPEG. Returns the file. */
    fun writeFrame(sessionDir: File, seq: Int, jpegBytes: ByteArray): File {
        val name = "frame_${"%04d".format(seq)}.jpg"
        val f = File(sessionDir, "frames/$name")
        f.writeBytes(jpegBytes)
        return f
    }

    /** Persist the checkerboard target description for the session. */
    fun writeTarget(sessionDir: File, rows: Int, cols: Int, squareSizeMm: Double) {
        val obj = JSONObject()
            .put("rows", rows)
            .put("cols", cols)
            .put("square_size_mm", squareSizeMm)
        File(sessionDir, "target.json").writeText(obj.toString())
    }

    /** Persist the solver result. */
    fun writeIntrinsics(sessionDir: File, result: FfiCalibrationResult) {
        val intr = JSONObject()
            .put("fx", result.intrinsics.fx)
            .put("fy", result.intrinsics.fy)
            .put("cx", result.intrinsics.cx)
            .put("cy", result.intrinsics.cy)
            .put("k1", result.intrinsics.k1)
            .put("k2", result.intrinsics.k2)
            .put("k3", result.intrinsics.k3)
            .put("p1", result.intrinsics.p1)
            .put("p2", result.intrinsics.p2)
        val obj = JSONObject()
            .put("intrinsics", intr)
            .put("width", result.width.toLong())
            .put("height", result.height.toLong())
            .put("rms_px", result.rmsPx)
            .put("n_frames_used", result.nFramesUsed.toLong())
            .put("n_frames_total", result.nFramesTotal.toLong())
        File(sessionDir, "intrinsics.json").writeText(obj.toString())
    }

    /**
     * Most recent session directory across the entire store
     * (any lens, any resolution, including legacy). Used by
     * inspection / submission flows that don't care about the
     * lens key.
     */
    fun latestSession(): File? = allSessionDirs().maxByOrNull { it.name }

    /**
     * Most recent session for the given `(lensId, width,
     * height)` triple, or `null` if no calibration exists for
     * that combination. This is the lookup the live pipeline
     * uses to decide whether to apply persisted intrinsics or
     * fall back to placeholders.
     */
    fun latestSessionFor(lensId: String, width: Int, height: Int): File? {
        val dir = lensDir(lensId, width, height)
        if (!dir.isDirectory) return null
        return dir.listFiles()
            ?.filter { it.isDirectory }
            ?.maxByOrNull { it.name }
    }

    /** Frames in the given session, sorted by name. */
    fun framesIn(sessionDir: File): List<File> =
        File(sessionDir, "frames").listFiles()
            ?.filter { it.isFile && it.name.endsWith(".jpg") }
            ?.sortedBy { it.name }
            ?: emptyList()

    /**
     * Load the persisted intrinsics from the latest session
     * (across all lenses), or `null` if none exists or the
     * file is malformed. Inspection-only — the live pipeline
     * uses [`latestIntrinsicsFor`] instead so it can enforce
     * the lens + resolution match.
     */
    fun latestIntrinsics(): PersistedIntrinsics? =
        latestSession()?.let(::readIntrinsics)

    /**
     * Load the persisted intrinsics for a specific lens +
     * resolution, or `null` if none exists.
     *
     * Calibration data is keyed by `(lensId, width, height)`.
     * A mismatch in either component returns `null`; the
     * caller is expected to degrade to placeholder intrinsics
     * and surface the mismatch in the diagnostic overlay.
     */
    fun latestIntrinsicsFor(lensId: String, width: Int, height: Int): PersistedIntrinsics? =
        latestSessionFor(lensId, width, height)?.let(::readIntrinsics)

    private fun readIntrinsics(sessionDir: File): PersistedIntrinsics? {
        val f = File(sessionDir, "intrinsics.json")
        if (!f.exists()) return null
        return try {
            val obj = JSONObject(f.readText())
            val intr = obj.getJSONObject("intrinsics")
            PersistedIntrinsics(
                fx = intr.getDouble("fx"),
                fy = intr.getDouble("fy"),
                cx = intr.getDouble("cx"),
                cy = intr.getDouble("cy"),
                k1 = intr.optDouble("k1", 0.0),
                k2 = intr.optDouble("k2", 0.0),
                k3 = intr.optDouble("k3", 0.0),
                p1 = intr.optDouble("p1", 0.0),
                p2 = intr.optDouble("p2", 0.0),
                width = obj.getInt("width"),
                height = obj.getInt("height"),
                rmsPx = obj.optDouble("rms_px", Double.NaN),
            )
        } catch (_: Exception) {
            null
        }
    }

    private fun lensDir(lensId: String, width: Int, height: Int): File =
        File(rootDir, "$lensId/${width}x${height}").apply { mkdirs() }

    /**
     * Best-effort enumeration of every session directory under
     * the store, including legacy sessions (no lens prefix).
     * A session is identified as "any directory containing a
     * `frames/` subdirectory or an `intrinsics.json` file"; we
     * walk at most three levels deep
     * (`<lens-id>/<resolution>/<session>/`) so this stays
     * O(N sessions).
     */
    private fun allSessionDirs(): List<File> {
        val out = mutableListOf<File>()
        val top = rootDir.listFiles() ?: return emptyList()
        for (entry in top) {
            if (!entry.isDirectory) continue
            if (looksLikeSession(entry)) {
                out += entry
                continue
            }
            // lens-id directory; recurse into <resolution>/<session>.
            val resDirs = entry.listFiles() ?: continue
            for (resDir in resDirs) {
                if (!resDir.isDirectory) continue
                val sessions = resDir.listFiles() ?: continue
                for (s in sessions) if (s.isDirectory && looksLikeSession(s)) out += s
            }
        }
        return out
    }

    private fun looksLikeSession(dir: File): Boolean =
        File(dir, "frames").isDirectory || File(dir, "intrinsics.json").isFile

    /**
     * Persisted calibration as loaded back from disk.
     *
     * The same nine intrinsics fields the Rust core takes plus
     * the resolution they were calibrated at and the solve RMS.
     * Mismatched resolution at apply time is the caller's
     * responsibility to detect (the Rust core's
     * `bris_calibrate::persist` does so for the CLI; the
     * Android side should match before constructing an
     * `FfiIntrinsics` from this).
     */
    data class PersistedIntrinsics(
        val fx: Double,
        val fy: Double,
        val cx: Double,
        val cy: Double,
        val k1: Double,
        val k2: Double,
        val k3: Double,
        val p1: Double,
        val p2: Double,
        val width: Int,
        val height: Int,
        val rmsPx: Double,
    )

    companion object {
        /** Construct rooted at `<app-files>/calibration/`. */
        fun forApp(context: Context): CalibrationStore =
            CalibrationStore(File(context.filesDir, "calibration"))
    }
}

/**
 * Tiny self-contained ULID generator. Not as rigorous as the
 * proper Crockford-base32 spec — uses lexicographically-
 * sortable hex with millisecond precision, which is good
 * enough for naming session directories.
 */
private fun ulid(): String {
    val ms = System.currentTimeMillis()
    val r = java.util.UUID.randomUUID().leastSignificantBits
    return "%013x%016x".format(ms, r)
}
