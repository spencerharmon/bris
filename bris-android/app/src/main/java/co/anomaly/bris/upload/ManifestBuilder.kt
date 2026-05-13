package co.anomaly.bris.upload

import android.os.Build
import org.json.JSONArray
import org.json.JSONObject
import java.time.Instant
import java.time.format.DateTimeFormatter

/**
 * Build a submission manifest matching `bris_collector::manifest::Manifest`
 * (schema version 1).
 *
 * The schema is defined in `docs/design/diagnostic_collection.md`
 * and validated by the collector. Mismatches here surface as
 * a 400 from the server with a clear `manifest validate:` or
 * `manifest parse:` detail; the submitter logs them.
 *
 * The manifest is independent of the Rust UniFFI types; we
 * build it from primitives the Kotlin side already has
 * (versions, device info, capture metadata) plus optional
 * payloads serialized as opaque JSON.
 */
class ManifestBuilder(
    private val deviceUuid: String,
    private val appVersion: String,
    private val brisCoreVersion: String,
    private val brisDataVersion: String? = null,
) {
    /** Build a manifest for a "send fix" submission. */
    fun fix(
        capturedAt: Instant,
        gps: GpsInfo? = null,
        note: String? = null,
        fixSummary: JSONObject,
        media: List<MediaSummary>,
    ): String = baseObject(
        kind = "fix",
        capturedAt = capturedAt,
        gps = gps,
        note = note,
        media = media,
    ).put("fix", fixSummary).toString()

    /** Build a manifest for a "send calibration" submission. */
    fun calibration(
        capturedAt: Instant,
        gps: GpsInfo? = null,
        note: String? = null,
        calibrationSummary: JSONObject,
        media: List<MediaSummary>,
    ): String = baseObject(
        kind = "calibration",
        capturedAt = capturedAt,
        gps = gps,
        note = note,
        media = media,
    ).put("calibration", calibrationSummary).toString()

    /** Build a manifest for a "debug capture" submission. */
    fun debugCapture(
        capturedAt: Instant,
        gps: GpsInfo? = null,
        note: String? = null,
        debugSummary: JSONObject,
        media: List<MediaSummary>,
    ): String = baseObject(
        kind = "debug_capture",
        capturedAt = capturedAt,
        gps = gps,
        note = note,
        media = media,
    ).put("debug_capture", debugSummary).toString()

    private fun baseObject(
        kind: String,
        capturedAt: Instant,
        gps: GpsInfo?,
        note: String?,
        media: List<MediaSummary>,
    ): JSONObject {
        val now = Instant.now()
        val device = JSONObject()
            .put("uuid", deviceUuid)
            .put("model", "${Build.MANUFACTURER} ${Build.MODEL}")
            .put("os", "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})")
        val versions = JSONObject()
            .put("app", appVersion)
            .put("bris_core", brisCoreVersion)
            .put("bris_data", brisDataVersion ?: JSONObject.NULL)
            .put("submission_schema", SCHEMA_VERSION)
        val mediaArr = JSONArray()
        for (m in media) {
            val item = JSONObject()
                .put("filename", m.filename)
                .put("role", m.role)
                .put("size_bytes", m.sizeBytes)
            m.frameIndex?.let { item.put("frame_index", it) }
            m.capturedAt?.let { item.put("captured_at", DateTimeFormatter.ISO_INSTANT.format(it)) }
            mediaArr.put(item)
        }
        val obj = JSONObject()
            .put("schema_version", SCHEMA_VERSION)
            .put("submission_kind", kind)
            .put("submitted_at", DateTimeFormatter.ISO_INSTANT.format(now))
            .put("device", device)
            .put("versions", versions)
            .put("captured_at", DateTimeFormatter.ISO_INSTANT.format(capturedAt))
            .put("media", mediaArr)
        if (gps != null) {
            obj.put(
                "gps",
                JSONObject()
                    .put("lat_deg", gps.latDeg)
                    .put("lon_deg", gps.lonDeg)
                    .put("horizontal_accuracy_m", gps.horizontalAccuracyM)
                    .put("source", gps.source),
            )
        } else {
            obj.put("gps", JSONObject.NULL)
        }
        if (note != null) {
            obj.put("note", note)
        } else {
            obj.put("note", JSONObject.NULL)
        }
        // Populate the kind-specific fields with NULL where not
        // applicable; the collector tolerates absent keys but
        // explicit nulls round-trip cleanly through serde_json.
        when (kind) {
            "fix" -> {
                obj.put("calibration", JSONObject.NULL)
                obj.put("debug_capture", JSONObject.NULL)
            }
            "calibration" -> {
                obj.put("fix", JSONObject.NULL)
                obj.put("debug_capture", JSONObject.NULL)
            }
            "debug_capture" -> {
                obj.put("fix", JSONObject.NULL)
                obj.put("calibration", JSONObject.NULL)
            }
        }
        return obj
    }

    companion object {
        const val SCHEMA_VERSION = 1
    }
}

/**
 * One media file summary referenced by the manifest. The
 * Submitter sends the file bytes alongside under the same
 * filename; the collector cross-checks size.
 */
data class MediaSummary(
    val filename: String,
    val role: String,
    val sizeBytes: Long,
    val frameIndex: Int? = null,
    val capturedAt: Instant? = null,
)

/**
 * Coarse GPS, when available. Source labels match the
 * collector's manifest schema: "gps", "fused", "network".
 */
data class GpsInfo(
    val latDeg: Double,
    val lonDeg: Double,
    val horizontalAccuracyM: Double,
    val source: String,
)
