package io.github.spencerharmon.bris.engine

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class CalibrationStoreTest {

    @get:Rule
    val tmp = TemporaryFolder()

    private fun store(): CalibrationStore = CalibrationStore(
        externalRoot = tmp.newFolder("ext"),
        legacyInternalRoot = tmp.newFolder("legacy"),
    )

    @Test
    fun newSession_creates_uuid_keyed_dir() {
        val s = store()
        val dir = s.newSession(lensId = "0", width = 4032, height = 3024)
        assertTrue(dir.isDirectory)
        // Directory name parses as a UUID.
        java.util.UUID.fromString(dir.name)
        // calibration.json stub written.
        val manifest = JSONObject(File(dir, "calibration.json").readText())
        assertEquals(dir.name, manifest.getString("calibration_id"))
        assertEquals("0", manifest.getString("lens_id"))
        assertEquals(4032, manifest.getInt("width"))
        assertEquals(3024, manifest.getInt("height"))
        assertEquals("in_progress", manifest.getString("status"))
    }

    @Test
    fun latestSessionFor_matches_by_lens_and_resolution() {
        val s = store()
        s.newSession("0", 4032, 3024)
        val match = s.newSession("0", 4032, 3024)
        s.newSession("1", 1920, 1080)
        val latest = s.latestSessionFor("0", 4032, 3024)
        assertNotNull(latest)
        // mtime tie-broken to most recent, but in this fast
        // test both might have the same mtime; just confirm
        // it's one of the two and not the wrong-lens one.
        assertTrue(latest!!.parentFile == match.parentFile)
    }

    @Test
    fun latestSessionFor_returns_null_when_unmatched() {
        val s = store()
        s.newSession("0", 4032, 3024)
        assertNull(s.latestSessionFor("0", 1920, 1080))
        assertNull(s.latestSessionFor("99", 4032, 3024))
    }

    @Test
    fun legacy_internal_path_is_read_through_fallback() {
        // Set up a legacy-shaped on-disk calibration.
        val legacy = tmp.newFolder("legacy2")
        val ext = tmp.newFolder("ext2")
        val legacySession = File(legacy, "0/4032x3024/01HXYZABC123").apply { mkdirs() }
        val intrJson = JSONObject()
            .put(
                "intrinsics",
                JSONObject()
                    .put("fx", 100.0).put("fy", 110.0)
                    .put("cx", 50.0).put("cy", 60.0)
                    .put("k1", 0.0).put("k2", 0.0).put("k3", 0.0)
                    .put("p1", 0.0).put("p2", 0.0),
            )
            .put("width", 4032).put("height", 3024)
            .put("rms_px", 0.5)
        File(legacySession, "intrinsics.json").writeText(intrJson.toString())

        val s = CalibrationStore(externalRoot = ext, legacyInternalRoot = legacy)
        val intr = s.latestIntrinsicsFor("0", 4032, 3024)
        assertNotNull(intr)
        assertEquals(100.0, intr!!.fx, 1e-9)
        assertEquals(0.5, intr.rmsPx, 1e-9)
    }

    @Test
    fun latestCalibrationIdFor_returns_uuid_from_new_layout() {
        val s = store()
        val dir = s.newSession("0", 4032, 3024)
        val id = s.latestCalibrationIdFor("0", 4032, 3024)
        assertEquals(dir.name, id)
    }

    @Test
    fun factory_profile_uuid_is_stable() {
        // The bake-in promise: re-fetching the profile gives
        // the same UUID, run after run. Catches accidental
        // randomization at construction.
        val a = FactoryCalibration.lookup("0", 4032, 3024, model = "S62 Pro")
        val b = FactoryCalibration.lookup("0", 4032, 3024, model = "S62 Pro")
        assertNotNull(a)
        assertNotNull(b)
        assertEquals(a!!.calibrationId, b!!.calibrationId)
        // Spec value baked into FactoryCalibration; if this
        // changes the field has been regenerated and all
        // prior captures' calibration_id back-references
        // break.
        assertEquals(
            "f15e1aa1-5ca7-4c62-b62f-cab1a1bca1ed",
            a.calibrationId.toString(),
        )
    }
}
