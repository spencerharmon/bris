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
 * solver diagnostics. Lives at
 * `<app-files>/calibration/<session-ulid>/`:
 *
 *   frames/<seq>.jpg       captured checkerboard frames
 *   intrinsics.json        solved intrinsics + per-session metadata
 *   target.json            checkerboard rows/cols/square_size_mm
 *
 * Sessions are append-only; the `current` symlink-equivalent
 * is just "the most recently created session" (we don't keep a
 * pointer file — we list and pick the latest by name, which
 * sorts correctly because ULIDs are time-sortable).
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
     * Begin a new session. Returns the session directory; the
     * caller passes this into [`writeFrame`], [`writeTarget`],
     * and [`writeIntrinsics`].
     */
    fun newSession(): File {
        val id = ulid()
        val dir = File(rootDir, id).apply { mkdirs() }
        File(dir, "frames").mkdirs()
        File(dir, "frames/rejected").mkdirs()
        return dir
    }

    /** Write one captured frame as JPEG. Returns the file. */
    fun writeFrame(sessionDir: File, seq: Int, jpegBytes: ByteArray): File {
        val name = "frame_${"%04d".format(seq)}.jpg"
        val f = File(sessionDir, "frames/$name")
        f.writeBytes(jpegBytes)
        return f
    }

    /**
     * Move a captured frame into the per-session
     * `frames/rejected/` subdir.
     *
     * Used when per-capture detection reports an outcome
     * the operator chooses to discard ("no board found",
     * "wrong grid size", or any other case the operator
     * doesn't want to feed to the solver). The file is
     * preserved (not deleted) so the session remains
     * forensically reproducible — `bris-collector`
     * submissions of a calibration session can include
     * the `rejected/` directory if the operator opts in.
     *
     * Returns the new path of the moved file, or `null` if
     * the source didn't exist.
     */
    fun rejectFrame(sessionDir: File, seq: Int, reasonCode: String): File? {
        val name = "frame_${"%04d".format(seq)}.jpg"
        val src = File(sessionDir, "frames/$name")
        if (!src.exists()) return null
        val dstName = "frame_${"%04d".format(seq)}_${reasonCode}.jpg"
        val dst = File(sessionDir, "frames/rejected/$dstName")
        dst.parentFile?.mkdirs()
        // File.renameTo can fail across mount points; on
        // Android internal storage that's not a concern, but
        // we fall back to a copy+delete just in case the
        // session was moved to external storage by the
        // operator.
        if (!src.renameTo(dst)) {
            src.copyTo(dst, overwrite = true)
            src.delete()
        }
        return dst
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
        val stats = JSONObject()
            .put("tried", result.detectionStats.tried.toLong())
            .put("skipped_no_board", result.detectionStats.skippedNoBoard.toLong())
            .put("skipped_wrong_size", result.detectionStats.skippedWrongSize.toLong())
            .put("skipped_io", result.detectionStats.skippedIo.toLong())
        val issues = org.json.JSONArray()
        for (issue in result.diagnosisIssues) {
            issues.put(
                JSONObject()
                    .put("level", issue.level.name)
                    .put("code", issue.code)
                    .put("message", issue.message)
                    .put("remediation", issue.remediation),
            )
        }
        val perView = org.json.JSONArray()
        for (v in result.perViewResiduals) {
            perView.put(
                JSONObject()
                    .put("source", v.source)
                    .put("rms_px", v.rmsPx)
                    .put("max_px", v.maxPx)
                    .put("n_corners", v.nCorners.toLong()),
            )
        }
        val obj = JSONObject()
            .put("intrinsics", intr)
            .put("width", result.width.toLong())
            .put("height", result.height.toLong())
            .put("rms_px", result.rmsPx)
            .put("n_frames_used", result.nFramesUsed.toLong())
            .put("n_frames_total", result.nFramesTotal.toLong())
            .put("detection_stats", stats)
            .put("diagnosis_overall", result.diagnosisOverall.name)
            .put("diagnosis_issues", issues)
            .put("per_view_residuals", perView)
        File(sessionDir, "intrinsics.json").writeText(obj.toString())
    }

    /** Most recent session directory, or `null` if none exist. */
    fun latestSession(): File? = rootDir.listFiles()
        ?.filter { it.isDirectory }
        ?.maxByOrNull { it.name }

    /** Frames in the given session, sorted by name. */
    fun framesIn(sessionDir: File): List<File> =
        File(sessionDir, "frames").listFiles()
            ?.filter { it.isFile && it.name.endsWith(".jpg") }
            ?.sortedBy { it.name }
            ?: emptyList()

    /**
     * Load the persisted intrinsics from the latest session,
     * or `null` if none exists or the file is malformed.
     *
     * Returns the in-memory representation matching the Rust
     * `bris_ffi::FfiIntrinsics` shape so the Kotlin caller can
     * pass it straight into `Engine.pushFrame`.
     */
    fun latestIntrinsics(): PersistedIntrinsics? {
        val sess = latestSession() ?: return null
        val f = File(sess, "intrinsics.json")
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
