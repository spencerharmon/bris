package io.github.spencerharmon.bris.engine

import android.content.Context
import android.os.Build
import io.github.spencerharmon.bris.BuildConfig
import io.github.spencerharmon.bris.location.CoarseLocation
import io.github.spencerharmon.bris.upload.GpsInfo
import org.json.JSONArray
import org.json.JSONObject
import uniffi.bris_ffi.writeBundleManifest
import uniffi.bris_ffi.version as brisFfiVersion

/**
 * Compose a `bris_bundle::BundleManifest` from the on-device
 * session state and hand it to the Rust FFI for persistence at
 * `<bundle-root>/bundle.json`.
 *
 * The schema lives in `crates/bris-bundle/src/lib.rs`
 * (`schema_version: 1`). We construct the JSON here rather
 * than introducing a parallel set of UniFFI records because
 *
 *   - the manifest is written exactly once per save, so the
 *     per-call serialisation overhead is irrelevant, and
 *   - keeping the schema in one place (the Rust crate) avoids
 *     the FFI contract drifting away from the on-disk format
 *     consumed by `bris-cli replay`.
 *
 * `BundleManifest::save_to_dir` writes the file
 * pretty-printed; the FFI wrapper validates `schema_version`
 * and parses the JSON before writing, so a typo here surfaces
 * as `FfiError::InvalidArgument` at save time rather than as
 * a silent corrupt bundle.
 *
 * Three axes are kept independent on purpose (see
 * `docs/design/debug_bundle_schema.md` \u2014 "Three independent
 * axes"):
 *
 *   - `ap_input` is the AP the on-device engine actually ran
 *     against. `null` means cold-start.\n *   - `gps_truth` is an out-of-band ground-truth fix used by
 *     replay scoring; **never** substituted for `ap_input`.
 *   - `ap_derivation_trace` is loose provenance metadata that
 *     evolves as the engine grows more AP sources.
 */
object DebugBundleWriter {

    /**
     * Plain-data observer fix used as the manifest's
     * `ap_input`. A local type — not [`uniffi.bris_ffi.FfiObserver`]
     * — so the JVM unit tests don't have to load the
     * UniFFI-generated class (and, transitively, the native
     * library) just to assert on the JSON shape.
     */
    data class ObserverFix(
        val latitudeDeg: Double,
        val longitudeDeg: Double,
        val eyeHeightM: Double,
    )

    /** Inputs the caller pulls together before invoking [write]. */
    data class Inputs(
        /** Operator-entered AP fed to the engine, or `null` for cold-start. */
        val observer: ObserverFix?,
        /** Provenance of [`observer`] (matches `ApProvenance` snake_case). */
        val apProvenance: String = "operator_entered",
        /** Lens id (Camera2 physical id) the session ran against. */
        val lensId: String,
        /** Capture resolution the engine consumed. */
        val captureWidth: Int,
        val captureHeight: Int,
        /** Intrinsics + provenance applied to the capture. */
        val calibration: CalibrationSource,
        /** Optional ground-truth GPS fix; **never** used as AP. */
        val gpsTruth: GpsInfo? = null,
        /**
         * Owning session UUIDv4 (string). When non-null, stamped
         * into `bundle.session_id` so `bris replay --bundle`
         * can locate the sibling `session.json` for overlay.
         */
        val sessionId: String? = null,
    )

    /**
     * Build the manifest JSON, call the Rust FFI to write
     * `<bundleDir>/bundle.json`. Returns `false` (and logs)
     * on failure; the buffer save proceeds regardless so a
     * manifest issue never destroys the operator's recorded
     * frames.
     */
    fun write(
        bundleDir: java.io.File,
        bundleId: String,
        snapshot: DebugCaptureBuffer.CaptureSnapshot,
        inputs: Inputs,
    ): Boolean {
        val manifest = buildManifestJson(bundleId, snapshot, inputs)
        return try {
            writeBundleManifest(bundleDir.absolutePath, manifest.toString())
            true
        } catch (t: Throwable) {
            android.util.Log.w(TAG, "writeBundleManifest failed: ${t.message}")
            false
        }
    }

    /** Convenience: pull GPS-truth (if Debug mode + permission). */
    fun maybeGpsTruth(context: Context): GpsInfo? =
        try { CoarseLocation.getLastKnown(context) } catch (_: Throwable) { null }

