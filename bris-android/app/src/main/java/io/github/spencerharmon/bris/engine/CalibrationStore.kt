package io.github.spencerharmon.bris.engine

import android.content.Context
import org.json.JSONObject
import uniffi.bris_ffi.FfiCalibrationResult
import java.io.File
import java.util.UUID

/**
 * On-device calibration session storage.
 *
 * Layout (unified, 2026-06):
 *
 *   <external-files>/calibration/<calibration-UUID>/
 *     calibration.json          # intrinsics + lens id + WxH
 *     frames/
 *       frame_NNNN.jpg          # checkerboard inputs
 *       rejected/
 *         frame_NNNN_<reason>.jpg
 *     target.json               # checkerboard description
 *
 * One UUID v4 per calibration session. Lens id and capture
 * resolution live as JSON fields inside `calibration.json`,
 * not as path components. The on-disk hierarchy is flat;
 * lookup by `(lensId, width, height)` walks every directory
 * and inspects the manifest.
 *
 * Legacy hierarchy (`<files>/calibration/<lensId>/<WxH>/<ulid>/`)
 * is supported read-only via a fallback resolver so existing
 * on-device calibrations from prior builds keep working.
 *
 * Built-in factory intrinsics for known device-lens-resolution
 * combinations are exposed by [`KnownIntrinsics`] and carry
 * stable UUIDs baked into the source. Captures that ran
 * against factory intrinsics stamp the corresponding factory
 * UUID in `bundle.intrinsics.source.calibration_id`.
 */
