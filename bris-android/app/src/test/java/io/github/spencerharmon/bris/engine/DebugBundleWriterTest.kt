package io.github.spencerharmon.bris.engine

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Unit-test the manifest JSON shape produced by
 * [`DebugBundleWriter.buildManifestJson`], focusing on the
 * `intrinsics.source.calibration_id` provenance threading
 * for the Operator calibration variant.
 *
 * Covers Phase 7.5 item 4: the Operator variant must carry
 * the real session UUID for new calibrations and a
 * `legacy:WxH` marker for pre-refactor on-disk calibrations
 * that have no recorded UUID.
 */
class DebugBundleWriterTest {

    private fun snapshot(): DebugBundleWriter.CaptureSnapshot =
        DebugBundleWriter.CaptureSnapshot(
            frameCount = 1,
            startedUnixMs = 1_000,
            endedUnixMs = 2_000,
            firstFrameBlake3 = "0".repeat(64),
            firstFrameWidth = 4032,
            firstFrameHeight = 3024,
        )

    private fun intrinsics(): CalibrationStore.PersistedIntrinsics =
        CalibrationStore.PersistedIntrinsics(
            fx = 100.0, fy = 110.0, cx = 50.0, cy = 60.0,
            k1 = 0.0, k2 = 0.0, k3 = 0.0, p1 = 0.0, p2 = 0.0,
            width = 4032, height = 3024, rmsPx = 0.5,
        )

    @Test
    fun operator_calibration_threads_real_uuid_into_manifest() {
        val realUuid = "11111111-2222-4333-8444-555555555555"
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Operator(
                intrinsics = intrinsics(),
                calibrationId = realUuid,
            ),
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val source = manifest.getJSONObject("intrinsics").getJSONObject("source")
        assertEquals("user_calibration", source.getString("kind"))
        assertEquals(realUuid, source.getString("calibration_id"))
    }

    @Test
    fun operator_calibration_threads_legacy_marker_into_manifest() {
        // Legacy on-disk calibration: CalibrationStore returns
        // `legacy:WxH` because the session predates UUID
        // recording. The marker must surface verbatim in the
        // bundle so consumers can tell it apart from a real
        // UUID and from the old `operator-WxH` placeholder.
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Operator(
                intrinsics = intrinsics(),
                calibrationId = "legacy:4032x3024",
            ),
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val source = manifest.getJSONObject("intrinsics").getJSONObject("source")
        assertEquals("user_calibration", source.getString("kind"))
        assertEquals("legacy:4032x3024", source.getString("calibration_id"))
    }

    @Test
    fun ap_input_absent_when_observer_null() {
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Placeholder,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        org.junit.Assert.assertFalse(
            "ap_input must be absent for cold-start",
            manifest.has("ap_input"),
        )
        org.junit.Assert.assertFalse(manifest.has("ap_derivation_trace"))
    }

