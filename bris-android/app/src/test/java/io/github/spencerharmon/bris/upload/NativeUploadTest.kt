package io.github.spencerharmon.bris.upload

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import io.github.spencerharmon.bris.engine.CaptureCatalog
import io.github.spencerharmon.bris.engine.Session
import io.github.spencerharmon.bris.engine.CaptureRecorder
import io.github.spencerharmon.bris.engine.CaptureStatus
import io.github.spencerharmon.bris.engine.CaptureThresholds
import io.github.spencerharmon.bris.engine.CalibrationSource
import io.github.spencerharmon.bris.engine.DebugBundleWriter
import io.github.spencerharmon.bris.engine.EngineWrapper
import io.github.spencerharmon.bris.engine.SightLog
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.SocketPolicy
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.util.UUID
import java.util.concurrent.TimeUnit
import uniffi.bris_ffi.FfiSubmissionSource
import uniffi.bris_ffi.writeBundleManifest
import uniffi.bris_ffi.FfiEngineConfig
import uniffi.bris_ffi.FfiObserver

/** Generated Kotlin -> JNA -> Rust -> real HTTP, with no fake Submitter port. */
class NativeUploadTest {
    @get:Rule val tmp = TemporaryFolder()

    private fun bundle(): File {
        val uuid = UUID.randomUUID()
        val session = File(tmp.root, "sessions/$uuid").apply { mkdirs() }
        File(session, "session.json").writeText(Session(uuid, "upload", 1700000000000,
            device = Session.DeviceInfo(model = "TestPhone")).toJson().toString())
        val capture = File(session, "captures/completed").apply { mkdirs() }
        writeBundleManifest(capture.absolutePath, """{
          "schema_version":1,"bundle_id":"completed","device":{"model":"TestPhone"},
          "capture":{"source_rotation_deg":0,"frame_count":0,"started_unix_ms":1700000000000,"ended_unix_ms":1700000001000},
          "intrinsics":{"source":{"kind":"placeholder"},"width":4,"height":3,"fx":1000,"fy":1000,"cx":2,"cy":1.5,"distortion":{"model":"none"}},
          "ap_input":{"lat":12.34,"lon":-56.78,"eye_height_m":2,"provenance":"operator_entered"},
          "gps_truth":{"lat":45,"lon":67,"lat_sigma_m":5,"lon_sigma_m":7,"captured_unix_ms":1700000000500,"source":"phone_gnss"}
        }""")
        File(capture, "pbris.log").writeText("capture completed without fix\n")
        return capture
    }

    private fun port() = FfiSubmitterPort(File(tmp.root, "queue"), "device-uuid") { note ->
        FfiSubmissionSource("device-uuid", "TestPhone", "Android", "test", "test", null, note)
    }