    internal fun buildManifestJson(
        bundleId: String,
        snapshot: DebugCaptureBuffer.CaptureSnapshot,
        inputs: Inputs,
    ): JSONObject {
        val device = JSONObject()
            .put("model", Build.MODEL ?: "unknown")
            .put("os", "Android ${Build.VERSION.RELEASE ?: Build.VERSION.SDK_INT.toString()}")
            .put("app_version", BuildConfig.BRIS_APP_VERSION)

        val build = buildInfoJson()

        val capture = JSONObject()
            // Frames written by `DebugCaptureBuffer` are
            // gravity-up already (the YUV path declares the
            // sensor as identity-rotation; CameraX's
            // `targetRotation` is applied upstream of the
            // analyzer). Declare 0 here and record the same
            // value in `pre_rotation_was_deg` for audit.
            .put("source_rotation_deg", 0)
            .put("pre_rotation_was_deg", 0)
            .put("frame_count", snapshot.frameCount)
            .put("started_unix_ms", snapshot.startedUnixMs)
            .put("ended_unix_ms", snapshot.endedUnixMs)
            .put("first_frame_blake3", snapshot.firstFrameBlake3)

        val intrinsics = intrinsicsRecord(
            inputs.calibration,
            inputs.lensId,
            inputs.captureWidth,
            inputs.captureHeight,
        )

        val root = JSONObject()
            .put("schema_version", 1)
            .put("bundle_id", bundleId)
            .put("device", device)
            .put("build", build)
            .put("capture", capture)
            .put("intrinsics", intrinsics)
            .put("notes", "")

        inputs.sessionId?.let { root.put("session_id", it) }

        if (inputs.observer != null) {
            val ap = JSONObject()
                .put("lat", inputs.observer.latitudeDeg)
                .put("lon", inputs.observer.longitudeDeg)
                .put("eye_height_m", inputs.observer.eyeHeightM)
                .put("provenance", apProvenanceJson(inputs.apProvenance))
            root.put("ap_input", ap)
            root.put(
                "ap_derivation_trace",
                JSONObject().put("method", inputs.apProvenance),
            )
        }

        if (inputs.gpsTruth != null) {
            val g = inputs.gpsTruth
            // Coarse Android last-known fixes carry a single
            // horizontal-accuracy figure; project it onto both
            // axes equally. Altitude/sat-count are not
            // available from `getLastKnownLocation`.
            val accuracy = if (g.horizontalAccuracyM > 0.0) g.horizontalAccuracyM else 100.0
            val truth = JSONObject()
                .put("lat", g.latDeg)
                .put("lon", g.lonDeg)
                .put("lat_sigma_m", accuracy)
                .put("lon_sigma_m", accuracy)
                .put("captured_unix_ms", System.currentTimeMillis())
                .put("source", "android_${g.source}")
            root.put("gps_truth", truth)
        }

        return root
    }

    private fun apProvenanceJson(method: String): Any {
        // The Rust schema uses externally-tagged snake_case
        // for `ApProvenance`. Unit variants serialise as a
        // bare string; `Other { detail }` as
        // `{"other": {"detail": "..."}}`. Anything outside the
        // documented set falls through to `Other` so the FFI
        // doesn't reject the manifest for an unknown variant.
        return when (method) {
            "operator_entered", "prior_fix", "cold_start_cop", "stale_prior_trigger" -> method
            else -> JSONObject().put("other", JSONObject().put("detail", method))
        }
    }

    private fun intrinsicsRecord(
        source: CalibrationSource,
        lensId: String,
        width: Int,
        height: Int,
    ): JSONObject {
        val intr = when (source) {
            is CalibrationSource.Operator -> source.intrinsics
            is CalibrationSource.Factory -> source.intrinsics
            CalibrationSource.Placeholder -> null
        }
        val sourceObj = when (source) {
            is CalibrationSource.Operator ->
                JSONObject().put("kind", "user_calibration")
                    // The on-device `CalibrationSource.Operator`
                    // doesn't carry the underlying session
                    // ULID; synthesise a stable id from the
                    // calibrated resolution so replay tooling
                    // can at least correlate two bundles that
                    // ran against the same calibration.
                    .put(
                        "calibration_id",
                        "operator-${source.intrinsics.width}x${source.intrinsics.height}",
                    )
            is CalibrationSource.Factory -> JSONObject().put("kind", "factory")
            CalibrationSource.Placeholder -> JSONObject().put("kind", "placeholder")
        }
        // Brown-Conrady is the only model the on-device path
        // produces today; `None` is reserved for pinhole and
        // `FisheyeEquidistant` for future fisheye captures.
        val distortion = if (intr != null) {
            JSONObject()
                .put("model", "brown_conrady")
                .put("k1", intr.k1).put("k2", intr.k2).put("k3", intr.k3)
                .put("p1", intr.p1).put("p2", intr.p2)
        } else {
            JSONObject().put("model", "none")
        }
        val rec = JSONObject()
            .put("source", sourceObj)
            .put("width", intr?.width ?: width)
            .put("height", intr?.height ?: height)
            .put("fx", intr?.fx ?: (width.toDouble() / 1.1547))
            .put("fy", intr?.fy ?: (width.toDouble() / 1.1547))
            .put("cx", intr?.cx ?: (width / 2.0))
            .put("cy", intr?.cy ?: (height / 2.0))
            .put("distortion", distortion)
        if (intr != null && intr.rmsPx.isFinite()) rec.put("rms_px", intr.rmsPx)
        if (source is CalibrationSource.Factory) {
            rec.put(
                "profile_key",
                JSONObject()
                    .put("model", Build.MODEL ?: "")
                    .put("lens_id", lensId)
                    .put("width", intr?.width ?: width)
                    .put("height", intr?.height ?: height),
            )
        }
        return rec
    }

    /**
     * Build a `bris_bundle::BuildInfo` JSON from the FFI's
     * `version()` call (compile-time-baked git provenance of
     * the FFI shared object) plus the Android `BuildConfig`
     * fields stamped by `build.gradle.kts`. Robust to FFI
     * load failure: a missing native library logs and
     * returns a build block with `git_sha = "unknown"`
     * rather than blowing up the manifest save.
     */
    private fun buildInfoJson(): JSONObject {
        val v = try {
            brisFfiVersion()
        } catch (t: Throwable) {
            android.util.Log.w(TAG, "bris_ffi.version() failed: ${t.message}")
            null
        }
        return JSONObject().apply {
            put("git_sha", v?.gitSha ?: "unknown")
            put("git_describe", v?.gitDescribe ?: "unknown")
            put("git_dirty", v?.gitDirty ?: false)
            put("commit_count", v?.commitCount?.toLong() ?: 0L)
            put("build_timestamp_utc", v?.buildTimestampUtc ?: "unknown")
            put("bris_ffi_semver", v?.brisFfi ?: "unknown")
            put("android_version_name", BuildConfig.BRIS_APP_VERSION)
            put("android_version_code", BuildConfig.BRIS_VERSION_CODE.toLong())
        }
    }

    private const val TAG = "DebugBundleWriter"
}