    @Test
    fun ap_input_reflects_operator_supplied_fix() {
        val inputs = DebugBundleWriter.Inputs(
            observer = DebugBundleWriter.ObserverFix(
                latitudeDeg = 30.5,
                longitudeDeg = -97.25,
                eyeHeightM = 3.5,
            ),
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Placeholder,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val ap = manifest.getJSONObject("ap_input")
        assertEquals(30.5, ap.getDouble("lat"), 0.0)
        assertEquals(-97.25, ap.getDouble("lon"), 0.0)
        assertEquals(3.5, ap.getDouble("eye_height_m"), 0.0)
    }

    @Test
    fun gps_truth_uses_provided_captured_unix_ms() {
        val supplied = 1_700_000_000_000L
        val gps = io.github.spencerharmon.bris.upload.GpsInfo(
            latDeg = 30.0,
            lonDeg = -97.0,
            horizontalAccuracyM = 12.5,
            source = "network",
            capturedUnixMs = supplied,
        )
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Placeholder,
            gpsTruth = gps,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val truth = manifest.getJSONObject("gps_truth")
        assertEquals(supplied, truth.getLong("captured_unix_ms"))
        assertEquals(12.5, truth.getDouble("lat_sigma_m"), 0.0)
        assertEquals(12.5, truth.getDouble("lon_sigma_m"), 0.0)
    }

    @Test
    fun gps_truth_omitted_when_accuracy_unknown() {
        val gps = io.github.spencerharmon.bris.upload.GpsInfo(
            latDeg = 30.0,
            lonDeg = -97.0,
            horizontalAccuracyM = 0.0,
            source = "network",
            capturedUnixMs = 1_700_000_000_000L,
        )
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Placeholder,
            gpsTruth = gps,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        org.junit.Assert.assertFalse(
            "gps_truth must be omitted when accuracy is unknown",
            manifest.has("gps_truth"),
        )
    }

    @Test
    fun placeholder_calibration_marks_intrinsics_placeholder_true() {
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Placeholder,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val intrinsics = manifest.getJSONObject("intrinsics")
        org.junit.Assert.assertTrue(intrinsics.has("placeholder"))
        org.junit.Assert.assertTrue(intrinsics.getBoolean("placeholder"))
    }

    @Test
    fun operator_calibration_omits_placeholder_marker() {
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Operator(
                intrinsics = intrinsics(),
                calibrationId = "11111111-2222-4333-8444-555555555555",
            ),
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        org.junit.Assert.assertFalse(
            manifest.getJSONObject("intrinsics").has("placeholder"),
        )
    }

    @Test
    fun source_rotation_90_rotates_intrinsics_and_swaps_dimensions() {
        // Sensor-native operator calibration is landscape
        // 4032x3024; analyzer applied CW90, so the on-disk
        // PGM is 3024x4032 portrait. bundle.json must match
        // the bytes on disk, not the sensor-native cal.
        val calIntr = CalibrationStore.PersistedIntrinsics(
            fx = 3100.0, fy = 3090.0, cx = 2014.0, cy = 1491.0,
            k1 = 0.01, k2 = -0.02, k3 = 0.0, p1 = 0.0, p2 = 0.0,
            width = 4032, height = 3024, rmsPx = 0.5,
        )
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Operator(
                intrinsics = calIntr,
                calibrationId = "11111111-2222-4333-8444-555555555555",
            ),
            sourceRotationDeg = 90,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val intr = manifest.getJSONObject("intrinsics")
        assertEquals(3024, intr.getInt("width"))
        assertEquals(4032, intr.getInt("height"))
        // fx/fy swapped
        assertEquals(3090.0, intr.getDouble("fx"), 0.0)
        assertEquals(3100.0, intr.getDouble("fy"), 0.0)
        // 90° CW: cx_new = (h-1)-cy_old = 3023 - 1491 = 1532
        // cy_new = cx_old = 2014
        assertEquals(1532.0, intr.getDouble("cx"), 0.0)
        assertEquals(2014.0, intr.getDouble("cy"), 0.0)
        // Capture block records the applied rotation.
        val cap = manifest.getJSONObject("capture")
        assertEquals(0, cap.getInt("source_rotation_deg"))
        assertEquals(90, cap.getInt("pre_rotation_was_deg"))
    }

    @Test
    fun source_rotation_0_leaves_intrinsics_alone() {
        val calIntr = CalibrationStore.PersistedIntrinsics(
            fx = 3100.0, fy = 3090.0, cx = 2014.0, cy = 1491.0,
            k1 = 0.0, k2 = 0.0, k3 = 0.0, p1 = 0.0, p2 = 0.0,
            width = 4032, height = 3024, rmsPx = 0.5,
        )
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Operator(
                intrinsics = calIntr,
                calibrationId = "11111111-2222-4333-8444-555555555555",
            ),
            sourceRotationDeg = 0,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val intr = manifest.getJSONObject("intrinsics")
        assertEquals(4032, intr.getInt("width"))
        assertEquals(3024, intr.getInt("height"))
        assertEquals(3100.0, intr.getDouble("fx"), 0.0)
        assertEquals(3090.0, intr.getDouble("fy"), 0.0)
        assertEquals(2014.0, intr.getDouble("cx"), 0.0)
        assertEquals(1491.0, intr.getDouble("cy"), 0.0)
    }

    @Test
    fun source_rotation_180_flips_principal_point() {
        val calIntr = CalibrationStore.PersistedIntrinsics(
            fx = 3100.0, fy = 3090.0, cx = 2014.0, cy = 1491.0,
            k1 = 0.0, k2 = 0.0, k3 = 0.0, p1 = 0.0, p2 = 0.0,
            width = 4032, height = 3024, rmsPx = 0.5,
        )
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Operator(
                intrinsics = calIntr,
                calibrationId = "11111111-2222-4333-8444-555555555555",
            ),
            sourceRotationDeg = 180,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val intr = manifest.getJSONObject("intrinsics")
        assertEquals(4032, intr.getInt("width"))
        assertEquals(3024, intr.getInt("height"))
        assertEquals(3100.0, intr.getDouble("fx"), 0.0)
        assertEquals(3090.0, intr.getDouble("fy"), 0.0)
        // (w-1)-cx = 4031 - 2014 = 2017; (h-1)-cy = 3023 - 1491 = 1532
        assertEquals(2017.0, intr.getDouble("cx"), 0.0)
        assertEquals(1532.0, intr.getDouble("cy"), 0.0)
    }

    @Test
    fun source_rotation_270_swaps_axes() {
        val calIntr = CalibrationStore.PersistedIntrinsics(
            fx = 3100.0, fy = 3090.0, cx = 2014.0, cy = 1491.0,
            k1 = 0.0, k2 = 0.0, k3 = 0.0, p1 = 0.0, p2 = 0.0,
            width = 4032, height = 3024, rmsPx = 0.5,
        )
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Operator(
                intrinsics = calIntr,
                calibrationId = "11111111-2222-4333-8444-555555555555",
            ),
            sourceRotationDeg = 270,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snapshot(), inputs)
        val intr = manifest.getJSONObject("intrinsics")
        assertEquals(3024, intr.getInt("width"))
        assertEquals(4032, intr.getInt("height"))
        assertEquals(3090.0, intr.getDouble("fx"), 0.0)
        assertEquals(3100.0, intr.getDouble("fy"), 0.0)
        // 270° CW: cx_new = cy_old = 1491; cy_new = (w-1)-cx_old = 4031-2014 = 2017
        assertEquals(1491.0, intr.getDouble("cx"), 0.0)
        assertEquals(2017.0, intr.getDouble("cy"), 0.0)
    }

    @Test
    fun first_frame_blake3_omitted_when_null() {
        val snap = DebugBundleWriter.CaptureSnapshot(
            frameCount = 0,
            startedUnixMs = 1_000,
            endedUnixMs = 2_000,
            firstFrameBlake3 = null,
            firstFrameWidth = 0,
            firstFrameHeight = 0,
        )
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 4032,
            captureHeight = 3024,
            calibration = CalibrationSource.Placeholder,
        )
        val manifest = DebugBundleWriter.buildManifestJson("cap-1", snap, inputs)
        org.junit.Assert.assertFalse(
            manifest.getJSONObject("capture").has("first_frame_blake3"),
        )
    }
}
