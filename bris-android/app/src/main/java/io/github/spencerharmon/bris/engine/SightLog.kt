package io.github.spencerharmon.bris.engine

import android.content.Context
import android.os.Build
import io.github.spencerharmon.bris.upload.GpsInfo
import io.github.spencerharmon.bris.upload.ManifestBuilder
import io.github.spencerharmon.bris.upload.MediaSummary
import org.json.JSONObject
import uniffi.bris_ffi.FfiPublishedFix
import uniffi.bris_ffi.formatPbris
import java.io.File
import java.io.FileOutputStream
import java.time.Instant

/**
 * On-device sight log: persisted records of operator-captured
 * fixes.
 *
 * Lives at `<external-files>/sights/<session-ulid>/`:
 *
 *   manifest.json                  schema-v1 manifest
 *   media/
 *     frame_<frame_id>.pgm         contributing-frame pixel bytes
 *     frame_<frame_id>.json        per-frame metadata
 *     pbris.log                    formatted $PBRIS,FIX line(s)
 *
 * Stored under the app's external-files dir specifically so
 * the operator can pull entries off the device via plain MTP /
 * `adb pull` without `run-as` gymnastics. See AGENTS.md
 * "Where work runs" and `docs/design/sight_session.md` for the
 * lifecycle.
 *
 * Only operator-captured fixes land here. The continuous
 * publication stream the engine emits during a capture session
 * is filtered down to whatever the [`SessionRecorder`] decided
 * was the session's outcome (the first sustained-green fix, or
 * the best yellow available at Stop / timeout, or nothing if
 * only red was ever published). Debug-capture's every-frame
 * dump is a separate path under `<app-files>/debug-capture/`.
 */
class SightLog(private val rootDir: File) {

    init {
        rootDir.mkdirs()
    }

    /**
     * Write one captured fix to disk. Returns the session
     * directory the entry landed in (so the caller can show a
     * "saved to ..." toast or navigate the operator to it).
     *
     * @param sessionId Caller-supplied ULID. The session
     *                  directory's name; matches the manifest's
     *                  submission ID for cross-reference with
     *                  the collector path.
     * @param outcome   The recorder's verdict for the session.
     *                  Drives how the entry is annotated and
     *                  whether it carries a fix at all.
     * @param frames    Map from contributing-frame ID to the
     *                  raw bytes pulled from the engine. Empty
     *                  when the session ended without a fix.
     * @param pbrisLines Formatted `$PBRIS,FIX` line(s) for the
     *                   captured fix. One per published fix the
     *                   recorder accepted.
     * @param deviceUuid Per-install UUID for the manifest.
     * @param appVersion Version stamp for the manifest.
     * @param coreVersion `bris-ffi` version (forwarded into the
     *                    manifest for cross-reference with the
     *                    engine that produced the fix).
     * @param gps        Optional coarse GPS for the manifest.
     * @param note       Optional operator-supplied note.
     */
    @Suppress("LongParameterList")
    fun writeEntry(
        sessionId: String,
        outcome: SessionOutcome,
        frames: Map<ULong, FrameBytes>,
        pbrisLines: List<String>,
        deviceUuid: String,
        appVersion: String,
        coreVersion: String,
        gps: GpsInfo? = null,
        note: String? = null,
    ): File {
        val sessionDir = File(rootDir, sessionId).apply { mkdirs() }
        val mediaDir = File(sessionDir, "media").apply { mkdirs() }

        val media = mutableListOf<MediaSummary>()
        for ((frameId, bytes) in frames) {
            val name = "frame_${"%016d".format(frameId.toLong())}.pgm"
            val pgmPath = File(mediaDir, name)
            writePgm(pgmPath, bytes)
            media.add(
                MediaSummary(
                    filename = name,
                    role = "fix_frame",
                    sizeBytes = pgmPath.length(),
                    frameIndex = frameId.toInt(),
                    capturedAt = bytes.capturedAt,
                ),
            )
            val metaName = "frame_${"%016d".format(frameId.toLong())}.json"
            val metaPath = File(mediaDir, metaName)
            metaPath.writeText(
                JSONObject()
                    .put("frame_id", frameId.toLong())
                    .put("width", bytes.width)
                    .put("height", bytes.height)
                    .put("captured_unix_ms", bytes.capturedAt.toEpochMilli())
                    .toString(),
            )
            media.add(
                MediaSummary(
                    filename = metaName,
                    role = "frame_diagnostic",
                    sizeBytes = metaPath.length(),
                    frameIndex = frameId.toInt(),
                    capturedAt = bytes.capturedAt,
                ),
            )
        }

        if (pbrisLines.isNotEmpty()) {
            val pbrisFile = File(mediaDir, "pbris.log")
            pbrisFile.writeText(pbrisLines.joinToString("\n", postfix = "\n"))
            media.add(
                MediaSummary(
                    filename = "pbris.log",
                    role = "pbris_log",
                    sizeBytes = pbrisFile.length(),
                ),
            )
        }

        // Build the kind-specific summary. We use submission_kind
        // = "fix" matching the collector's schema; sight-log
        // entries are exactly the per-fix payload the collector
        // would have received.
        val fixSummary = when (outcome) {
            is SessionOutcome.Captured -> JSONObject()
                .put("latitude_deg", outcome.fix.latitudeDeg)
                .put("longitude_deg", outcome.fix.longitudeDeg)
                .put("sigma_major_nm", outcome.fix.sigmaMajorNm)
                .put("sigma_minor_nm", outcome.fix.sigmaMinorNm)
                .put("orientation_rad", outcome.fix.orientationRad)
                .put("n_sights", outcome.fix.nSights.toLong())
                .put("dominant_source", outcome.fix.dominantSource)
                .put("verdict", outcome.verdict.name.lowercase())
                .put("session_outcome", "captured")
            is SessionOutcome.NoFix -> JSONObject()
                .put("session_outcome", "no_fix")
                .put("reason", outcome.reason)
        }

        val builder = ManifestBuilder(
            deviceUuid = deviceUuid,
            appVersion = appVersion,
            brisCoreVersion = coreVersion,
        )
        val manifestJson = builder.fix(
            capturedAt = Instant.now(),
            gps = gps,
            note = note,
            fixSummary = fixSummary,
            media = media,
        )
        File(sessionDir, "manifest.json").writeText(manifestJson)
        return sessionDir
    }

