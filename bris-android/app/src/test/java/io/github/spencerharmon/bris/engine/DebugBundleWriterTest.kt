package io.github.spencerharmon.bris.engine

import io.github.spencerharmon.bris.upload.GpsInfo
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure-JVM tests for [DebugBundleWriter.buildManifestJson].
 *
 * Asserts the JSON the Android side produces matches the
 * `bris_bundle::BundleManifest` schema's field names and the
 * tag conventions for `IntrinsicsSource`, `Distortion`,
 * `ApProvenance`. The FFI's `write_bundle_manifest` re-parses
 * the JSON through serde so a contract drift here surfaces as
 * a hard error at save time \u2014 these tests catch it sooner.
 *
 * Avoids touching UniFFI's native library by using only
 * `FfiObserver` (a plain data class) and constructing the
 * `CalibrationSource` variants directly.
 */
class DebugBundleWriterTest {

    private val snapshot = DebugCaptureBuffer.CaptureSnapshot(
        frameCount = 42,
        startedUnixMs = 1_700_000_000_000,
        endedUnixMs = 1_700_000_001_000,
        firstFrameBlake3 = "0".repeat(64),
        firstFrameWidth = 1280,
        firstFrameHeight = 720,
    )

    private val placeholderInputs = DebugBundleWriter.Inputs(
        observer = DebugBundleWriter.ObserverFix(
            latitudeDeg = 12.3,
            longitudeDeg = -45.6,
            eyeHeightM = 2.0,
        ),
        lensId = "0",
        captureWidth = 1280,
        captureHeight = 720,
        calibration = CalibrationSource.Placeholder,
    )

    @Test
    fun placeholderManifest_hasRequiredTopLevelFields() {
        val obj = DebugBundleWriter.buildManifestJson("bundle-x", snapshot, placeholderInputs)
        assertEquals(1, obj.getInt("schema_version"))
        assertEquals("bundle-x", obj.getString("bundle_id"))
        assertTrue(obj.has("device"))
        assertTrue(obj.has("capture"))
        assertTrue(obj.has("intrinsics"))
        assertTrue(obj.has("build"))
    }

    @Test
    fun buildBlock_isPresentAndCarriesRequiredKeys() {
        // In pure-JVM tests the native library is unavailable;
        // `bris_ffi.version()` throws and the writer falls back
        // to "unknown" string values. Real APK builds populate
        // these from build.rs / build.gradle.kts.
        val build = DebugBundleWriter.buildManifestJson("b", snapshot, placeholderInputs)
            .getJSONObject("build")
        assertTrue(build.has("git_sha"))
        assertTrue(build.has("git_describe"))
        assertTrue(build.has("git_dirty"))
        assertTrue(build.has("commit_count"))
        assertTrue(build.has("build_timestamp_utc"))
        assertTrue(build.has("bris_ffi_semver"))
        assertTrue(build.has("android_version_name"))
        assertTrue(build.has("android_version_code"))
    }

    @Test
    fun captureBlock_carriesFrameCountAndChecksum() {
        val cap = DebugBundleWriter.buildManifestJson("b", snapshot, placeholderInputs)
            .getJSONObject("capture")
        assertEquals(0, cap.getInt("source_rotation_deg"))
        assertEquals(42L, cap.getLong("frame_count"))
        assertEquals(1_700_000_000_000L, cap.getLong("started_unix_ms"))
        assertEquals(1_700_000_001_000L, cap.getLong("ended_unix_ms"))
        assertEquals("0".repeat(64), cap.getString("first_frame_blake3"))
    }

    @Test
    fun placeholderIntrinsics_useNoneDistortion() {
        val intr = DebugBundleWriter.buildManifestJson("b", snapshot, placeholderInputs)
            .getJSONObject("intrinsics")
        assertEquals("placeholder", intr.getJSONObject("source").getString("kind"))
        assertEquals("none", intr.getJSONObject("distortion").getString("model"))
        assertFalse(intr.has("profile_key"))
    }