class CalibrationStore(
    private val externalRoot: File,
    private val legacyInternalRoot: File,
) {

    init {
        externalRoot.mkdirs()
    }

    /**
     * Begin a new calibration session. Generates a fresh
     * UUID v4 and returns the session directory.
     */
    fun newSession(lensId: String, width: Int, height: Int): File {
        val id = UUID.randomUUID()
        val dir = File(externalRoot, id.toString()).apply { mkdirs() }
        File(dir, "frames/rejected").mkdirs()
        // Stamp the lens+resolution in advance so a partial
        // session is identifiable before writeIntrinsics
        // finalizes the manifest.
        val stub = JSONObject()
            .put("calibration_id", id.toString())
            .put("lens_id", lensId)
            .put("width", width)
            .put("height", height)
            .put("status", "in_progress")
        File(dir, "calibration.json").writeText(stub.toString())
        return dir
    }

    /** Write one captured frame as JPEG. */
    fun writeFrame(sessionDir: File, seq: Int, jpegBytes: ByteArray): File {
        val name = "frame_${"%04d".format(seq)}.jpg"
        val f = File(sessionDir, "frames/$name")
        f.writeBytes(jpegBytes)
        return f
    }

    /**
     * Move a captured frame into the per-session
     * `frames/rejected/` subdir.
     */
    fun rejectFrame(sessionDir: File, seq: Int, reasonCode: String): File? {
        val name = "frame_${"%04d".format(seq)}.jpg"
        val src = File(sessionDir, "frames/$name")
        if (!src.exists()) return null
        val dstName = "frame_${"%04d".format(seq)}_${reasonCode}.jpg"
        val dst = File(sessionDir, "frames/rejected/$dstName")
        dst.parentFile?.mkdirs()
        if (!src.renameTo(dst)) {
            src.copyTo(dst, overwrite = true)
            src.delete()
        }
        return dst
    }

    /** Persist the checkerboard target description. */
    fun writeTarget(sessionDir: File, rows: Int, cols: Int, squareSizeMm: Double) {
        val obj = JSONObject()
            .put("rows", rows)
            .put("cols", cols)
            .put("square_size_mm", squareSizeMm)
        File(sessionDir, "target.json").writeText(obj.toString())
    }

    /**
     * Persist the solver result, rewriting `calibration.json`
     * with the full manifest (intrinsics + stats + per-view
     * residuals + diagnosis). Preserves the `calibration_id`
     * and lens/resolution stamped at `newSession`.
     */
    fun writeIntrinsics(sessionDir: File, result: FfiCalibrationResult) {
        val existing = File(sessionDir, "calibration.json").let { f ->
            if (f.isFile) runCatching { JSONObject(f.readText()) }.getOrNull() else null
        }
        val calibrationId = existing?.optString("calibration_id")
            ?.takeIf { it.isNotEmpty() }
            ?: sessionDir.name
        val lensId = existing?.optString("lens_id") ?: ""
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
            .put("calibration_id", calibrationId)
            .put("lens_id", lensId)
            .put("status", "complete")
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
        File(sessionDir, "calibration.json").writeText(obj.toString())
    }

    /**
     * Most recent session for the given `(lensId, width,
     * height)` triple across both the new flat external layout
     * AND the legacy internal hierarchy. Returns `null` if no
     * match. The live pipeline uses this to decide whether
     * to apply persisted intrinsics or fall back to a factory
     * profile / placeholder.
     */
    fun latestSessionFor(lensId: String, width: Int, height: Int): File? {
        val external = externalSessionsMatching(lensId, width, height)
            .maxByOrNull { it.lastModified() }
        if (external != null) return external
        // Legacy fallback.
        val legacyDir = File(legacyInternalRoot, "$lensId/${width}x${height}")
        if (!legacyDir.isDirectory) return null
        return legacyDir.listFiles()
            ?.filter { it.isDirectory }
            ?.maxByOrNull { it.name }
    }

    /** Frames in the given session. */
    fun framesIn(sessionDir: File): List<File> =
        File(sessionDir, "frames").listFiles()
            ?.filter { it.isFile && it.name.endsWith(".jpg") }
            ?.sortedBy { it.name }
            ?: emptyList()

    /**
     * Load the persisted intrinsics for a specific lens +
     * resolution, or `null` if none exists.
     */
    fun latestIntrinsicsFor(lensId: String, width: Int, height: Int): PersistedIntrinsics? =
        latestSessionFor(lensId, width, height)?.let(::readIntrinsics)

    /**
     * Extract the `calibration_id` for the latest session
     * matching this lens+resolution, or `null` if there
     * isn't one (in which case the caller falls back to
     * factory UUID lookup via [`FactoryCalibration`]).
     *
     * Returns one of:
     *
     *  - A real `UUIDv4` string: the new-layout session's
     *    recorded `calibration_id`. Stamped into
     *    `bundle.intrinsics.source.calibration_id` so
     *    captures back-reference the calibration that
     *    produced their intrinsics.
     *  - `"legacy:WxH"`: the on-disk calibration predates
     *    the UUID-recording layout (either a pre-#58
     *    `<files>/calibration/<lensId>/<WxH>/<ulid>/` tree
     *    with `intrinsics.json`, or a hand-edited
     *    `calibration.json` missing the `calibration_id`
     *    field). The marker is **deliberately distinct**
     *    from both a real UUID and from the synthesised
     *    `operator-WxH` placeholder this codebase shipped
     *    in earlier builds: consumers (`bris-cli replay`,
     *    diagnostic overlays) can tell a legitimately
     *    untraceable calibration apart from a buggy stub.
     *    New calibrations always record the real UUID via
     *    [`newSession`]; the marker is migration-only and
     *    will fall out of the corpus once all pre-#58
     *    on-device calibrations have been re-run.
     *  - `null`: no matching session at all.
     */
    fun latestCalibrationIdFor(lensId: String, width: Int, height: Int): String? {
        val dir = latestSessionFor(lensId, width, height) ?: return null
        val manifest = File(dir, "calibration.json")
        if (manifest.isFile) {
            val obj = runCatching { JSONObject(manifest.readText()) }.getOrNull()
            val id = obj?.optString("calibration_id")
            if (!id.isNullOrEmpty()) return id
        }
        return "legacy:${width}x${height}"
    }

    private fun externalSessionsMatching(
        lensId: String,
        width: Int,
        height: Int,
    ): List<File> {
        val dirs = externalRoot.listFiles { f -> f.isDirectory } ?: return emptyList()
        return dirs.mapNotNull { d ->
            val manifest = File(d, "calibration.json")
            if (!manifest.isFile) return@mapNotNull null
            val obj = runCatching { JSONObject(manifest.readText()) }.getOrNull()
                ?: return@mapNotNull null
            val mLens = obj.optString("lens_id")
            val mW = obj.optInt("width")
            val mH = obj.optInt("height")
            if (mLens == lensId && mW == width && mH == height) d else null
        }
    }

    private fun readIntrinsics(sessionDir: File): PersistedIntrinsics? {
        // New layout: calibration.json
        val newPath = File(sessionDir, "calibration.json")
        if (newPath.isFile) {
            return runCatching { decode(JSONObject(newPath.readText())) }.getOrNull()
        }
        // Legacy: intrinsics.json
        val legacyPath = File(sessionDir, "intrinsics.json")
        if (legacyPath.isFile) {
            return runCatching { decode(JSONObject(legacyPath.readText())) }.getOrNull()
        }
        return null
    }

    private fun decode(obj: JSONObject): PersistedIntrinsics? {
        val intr = obj.optJSONObject("intrinsics") ?: return null
        return PersistedIntrinsics(
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
    }

    /**
     * Persisted calibration as loaded back from disk.
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
        /**
         * Mount with the new external-files root for writes,
         * and the legacy internal root as a read-through
         * fallback for older calibrations.
         */
        fun forApp(context: Context): CalibrationStore {
            val external = context.getExternalFilesDir(null) ?: context.filesDir
            return CalibrationStore(
                externalRoot = File(external, "calibration"),
                legacyInternalRoot = File(context.filesDir, "calibration"),
            )
        }
    }
}