    @Test fun recorderStopAndTimeoutCompleteCanonicalBundlesIntoNativeQueue() = runBlocking {
        val job = SupervisorJob()
        val engineJob = SupervisorJob()
        val settings = CollectorSettingsStore(PreferenceDataStoreFactory.create(
            scope = CoroutineScope(Dispatchers.IO + job)) { File(tmp.root, "prefs.preferences_pb") })
        val config = FfiEngineConfig(
            observer = FfiObserver(12.34, -56.78, 2.0, 0.5),
            stitchingWindowSeconds = 2.0, sightWindowSeconds = 600.0,
            sightWindowCapacity = 10u, minFixPublicationIntervalMs = 1000u,
            inputRingCapacity = 120u, segmentationModelPath = null,
            horizonAnalysisWidth = null, horizonAnalysisHeight = null,
            horizonAnalysisMaxLongEdgePx = null, coldStartCoarseHemisphere = null,
            assumedMaxSpeedKn = null, storeDataRoot = File(tmp.root, "engine").absolutePath,
            enableVerticalLineProvider = null, mlGravityModelPath = null, enableMlGravity = null,
        )
        val engine = EngineWrapper.create(config, CoroutineScope(Dispatchers.Default + engineJob))
        val coordinator = UploadCoordinator(port(), settings) { "device-uuid" }
        try {
            coordinator.saveSettings("https://collector.example.com", "token")
            coordinator.setAutomaticUpload(true)
            for (timeout in listOf(false, true)) {
                val recorderJob = SupervisorJob()
                val session = UUID.randomUUID()
                val captures = File(tmp.root, "sessions/$session/captures")
                val recorder = CaptureRecorder(
                    engine = engine, sightLog = SightLog(captures),
                    scope = CoroutineScope(Dispatchers.Default + recorderJob),
                    thresholds = CaptureThresholds(timeoutMs = if (timeout) 20 else 60000),
                    deviceUuidProvider = { "device-uuid" }, appVersion = "test",
                    coreVersionProvider = { "test" },
                    captureDirProvider = { File(captures, it) },
                    bundleInputsProvider = { DebugBundleWriter.Inputs(
                        observer = DebugBundleWriter.ObserverFix(12.34, -56.78, 2.0),
                        lensId = "test-lens", captureWidth = 4, captureHeight = 3,
                        calibration = CalibrationSource.Placeholder, sessionId = session.toString(),
                    ) },
                    onBundleCompleted = { coordinator.captureCompleted(it.absolutePath) },
                    debugModeProvider = { false },
                )
                try {
                    recorder.start()
                    if (!timeout) { recorder.stop(); recorder.stop() }
                    val terminal = withTimeout(5000) { recorder.status.first {
                        it is CaptureStatus.Saved || it is CaptureStatus.Failed
                    } }
                    assertTrue("$terminal", terminal is CaptureStatus.Saved)
                    val saved = terminal as CaptureStatus.Saved
                    assertTrue(File(saved.captureDir, "bundle.json").isFile)
                    assertTrue(File(saved.captureDir, "pbris.log").isFile)
                } finally { recorderJob.cancelAndJoin() }
            }
            assertEquals(2, coordinator.counts().first)
            assertFalse(settings.current().debugMode)
        } finally { engine.close(); engineJob.cancelAndJoin(); job.cancelAndJoin() }
    }

    @Test fun canonicalSessionReviewRefusalApprovalAndDestinationChange() = runBlocking {
        val job = SupervisorJob()
        val settings = CollectorSettingsStore(PreferenceDataStoreFactory.create(
            scope = CoroutineScope(Dispatchers.IO + job)) { File(tmp.root, "prefs.preferences_pb") })
        val server = MockWebServer()
        server.start()
        try {
            val capture = bundle()
            val selected = CaptureCatalog(tmp.root).listGroups().single().captures.single()
            assertEquals(capture, selected.dir)
            val coordinator = UploadCoordinator(port(), settings) { "device-uuid" }
            coordinator.saveSettings(server.url("/").toString(), "device-token")
            coordinator.setDebugMode(true)
            coordinator.review(selected.dir.absolutePath) // dismiss/cancel: never approve
            assertEquals(0, coordinator.counts().first)
            assertEquals(0, server.requestCount)
            val old = coordinator.review(selected.dir.absolutePath)
            coordinator.approve(old) // offline approval bound to A
            coordinator.saveSettings("https://other.example.com", "other-token")
            assertEquals(0, coordinator.counts().first)
            try { coordinator.approve(old); fail("stale destination approval") } catch (_: IllegalStateException) {}
            coordinator.saveSettings(server.url("/").toString(), "device-token")
            coordinator.approve(coordinator.review(selected.dir.absolutePath))
            server.enqueue(MockResponse().setBody("{\"id\":\"accepted\"}"))
            assertEquals(1, coordinator.flush().sent)
            val request = server.takeRequest(5, TimeUnit.SECONDS)!!
            assertEquals("device-uuid", request.getHeader("X-Bris-Device-Uuid"))
            assertEquals("Bearer device-token", request.getHeader("Authorization"))
            val payload = request.body.readUtf8()
            assertTrue(payload.contains(File(capture, "bundle.json").readText()))
            assertTrue(payload.contains("12.34"))
            assertTrue(payload.contains("45.0"))
        } finally { job.cancelAndJoin(); server.shutdown() }
    }

