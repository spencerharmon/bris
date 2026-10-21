package io.github.spencerharmon.bris.engine

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.runner.RunWith
import uniffi.bris_ffi.FfiCalibrationResult
import uniffi.bris_ffi.FfiDetectionStats
import uniffi.bris_ffi.FfiDiagnosisIssue
import uniffi.bris_ffi.FfiDiagnosisLevel
import uniffi.bris_ffi.FfiIntrinsics
import uniffi.bris_ffi.FfiViewResidual
import uniffi.bris_ffi.readCalibrationManifest
import uniffi.bris_ffi.writeCalibrationManifest
import java.io.File
import java.io.FileNotFoundException

/** Uses the packaged native library and real files, not a persistence mock. */
@RunWith(AndroidJUnit4::class)
class CalibrationStoreManifestTest {
    @get:Rule
    val tmp = TemporaryFolder(InstrumentationRegistry.getInstrumentation().targetContext.cacheDir)

    private fun store() = CalibrationStore(tmp.newFolder("ext"), tmp.newFolder("legacy"))

    private fun result() = FfiCalibrationResult(
        intrinsics = FfiIntrinsics(
            fx = 3100.25, fy = 3098.5, cx = 2011.75, cy = 1510.5,
            k1 = -0.12, k2 = 0.03, k3 = -0.004, p1 = 0.001, p2 = -0.002,
        ),
        width = 4032u, height = 3024u, rmsPx = 0.45,
        nFramesUsed = 1u, nFramesTotal = 7u,
        detectionStats = FfiDetectionStats(
            tried = 7u, skippedNoBoard = 1u, skippedWrongSize = 2u, skippedIo = 3u,
        ),
        diagnosisOverall = FfiDiagnosisLevel.WARN,
        diagnosisIssues = listOf(
            FfiDiagnosisIssue(
                level = FfiDiagnosisLevel.WARN, code = "few_views",
                message = "Only one view", remediation = "Capture more views",
            ),
        ),
        perViewResiduals = listOf(
            FfiViewResidual(source = "frame_0001.jpg", rmsPx = 0.45, maxPx = 0.9, nCorners = 54u),
        ),
    )

    @Test
    fun writeIntrinsics_roundTrips_full_result_and_session_metadata_through_native_manifest() {
        val store = store()
        val original = store.newSession("physical-camera-2", 4032, 3024)
        // Moving the directory must not replace the recorded session identity.
        val dir = File(original.parentFile, "relocated-session")
        check(original.renameTo(dir))
        val expected = result()
        store.writeIntrinsics(dir, expected)
        val path = File(dir, "calibration.json")
        val manifest = JSONObject(path.readText())
        assertEquals(original.name, manifest.getString("calibration_id"))
        assertEquals("physical-camera-2", manifest.getString("lens_id"))
        assertEquals("complete", manifest.getString("status"))
        assertEquals("WARN", manifest.getString("diagnosis_overall"))
        assertEquals(expected, readCalibrationManifest(path.absolutePath))
        assertEquals(
            CalibrationStore.PersistedIntrinsics(
                fx = 3100.25, fy = 3098.5, cx = 2011.75, cy = 1510.5,
                k1 = -0.12, k2 = 0.03, k3 = -0.004, p1 = 0.001, p2 = -0.002,
                width = 4032, height = 3024, rmsPx = 0.45,
            ),
            store.latestIntrinsicsFor("physical-camera-2", 4032, 3024),
        )
        val source = resolveCalibration(store, "physical-camera-2", 4032, 3024)
            as CalibrationSource.Operator
        assertEquals(original.name, source.calibrationId)
    }

    @Test
    fun readIntrinsics_accepts_native_full_manifest_in_legacy_hierarchy_without_session_metadata() {
        val external = tmp.newFolder("ext")
        val legacy = tmp.newFolder("legacy")
        val dir = File(legacy, "0/4032x3024/01HXYZABC123").apply { mkdirs() }
        val path = File(dir, "intrinsics.json")
        writeCalibrationManifest(path.absolutePath, result(), null, null)
        val store = CalibrationStore(external, legacy)
        assertEquals(3100.25, store.latestIntrinsicsFor("0", 4032, 3024)!!.fx, 1e-9)
        assertEquals("legacy:4032x3024", store.latestCalibrationIdFor("0", 4032, 3024))
    }

    @Test
    fun readIntrinsics_does_not_fall_back_to_minimal_decoder_for_invalid_full_manifest() {
        val store = store()
        val dir = store.newSession("0", 4032, 3024)
        val path = File(dir, "calibration.json")
        assertNull(store.latestIntrinsicsFor("0", 4032, 3024))
        writeCalibrationManifest(path.absolutePath, result(), dir.name, "0")
        val complete = path.readText()
        // Intrinsics remain valid in every case, so the old Kotlin decoder
        // would accept these despite the invalid full-manifest fields.
        val invalid = listOf(
            JSONObject(complete).put("status", "in_progress"),
            JSONObject(complete).put("n_frames_used", "not-a-count"),
            JSONObject(complete).apply { remove("detection_stats") },
            JSONObject(complete).apply { remove("status") },
        )
        for (manifest in invalid) {
            path.writeText(manifest.toString())
            assertThrows(Exception::class.java) { readCalibrationManifest(path.absolutePath) }
            assertNull(store.latestIntrinsicsFor("0", 4032, 3024))
        }
    }

    @Test
    fun writeIntrinsics_rejects_missing_or_invalid_metadata_without_replacing_stub() {
        val store = store()
        val dir = store.newSession("0", 4032, 3024)
        val path = File(dir, "calibration.json")
        val stub = path.readText()
        for (field in listOf("calibration_id", "lens_id")) {
            for (value in listOf(null, JSONObject.NULL, "", "  ", 7)) {
                val invalid = JSONObject(stub).put(field, value).toString()
                path.writeText(invalid)
                assertThrows(IllegalArgumentException::class.java) {
                    store.writeIntrinsics(dir, result())
                }
                assertEquals(invalid, path.readText())
            }
        }
        path.writeText("not json")
        assertThrows(org.json.JSONException::class.java) { store.writeIntrinsics(dir, result()) }
        assertEquals("not json", path.readText())
        check(path.delete())
        assertThrows(FileNotFoundException::class.java) { store.writeIntrinsics(dir, result()) }
        assertFalse(path.exists())
    }
}