    @Test
    fun factoryIntrinsics_emitProfileKeyAndBrownConrady() {
        val intrinsics = CalibrationStore.PersistedIntrinsics(
            fx = 3100.0, fy = 3090.0, cx = 2016.0, cy = 1512.0,
            k1 = 0.02, k2 = -0.03, k3 = 0.0, p1 = -0.001, p2 = -0.002,
            width = 4032, height = 3024, rmsPx = 0.73,
        )
        val obj = DebugBundleWriter.buildManifestJson(
            "b",
            snapshot,
            placeholderInputs.copy(
                calibration = CalibrationSource.Factory(intrinsics, "test"),
            ),
        )
        val intr = obj.getJSONObject("intrinsics")
        assertEquals("factory", intr.getJSONObject("source").getString("kind"))
        assertEquals("brown_conrady", intr.getJSONObject("distortion").getString("model"))
        assertEquals(0.02, intr.getJSONObject("distortion").getDouble("k1"), 1e-9)
        assertEquals(4032, intr.getJSONObject("profile_key").getInt("width"))
        assertEquals(0.73, intr.getDouble("rms_px"), 1e-9)
    }

    @Test
    fun operatorIntrinsics_emitUserCalibrationSource() {
        val intrinsics = CalibrationStore.PersistedIntrinsics(
            fx = 1000.0, fy = 1000.0, cx = 640.0, cy = 360.0,
            k1 = 0.0, k2 = 0.0, k3 = 0.0, p1 = 0.0, p2 = 0.0,
            width = 1280, height = 720, rmsPx = 0.5,
        )
        val src = DebugBundleWriter.buildManifestJson(
            "b",
            snapshot,
            placeholderInputs.copy(calibration = CalibrationSource.Operator(intrinsics)),
        ).getJSONObject("intrinsics").getJSONObject("source")
        assertEquals("user_calibration", src.getString("kind"))
        assertTrue(src.getString("calibration_id").startsWith("operator-1280x720"))
    }

    @Test
    fun observerPresent_emitsApInputAndTrace() {
        val obj = DebugBundleWriter.buildManifestJson("b", snapshot, placeholderInputs)
        val ap = obj.getJSONObject("ap_input")
        assertEquals(12.3, ap.getDouble("lat"), 1e-9)
        assertEquals(-45.6, ap.getDouble("lon"), 1e-9)
        assertEquals(2.0, ap.getDouble("eye_height_m"), 1e-9)
        assertEquals("operator_entered", ap.getString("provenance"))
        assertEquals(
            "operator_entered",
            obj.getJSONObject("ap_derivation_trace").getString("method"),
        )
    }

    @Test
    fun observerNull_omitsApInput() {
        val obj = DebugBundleWriter.buildManifestJson(
            "b",
            snapshot,
            placeholderInputs.copy(observer = null),
        )
        assertFalse(obj.has("ap_input"))
        assertFalse(obj.has("ap_derivation_trace"))
    }

    @Test
    fun unknownApProvenance_fallsBackToOtherVariant() {
        val obj = DebugBundleWriter.buildManifestJson(
            "b",
            snapshot,
            placeholderInputs.copy(apProvenance = "mystery_source"),
        )
        val provenance = obj.getJSONObject("ap_input").getJSONObject("provenance")
        assertEquals("mystery_source", provenance.getJSONObject("other").getString("detail"))
    }

    @Test
    fun gpsTruthIncluded_carriesAccuracyOnBothAxes() {
        val obj = DebugBundleWriter.buildManifestJson(
            "b",
            snapshot,
            placeholderInputs.copy(
                gpsTruth = GpsInfo(
                    latDeg = 30.0,
                    lonDeg = -97.0,
                    horizontalAccuracyM = 12.5,
                    source = "network",
                ),
            ),
        )
        val gps = obj.getJSONObject("gps_truth")
        assertEquals(30.0, gps.getDouble("lat"), 1e-9)
        assertEquals(12.5, gps.getDouble("lat_sigma_m"), 1e-9)
        assertEquals(12.5, gps.getDouble("lon_sigma_m"), 1e-9)
        assertEquals("android_network", gps.getString("source"))
    }

    @Test
    fun session_id_emitted_when_set() {
        val obj = DebugBundleWriter.buildManifestJson(
            "b",
            snapshot,
            placeholderInputs.copy(sessionId = "abc-123"),
        )
        assertEquals("abc-123", obj.getString("session_id"))
    }

    @Test
    fun session_id_omitted_when_null() {
        val obj = DebugBundleWriter.buildManifestJson(
            "b",
            snapshot,
            placeholderInputs.copy(sessionId = null),
        )
        assertFalse("session_id should be omitted when null", obj.has("session_id"))
    }
}