    @Test fun silentCompletionRestartRetryAndToggleOffCancelRealNativeRequest() = runBlocking {
        var job = SupervisorJob()
        val file = File(tmp.root, "prefs.preferences_pb")
        fun store() = CollectorSettingsStore(PreferenceDataStoreFactory.create(
            scope = CoroutineScope(Dispatchers.IO + job)) { file })
        var settings = store()
        val server = MockWebServer()
        server.start()
        try {
            val capture = bundle()
            var coordinator = UploadCoordinator(port(), settings) { "device-uuid" }
            coordinator.saveSettings(server.url("/").toString(), "token")
            coordinator.captureCompleted(capture.absolutePath)
            assertEquals(0, coordinator.counts().first)
            coordinator.setAutomaticUpload(true)
            coordinator.captureCompleted(capture.absolutePath)
            assertFalse(settings.current().debugMode)
            server.enqueue(MockResponse().setResponseCode(503))
            assertTrue(coordinator.flush().needsRetry)
            assertNotNull(server.takeRequest(5, TimeUnit.SECONDS))
            job.cancelAndJoin()
            job = SupervisorJob()
            settings = store()
            coordinator = UploadCoordinator(port(), settings) { "device-uuid" }
            assertTrue(settings.current().automaticUpload)
            assertEquals(1, coordinator.counts().first)
            // Actual persisted native backoff, not a fake port's retry result.
            delay(5100)
            server.enqueue(MockResponse().setBody("{\"id\":\"retried\"}"))
            assertEquals(1, coordinator.flush().sent)
            assertNotNull(server.takeRequest(5, TimeUnit.SECONDS))
            coordinator.captureCompleted(capture.absolutePath)
            server.enqueue(MockResponse().setSocketPolicy(SocketPolicy.NO_RESPONSE))
            val current = coordinator
            val flushing = async(Dispatchers.Default) { runCatching { current.flush() } }
            assertNotNull(server.takeRequest(5, TimeUnit.SECONDS))
            coordinator.setAutomaticUpload(false)
            withTimeout(2000) { flushing.await() }
            assertEquals(0, coordinator.counts().first)
            coordinator.setAutomaticUpload(true)
            assertFalse(coordinator.flush().needsRetry)
            assertEquals(3, server.requestCount)
        } finally { job.cancelAndJoin(); server.shutdown() }
    }

    @Test fun onBeforeConfigurationThenSaveAndRegisterDeliversSameArtifact() = runBlocking {
        val job = SupervisorJob()
        val settings = CollectorSettingsStore(PreferenceDataStoreFactory.create(
            scope = CoroutineScope(Dispatchers.IO + job)) { File(tmp.root, "prefs.preferences_pb") })
        val server = MockWebServer()
        server.start()
        try {
            val capture = bundle()
            val coordinator = UploadCoordinator(port(), settings) { "device-uuid" }
            coordinator.setAutomaticUpload(true)
            coordinator.captureCompleted(capture.absolutePath)
            assertEquals(1, coordinator.counts().first)
            // Settings "Save & register": save, then register with the enrollment token.
            coordinator.saveSettings(server.url("/").toString(), "enroll-admin")
            assertEquals(1, coordinator.counts().first)
            assertEquals(0, coordinator.flush().sent) // held from the enrollment credential
            assertEquals(0, server.requestCount)
            server.enqueue(MockResponse().setBody("{\"token\":\"issued-device\"}"))
            assertEquals(RegistrationResult.Registered("issued-device"), coordinator.register())
            val reg = server.takeRequest(5, TimeUnit.SECONDS)!!
            assertEquals("/v1/devices/register", reg.path)
            assertEquals(1, coordinator.counts().first)
            server.enqueue(MockResponse().setBody("{\"id\":\"accepted\"}"))
            assertEquals(1, coordinator.flush().sent)
            val submit = server.takeRequest(5, TimeUnit.SECONDS)!!
            assertEquals("Bearer issued-device", submit.getHeader("Authorization"))
            assertTrue(submit.body.readUtf8().contains(File(capture, "bundle.json").readText()))
            assertEquals(0, coordinator.counts().first)
            assertEquals(2, server.requestCount)
        } finally { job.cancelAndJoin(); server.shutdown() }
    }
}
