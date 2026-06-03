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
}