    /**
     * Most-recent sight log entries, oldest-first. Used by the
     * sight log list screen.
     */
    fun list(): List<File> = rootDir.listFiles()
        ?.filter { it.isDirectory }
        ?.sortedBy { it.name }
        ?: emptyList()

    /**
     * Soft-delete: move an entry into `.trash/`. The bytes
     * stay on disk so the operator can restore or pull them via
     * adb; the list view ignores `.trash/`. A future cleanup
     * sweep removes entries past the retention window
     * (deferred).
     */
    fun softDelete(sessionDir: File): Boolean {
        if (!sessionDir.exists()) return false
        val trashDir = File(rootDir, ".trash").apply { mkdirs() }
        return sessionDir.renameTo(File(trashDir, sessionDir.name))
    }

    /**
     * Per-fix images-only delete: drop all PGM frames in the
     * media directory, keep the manifest + JSON snapshots +
     * pbris.log. Frees the bulk of per-entry storage while
     * preserving the diagnostic record.
     */
    fun deleteImages(sessionDir: File): Int {
        val mediaDir = File(sessionDir, "media")
        if (!mediaDir.isDirectory) return 0
        var n = 0
        mediaDir.listFiles()?.forEach {
            if (it.name.endsWith(".pgm") && it.delete()) n++
        }
        return n
    }

    /** Bytes of one frame retrieved from the engine via `frame_by_id`. */
    data class FrameBytes(
        val width: Int,
        val height: Int,
        /** u16 little-endian, length = width * height * 2. */
        val pixelsLe: ByteArray,
        val capturedAt: Instant,
    )

    private fun writePgm(path: File, frame: FrameBytes) {
        // Bris on-disk format is P5 grayscale 8-bit: down-shift
        // each u16 pixel by 8 bits to a u8. Same convention the
        // existing test_video corpus uses; round-trips through
        // bris_vision::Frame's u8-widening intake.
        FileOutputStream(path).use { out ->
            val header = "P5\n${frame.width} ${frame.height}\n255\n"
            out.write(header.toByteArray())
            // Walk the LE u16 buffer two bytes at a time, write
            // the high byte. Avoids allocating a whole u8
            // intermediate.
            val w = frame.width
            val h = frame.height
            val expected = w * h * 2
            require(frame.pixelsLe.size == expected) {
                "FrameBytes pixelsLe size=${frame.pixelsLe.size} != width*height*2=$expected"
            }
            var i = 0
            val buf = ByteArray(w)
            for (row in 0 until h) {
                for (col in 0 until w) {
                    // Little-endian: low byte at i, high at i+1.
                    buf[col] = frame.pixelsLe[i + 1]
                    i += 2
                }
                out.write(buf)
            }
        }
    }

    companion object {
        /** Mount the sight log under `<external-files>/sights/`. */
        fun forApp(context: Context): SightLog {
            val root = context.getExternalFilesDir(null)
                ?: context.filesDir
            return SightLog(File(root, "sights"))
        }

        /**
         * Manifest "device" + "version" stamping convenience.
         * The real values come from BuildConfig + the FFI's
         * version() call at the call site; this just centralizes
         * the format strings used in fallback paths.
         */
        fun deviceModelString(): String =
            "${Build.MANUFACTURER} ${Build.MODEL}"

        fun deviceOsString(): String =
            "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})"
    }
}

/** What the [`SessionRecorder`] decided about a capture session. */
sealed interface SessionOutcome {
    /**
     * The session produced a fix worth recording. `verdict` is
     * the threshold band the fix landed in (green if accepted
     * automatically; yellow if accepted on Stop / timeout
     * because no green ever arrived).
     */
    data class Captured(
        val fix: FfiPublishedFix,
        val verdict: FixVerdict,
    ) : SessionOutcome

    /**
     * The session ended without producing a usable fix. `reason`
     * is the human-readable explanation surfaced in the
     * sight-log entry (e.g. "no fix published before timeout",
     * "operator stopped before any non-red fix").
     */
    data class NoFix(val reason: String) : SessionOutcome
}

/**
 * Threshold band a published fix lands in.
 *
 * Defaults track plan.org Phase 7 ("session UX"):
 *   green:  σ_major ≤ 1.0 nm
 *   yellow: 1.0 nm < σ_major ≤ 5.0 nm
 *   red:    σ_major > 5.0 nm  (never accepted)
 *
 * Operator-configurable thresholds are deferred to a settings
 * UI; the values are read from [`SessionThresholds`] today.
 */
enum class FixVerdict { GREEN, YELLOW, RED }
