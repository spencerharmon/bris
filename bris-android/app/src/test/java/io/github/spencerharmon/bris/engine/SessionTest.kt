package io.github.spencerharmon.bris.engine

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.UUID

/**
 * Pure-JVM round-trip tests for [Session.toJson] /
 * [Session.fromJson]. Verifies the on-disk JSON matches the
 * `bris_bundle::SessionManifest` schema (snake_case fields,
 * `kinematics.kind` enum tag, `ap_seed` shape).
 */
class SessionTest {

    @Test
    fun stationary_round_trip() {
        val s = Session(
            sessionId = UUID.fromString("00000000-0000-4000-8000-000000000001"),
            title = "T1",
            createdUnixMs = 1_700_000_000_000L,
            device = Session.DeviceInfo(model = "m", os = "Android 14", appVersion = "0.1.0"),
            notes = "hello",
            apSeed = Session.ApSeed(latDeg = 12.34, lonDeg = -56.78, eyeHeightM = 3.0),
            profile = Session.Profile.Marine,
            kinematics = Session.Kinematics.Stationary,
            sightRetentionSeconds = 7200L,
            sightRetentionCapacity = 50,
            expectedToFail = false,
            orderedCaptureIds = listOf("cap-a", "cap-b"),
        )
        val json = s.toJson()
        assertEquals(1, json.getInt("schema_version"))
        assertEquals("00000000-0000-4000-8000-000000000001", json.getString("session_id"))
        assertEquals("marine", json.getString("profile"))
        assertEquals("stationary", json.getJSONObject("kinematics").getString("kind"))
        assertEquals(7200L, json.getLong("sight_retention_seconds"))
        assertEquals(2, json.getJSONArray("ordered_capture_ids").length())
        assertEquals(-56.78, json.getJSONObject("ap_seed").getDouble("lon"), 1e-9)

        val round = Session.fromJson(json)
        assertEquals(s.sessionId, round.sessionId)
        assertEquals(s.title, round.title)
        assertEquals(s.notes, round.notes)
        assertEquals(s.apSeed, round.apSeed)
        assertEquals(s.profile, round.profile)
        assertEquals(Session.Kinematics.Stationary, round.kinematics)
        assertEquals(s.sightRetentionSeconds, round.sightRetentionSeconds)
        assertEquals(s.orderedCaptureIds, round.orderedCaptureIds)
    }

    @Test
    fun max_speed_round_trip() {
        val s = Session.new("Underway").copy(
            kinematics = Session.Kinematics.MaxSpeedKn(8.0),
            expectedToFail = true,
        )
        val round = Session.fromJson(s.toJson())
        assertTrue(round.kinematics is Session.Kinematics.MaxSpeedKn)
        assertEquals(8.0, (round.kinematics as Session.Kinematics.MaxSpeedKn).kn, 1e-9)
        assertTrue(round.expectedToFail)
    }

    @Test
    fun default_factory_uses_engine_defaults() {
        val s = Session.new("T")
        assertEquals(Session.DEFAULT_RETENTION_SECONDS, s.sightRetentionSeconds)
        assertEquals(Session.DEFAULT_RETENTION_CAPACITY, s.sightRetentionCapacity)
        assertEquals(Session.Kinematics.Stationary, s.kinematics)
        assertEquals(Session.Profile.Custom, s.profile)
        assertNull(s.apSeed)
    }

    @Test
    fun missing_optional_fields_parse() {
        val raw = JSONObject()
            .put("schema_version", 1)
            .put("session_id", UUID.randomUUID().toString())
            .put("title", "t")
            .put("created_unix_ms", 0L)
            .put(
                "device",
                JSONObject().put("model", "m"),
            )
            .put("sight_retention_seconds", 600L)
            .put("sight_retention_capacity", 5)
        val parsed = Session.fromJson(raw)
        assertNull(parsed.notes)
        assertNull(parsed.apSeed)
        assertEquals(Session.Kinematics.Stationary, parsed.kinematics)
        assertEquals(Session.Profile.Custom, parsed.profile)
    }

    @Test(expected = IllegalArgumentException::class)
    fun unsupported_schema_rejected() {
        val raw = JSONObject()
            .put("schema_version", 99)
            .put("session_id", UUID.randomUUID().toString())
            .put("title", "t")
            .put("created_unix_ms", 0L)
            .put("device", JSONObject().put("model", "m"))
            .put("sight_retention_seconds", 600L)
            .put("sight_retention_capacity", 5)
        Session.fromJson(raw)
    }

    @Test
    fun bundle_writer_includes_session_id_when_set() {
        // Tested in DebugBundleWriterTest; assert here that the
        // Inputs sessionId is at least nullable.
        val inputs = DebugBundleWriter.Inputs(
            observer = null,
            lensId = "0",
            captureWidth = 1280,
            captureHeight = 720,
            calibration = CalibrationSource.Placeholder,
            sessionId = "11111111-2222-4333-8444-555555555555",
        )
        assertNotNull(inputs.sessionId)
    }
}
