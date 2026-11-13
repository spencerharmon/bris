package io.github.spencerharmon.bris.upload

import android.Manifest
import android.content.Context
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsOff
import androidx.compose.ui.test.assertIsOn
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performTextReplacement
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.core.app.ActivityScenario
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.rule.GrantPermissionRule
import io.github.spencerharmon.bris.MainActivity
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.ui.AUTOMATIC_UPLOAD_SWITCH_TAG
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.io.InputStream
import java.io.OutputStream
import java.net.HttpURLConnection
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.URL
import java.security.MessageDigest
import java.util.UUID
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference

/**
 * ROI rule 1b acceptance on Android (auto-upload-collection-acceptance).
 *
 * Drives the SHIPPING path end to end against a REAL `bris-collector` that the
 * connected-test harness (`scripts/connected_tests.py`) starts on the host with
 * temporary durable storage and passes in as instrumentation arguments:
 *
 *   Settings UI (URL/token, "Save & register" -> per-device token, the
 *   "Automatic upload" switch) -> MainActivity Start/Stop capture -> CaptureRecorder
 *   -> the production `onBundleCompleted` callback -> [Upload.captureCompleted]
 *   -> UploadCoordinator -> generated UniFFI `Submitter` (real native queue)
 *   -> WorkManager [FlushWorker] -> HTTP -> collector.
 *
 * The app talks to the collector through [Relay], a byte-level TCP relay on the
 * device loopback the test can take offline or make hold an in-flight
 * transfer; it never parses or answers HTTP itself. Verification reads the
 * collector's own admin API (manifest + stored media bytes). The harness then
 * replays stored submissions in place with `bris replay`. Debug-off recordings
 * may legitimately have zero retained frames; the synthetic two-frame captures
 * below separately exercise frame/sidecar preservation and replay.
 *
 * Run the lifecycle method on fresh app data. Run preparePendingRestart on fresh
 * app data, then force-stop the target and invoke reopenPendingRestart in a NEW
 * instrumentation process, without reinstalling/clearing either package. Both
 * phases require the same collector URL/admin token and durable collector store.
 * Do not run this entire class in one instrumentation invocation.
 */
@RunWith(AndroidJUnit4::class)
class AutoUploadAcceptanceTest {

    @get:Rule
    val compose = createEmptyComposeRule()

    @get:Rule
    val cameraPermission: GrantPermissionRule = GrantPermissionRule.grant(Manifest.permission.CAMERA)

    private val context: Context =
        InstrumentationRegistry.getInstrumentation().targetContext.applicationContext
    private val args = InstrumentationRegistry.getArguments()
    private val collectorUrl = checkNotNull(args.getString("brisCollectorUrl")) {
        "brisCollectorUrl instrumentation argument missing (run via scripts/connected_tests.py)"
    }.trimEnd('/')
    private val adminToken = checkNotNull(args.getString("brisCollectorAdminToken")) {
        "brisCollectorAdminToken instrumentation argument missing"
    }
    private val collector = URL(collectorUrl)
    private lateinit var gate: Relay
    private val admin by lazy { Relay(collector.host, collector.port) }
    private var activity: ActivityScenario<MainActivity>? = null
    private val restartState get() = File(context.filesDir, "auto-upload-pending-restart.json")

    @After
    fun tearDown() {
        activity?.close()
        if (::gate.isInitialized) gate.close()
        admin.close()
    }

    @Test
    fun automaticUploadIsSilentDurableVisibleAndRevocable() = runBlocking {
        val store = Upload.settings(context)
        val uuid = Prefs(context).deviceUuid()
        gate = Relay(collector.host, collector.port)
        launchApp()
        createSession()
        openSettings()

        // ---- Default: toggle OFF, debug OFF. ----
        assertFalse("automatic upload must default OFF", store.current().automaticUpload)
        switch().performScrollTo().assertIsOff()
        assertDebugOff()

        // ---- Settings UI: configure + register a per-device token. ----
        registerThroughSettings()
        assertTrue(store.current().deviceToken != adminToken)
        val baseline = collectorIds().toSet()

        // ---- OFF: a completed session sends nothing. ----
        gate.connections.set(0)
        recordThroughUi()
        Thread.sleep(5_000)
        assertEquals("toggle OFF must not contact the collector", 0, gate.connections.get())
        assertEquals(0 to 0, Upload.coordinator(context).counts())
        openSettings()

        // ---- ON via the Settings switch: silent, no Debug, no review. ----
        switch().performScrollTo().performClick()
        waitFor("toggle ON persisted") { store.current().automaticUpload }
        switch().assertIsOn()
        val on = store.current()
        assertFalse(on.debugMode)
        assertNull("no manual/review scope exists on the automatic path", on.manualScope)
        // Keep forwarding down until the recorder's finalized source artifacts
        // have been hashed. No test completion/enqueue call is made for this capture.
        gate.mode.set(Relay.Mode.DOWN)
        val recorded = recordThroughUi()
        val recordedHashes = artifactHashes(recorded)
        openSettings()
        assertQueue(1, 0)
        gate.mode.set(Relay.Mode.FORWARD)
        assertStored(waitForNewSubmission(baseline, 240_000), recordedHashes, uuid)
        assertQueue(0, 0)

        // Retain a deterministic two-frame replay fixture in addition to the UI recording.
        val first = capture("online")
        val firstHashes = artifactHashes(first)
        val beforeFirst = collectorIds().toSet()
        Upload.captureCompleted(context, first)
        val firstId = waitForNewSubmission(beforeFirst)
        assertStored(firstId, firstHashes, uuid)
        assertNoReview()
        waitFor("queue drained") { Upload.coordinator(context).counts() == (0 to 0) }

        // ---- Offline: visible pending, WorkManager retry delivers after recovery. ----
        gate.mode.set(Relay.Mode.DOWN)
        val offline = capture("offline")
        val offlineHashes = artifactHashes(offline)
        Upload.captureCompleted(context, offline)
        waitFor("pending visible in Settings", 30_000) {
            compose.onAllNodesWithText("Queue: 1 pending, 0 rejected").fetchSemanticsNodes().isNotEmpty()
        }
        waitFor("worker observed the transient failure", 60_000) {
            store.current().lastError?.contains("will retry") == true
        }
        // This is network recovery only. Real process recovery is tested separately.
        assertEquals(1, File(FfiSubmitterPort.queueRoot(context), "pending").listFiles()!!.count { it.isDirectory })
        val known = collectorIds().toSet()
        gate.mode.set(Relay.Mode.FORWARD)
        val retriedId = waitForNewSubmission(known, timeoutMs = 240_000)
        assertStored(retriedId, offlineHashes, uuid)
        waitFor("Settings shows drained queue", 30_000) {
            compose.onAllNodesWithText("Queue: 0 pending, 0 rejected").fetchSemanticsNodes().isNotEmpty()
        }

        // ---- Toggle OFF revokes a queued retry: no new requests. ----
        gate.mode.set(Relay.Mode.DOWN)
        Upload.captureCompleted(context, capture("revoked"))
        waitFor("revoked capture pending", 30_000) { Upload.coordinator(context).counts().first == 1 }
        switch().performScrollTo().performClick()
        waitFor("toggle OFF persisted") { !store.current().automaticUpload }
        waitFor("queued retry revoked", 30_000) { Upload.coordinator(context).counts().first == 0 }
        val afterRevoke = collectorIds().toSet()
        gate.mode.set(Relay.Mode.FORWARD)
        gate.connections.set(0)
        compose.onNodeWithText("Upload now").performScrollTo().performClick()
        Upload.captureCompleted(context, capture("off-again"))
        Thread.sleep(75_000) // past the 30 s WorkManager backoff + one doubling
        assertEquals("no requests after toggle-off", 0, gate.connections.get())
        assertEquals(afterRevoke, collectorIds().toSet())

        // ---- Active transfer is cancelled by toggle-off. ----
        switch().performScrollTo().performClick()
        waitFor("toggle ON again") { store.current().automaticUpload }
        gate.mode.set(Relay.Mode.HOLD)
        gate.connections.set(0)
        Upload.captureCompleted(context, capture("held"))
        waitFor("transfer in flight", 60_000) { gate.held.get() != null }
        val held = gate.held.get()!!
        switch().performScrollTo().performClick()
        waitFor("client closed the in-flight transfer", 10_000) { held.closed }
        gate.mode.set(Relay.Mode.FORWARD)
        val heldConnections = gate.connections.get()
        Thread.sleep(45_000)
        assertEquals("no new connection after cancellation", heldConnections, gate.connections.get())
        assertEquals(afterRevoke, collectorIds().toSet())
        assertEquals(0, Upload.coordinator(context).counts().first)

        // ---- A REAL collector 401 is terminal and visible, not a transient retry. ----
        // Rotating server-side credentials leaves the app's registered token stale
        // without changing its local authorization scope or manufacturing an HTTP reply.
        rotateDeviceToken(uuid)
        switch().performScrollTo().performClick()
        waitFor("toggle ON for rejection") { store.current().automaticUpload }
        val beforeRejected = collectorIds().toSet()
        val rejectedCount = Upload.coordinator(context).counts().second + 1
        val connectionsBeforeRejection = gate.connections.get()
        Upload.captureCompleted(context, capture("rejected"))
        assertQueue(0, rejectedCount)
        waitFor("visible permanent rejection") {
            compose.onAllNodesWithText("Last error: rejected", substring = true)
                .fetchSemanticsNodes().isNotEmpty()
        }
        compose.onNodeWithText("Last error: rejected", substring = true)
            .performScrollTo().assertIsDisplayed()
        assertTrue("collector must reject the upload with HTTP 401",
            store.current().lastError.orEmpty().contains("401"))
        assertTrue(gate.connections.get() > connectionsBeforeRejection)
        val rejectedConnections = gate.connections.get()
        // An explicit drain plus a full retry window must not retry a dead letter.
        compose.onNodeWithText("Upload now").performScrollTo().performClick()
        Thread.sleep(75_000)
        assertEquals("rejected entry must not be retried", rejectedConnections, gate.connections.get())
        assertEquals(beforeRejected, collectorIds().toSet())
        assertQueue(0, rejectedCount)
    }

    /** First standalone harness invocation; leaves consent ON and one durable pending item. */
    @Test
    fun preparePendingRestart() = runBlocking {
        check(!restartState.exists()) { "preparePendingRestart requires fresh app data" }
        gate = Relay(collector.host, collector.port)
        launchApp()
        createSession()
        openSettings()
        switch().performScrollTo().assertIsOff()
        assertDebugOff()
        registerThroughSettings()
        switch().performScrollTo().performClick()
        waitFor("restart consent persisted") { Upload.settings(context).current().automaticUpload }
        gate.mode.set(Relay.Mode.DOWN)
        val baseline = collectorIds().toSet()
        val recorded = recordThroughUi()
        val hashes = artifactHashes(recorded)
        openSettings()
        assertQueue(1, 0)
        waitFor("restart pending upload attempted offline", 60_000) {
            Upload.settings(context).current().lastError?.contains("will retry") == true
        }
        val pending = File(FfiSubmitterPort.queueRoot(context), "pending")
            .listFiles()!!.filter { it.isDirectory }.single()
        val settings = Upload.settings(context).current()
        val state = JSONObject()
            .put("processIdentity", AcceptanceProcess.identity)
            .put("collectorUrl", collectorUrl)
            .put("relayPort", gate.localPort)
            .put("deviceUuid", Prefs(context).deviceUuid())
            .put("automaticScope", checkNotNull(settings.automaticScope))
            .put("pendingId", pending.name)
            .put("capturePath", recorded.absolutePath)
            .put("hashes", JSONObject(hashes))
            .put("collectorIds", JSONArray(baseline.toList()))
        restartState.outputStream().use { out ->
            out.write(state.toString().toByteArray(Charsets.UTF_8))
            out.fd.sync()
        }
        assertEquals(baseline, collectorIds().toSet())
        switch().performScrollTo().assertIsOn()
        assertDebugOff()
        // Teardown closes only the activity/relays, never the queue or consent.
    }

    /** Second standalone invocation after force-stop, with the SAME app and collector data. */
    @Test
    fun reopenPendingRestart() = runBlocking {
        check(restartState.isFile) { "run preparePendingRestart first, without clearing app data" }
        val state = JSONObject(restartState.readText())
        assertFalse("restart phases must execute in different app processes",
            state.getString("processIdentity") == AcceptanceProcess.identity)
        assertEquals("collector must survive both phases", state.getString("collectorUrl"), collectorUrl)
        // Bind the persisted endpoint DOWN before launching MainActivity. Never save
        // settings here: changing the endpoint/scope would revoke the pending entry.
        gate = Relay(collector.host, collector.port, state.getInt("relayPort"), Relay.Mode.DOWN)
        val savedIds = state.getJSONArray("collectorIds")
        val baseline = (0 until savedIds.length()).map { savedIds.getString(it) }.toSet()
        assertEquals("no delivery across the offline boundary", baseline, collectorIds().toSet())
        val expectedJson = state.getJSONObject("hashes")
        val expected = expectedJson.keys().asSequence().associateWith { expectedJson.getString(it) }
        assertEquals(expected, artifactHashes(File(state.getString("capturePath"))))
        assertTrue("the same pending entry must survive process death",
            File(FfiSubmitterPort.queueRoot(context), "pending/${state.getString("pendingId")}").isDirectory)

        launchApp()
        openSettings()
        val settings = Upload.settings(context).current()
        assertEquals(gate.url, settings.baseUrl)
        assertEquals(state.getString("automaticScope"), settings.automaticScope)
        assertEquals(state.getString("deviceUuid"), Prefs(context).deviceUuid())
        waitFor("persisted ON rendered") { settingsOnScreen() }
        switch().performScrollTo().assertIsOn()
        assertDebugOff()
        assertQueue(1, 0)
        assertEquals(baseline, collectorIds().toSet())

        // Only restore the network. MainActivity startup / persisted WorkManager
        // work must deliver; no captureCompleted, scheduleFlush, or Upload-now call.
        gate.mode.set(Relay.Mode.FORWARD)
        val id = waitForNewSubmission(baseline, 240_000)
        assertStored(id, expected, state.getString("deviceUuid"))
        assertQueue(0, 0)
        assertNoReview()
        switch().performScrollTo().assertIsOn()
        assertDebugOff()
        assertEquals(baseline + id, collectorIds().toSet())
    }

    // ------------------------------------------------------------ helpers

    private fun switch() = compose.onNodeWithTag(AUTOMATIC_UPLOAD_SWITCH_TAG)

    private fun launchApp() {
        activity = ActivityScenario.launch(MainActivity::class.java)
        waitFor("real MainActivity live screen") {
            compose.onAllNodesWithText("Start capture").fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun openSettings() {
        compose.onNodeWithText("Settings").performClick()
        waitFor("Settings collector controls") {
            compose.onAllNodesWithText("Automatic upload").fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun createSession() {
        // Select the session BEFORE Start, avoiding LiveScreen's documented
        // first-start fallback which replaces its recorder mid-capture.
        compose.onNodeWithText("Sessions").performClick()
        compose.onNodeWithText("+ New session").performClick()
        waitFor("session editor") { compose.onAllNodesWithText("Title").fetchSemanticsNodes().isNotEmpty() }
        waitFor("active session persisted") { Prefs(context).activeSessionIdFlow.first() != null }
        compose.onNodeWithText("Back").performScrollTo().performClick()
        compose.onNodeWithText("Back").performClick()
    }

    private fun registerThroughSettings() {
        compose.onNodeWithText("Base URL").performScrollTo().performTextReplacement(gate.url)
        compose.onNodeWithText("Token (enrollment or device)").performScrollTo()
            .performTextReplacement(adminToken)
        compose.onNodeWithText("Save & register").performScrollTo().performClick()
        waitFor("per-device registration") { Upload.settings(context).current().registered }
    }

    private fun assertDebugOff() {
        assertFalse(runBlocking { Prefs(context).debugModeFlow.first() })
        compose.onNodeWithTag("debug-mode-switch")
            .performScrollTo().assertIsOff()
    }

    private fun settingsOnScreen(): Boolean =
        compose.onAllNodesWithText("Automatic upload").fetchSemanticsNodes().isNotEmpty() &&
            runCatching { switch().assertIsOn(); true }.getOrDefault(false)

    private fun assertQueue(pending: Int, rejected: Int) {
        val text = "Queue: $pending pending, $rejected rejected"
        waitFor(text, 60_000) { compose.onAllNodesWithText(text).fetchSemanticsNodes().isNotEmpty() }
        compose.onNodeWithText(text).performScrollTo().assertIsDisplayed()
    }

    private fun assertNoReview() {
        compose.onNodeWithText("Submit to collector").assertDoesNotExist()
        compose.onNodeWithText("Approve & upload").assertDoesNotExist()
        compose.onNodeWithText("Cannot prepare submission:", substring = true).assertDoesNotExist()
    }

    /** Called from Settings; returns on the live screen, after the production callback. */
    private fun recordThroughUi(): File {
        val sessionId = runBlocking { checkNotNull(Prefs(context).activeSessionIdFlow.first()) }
        val root = File(context.getExternalFilesDir(null) ?: context.filesDir, "sessions/$sessionId/captures")
        val before = root.listFiles()?.map { it.name }?.toSet().orEmpty()
        compose.onNodeWithText("Back").performScrollTo().performClick()
        fun framesPushed(): Long {
            val nodes = compose.onAllNodesWithText("frames pushed:", substring = true).fetchSemanticsNodes()
            val text = nodes.single().config[SemanticsProperties.Text].joinToString(" ") { it.text }
            return checkNotNull(Regex("frames pushed: ([0-9]+)").find(text)).groupValues[1].toLong()
        }
        val framesBefore = framesPushed()
        compose.onNodeWithText("Start capture").assertIsDisplayed().performClick()
        compose.onNodeWithText("Stop capture").assertIsDisplayed()
        waitFor("real CameraX analyzer delivered a frame", 60_000) { framesPushed() > framesBefore }
        compose.onNodeWithText("Stop capture").performClick()
        waitFor("CaptureRecorder finalized through its callback", 60_000) {
            compose.onAllNodesWithText("No fix recorded", substring = true).fetchSemanticsNodes().isNotEmpty() ||
                compose.onAllNodesWithText("Saved to", substring = true).fetchSemanticsNodes().isNotEmpty()
        }
        compose.onNodeWithText("Start capture").assertIsDisplayed()
        assertNoReview()
        val dir = root.listFiles()!!.filter { it.isDirectory && it.name !in before }.single()
        assertTrue("recorder must write sight-log manifest", File(dir, "manifest.json").isFile)
        assertTrue("recorder must write bundle before callback", File(dir, "bundle.json").isFile)
        assertTrue("debug-off completion retains diagnostics", File(dir, "pbris.log").isFile)
        assertTrue("debug-off capture must not attach GPS truth",
            JSONObject(File(dir, "bundle.json").readText()).isNull("gps_truth"))
        File(dir, "frames").listFiles()!!.filter { it.extension == "json" }.forEach {
            assertEquals("debug-off retains only fix frames", "fix_frame", JSONObject(it.readText()).getString("retention"))
        }
        return dir
    }

    private fun waitFor(what: String, timeoutMs: Long = 20_000, cond: suspend () -> Boolean) = runBlocking {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (true) {
            compose.waitForIdle()
            assertNoReview()
            if (cond()) return@runBlocking
            check(System.currentTimeMillis() < deadline) { "timed out waiting for: $what" }
            Thread.sleep(250)
        }
    }

    /** A real two-frame bris-bundle v1 capture with honest ap_input + distinct gps_truth. */
    private fun capture(tag: String): File {
        val dir = File(context.filesDir, "acceptance-captures/$tag-${System.nanoTime()}")
        val frames = File(dir, "frames").apply { mkdirs() }
        for (seq in 0..1) {
            val stem = "%08d".format(seq)
            File(frames, "$stem.pgm").writeBytes("P5\n4 3\n255\n".toByteArray() + ByteArray(12) { (seq * 40).toByte() })
            File(frames, "$stem.json").writeText(
                JSONObject().put("seq", seq).put("width", 4).put("height", 3)
                    .put("captured_unix_ms", 1_700_000_000_000L + seq * 1000).toString(),
            )
        }
        val bundle = JSONObject()
            .put("schema_version", 1)
            .put("bundle_id", dir.name)
            .put("device", JSONObject().put("model", "android-acceptance"))
            .put("capture", JSONObject().put("source_rotation_deg", 0).put("frame_count", 2)
                .put("started_unix_ms", 1_700_000_000_000L).put("ended_unix_ms", 1_700_000_001_000L))
            .put("intrinsics", JSONObject().put("source", JSONObject().put("kind", "placeholder"))
                .put("width", 4).put("height", 3).put("fx", 1000.0).put("fy", 1000.0)
                .put("cx", 2.0).put("cy", 1.5).put("distortion", JSONObject().put("model", "none")))
            .put("ap_input", JSONObject().put("lat", 12.34).put("lon", -56.78)
                .put("eye_height_m", 2.0).put("provenance", "operator_entered"))
            .put("gps_truth", JSONObject().put("lat", 41.5).put("lon", -70.25)
                .put("lat_sigma_m", 5.0).put("lon_sigma_m", 7.0)
                .put("captured_unix_ms", 1_700_000_000_500L).put("source", "phone_gnss"))
        File(dir, "bundle.json").writeText(bundle.toString())
        return dir
    }

    private fun get(path: String): ByteArray {
        val conn = URL(admin.url + path).openConnection() as HttpURLConnection
        conn.setRequestProperty("Authorization", "Bearer $adminToken")
        conn.connectTimeout = 10_000
        conn.readTimeout = 10_000
        try {
            check(conn.responseCode == 200) { "GET $path -> ${conn.responseCode}" }
            return conn.inputStream.use { it.readBytes() }
        } finally {
            conn.disconnect()
        }
    }

    private fun rotateDeviceToken(uuid: String) {
        val conn = URL(admin.url + "/v1/devices/register").openConnection() as HttpURLConnection
        conn.requestMethod = "POST"
        conn.setRequestProperty("Authorization", "Bearer $adminToken")
        conn.setRequestProperty("Content-Type", "application/json")
        conn.connectTimeout = 10_000
        conn.readTimeout = 10_000
        conn.doOutput = true
        try {
            conn.outputStream.use { it.write(JSONObject().put("device_uuid", uuid).toString().toByteArray()) }
            assertEquals("real collector token rotation", 200, conn.responseCode)
            val rotated = conn.inputStream.use { JSONObject(String(it.readBytes())).getString("token") }
            assertFalse("server must invalidate the app's token",
                rotated == runBlocking { Upload.settings(context).current().deviceToken })
        } finally {
            conn.disconnect()
        }
    }

    private fun collectorIds(): List<String> {
        val list = JSONArray(String(get("/v1/submissions")))
        return (0 until list.length()).map { list.getJSONObject(it).getString("id") }
    }

    private fun waitForNewSubmission(known: Set<String>, timeoutMs: Long = 120_000): String {
        var found: String? = null
        waitFor("collector receipt", timeoutMs) {
            found = (collectorIds().toSet() - known).singleOrNull()
            found != null
        }
        return found!!
    }

    private fun sha256(bytes: ByteArray) =
        MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }

    /** Independent source inventory, captured before forwarding/enqueue, not upload-manifest hashes. */
    private fun artifactHashes(capture: File): Map<String, String> {
        // manifest.json is the local sight-log entry, NOT a collector media part.
        // The wire contract includes bundle, every frame/sidecar, and optional logs/index.
        val files = listOf(File(capture, "bundle.json")) +
            listOf("pbris.log", "index.jsonl").map { File(capture, it) }.filter { it.isFile } +
            File(capture, "frames").walkTopDown().filter { it.isFile }.toList()
        assertEquals("flattened media filenames must be unique", files.size, files.map { it.name }.toSet().size)
        return files.associate { it.name to sha256(it.readBytes()) }
    }

    /** Schema, identity, exact source inventory and every source digest, including all frame bytes. */
    private fun assertStored(id: String, expected: Map<String, String>, uuid: String) {
        val manifest = JSONObject(String(get("/v1/submissions/$id")))
        assertEquals(1, manifest.getInt("schema_version"))
        assertEquals("manifest identity", uuid, manifest.getJSONObject("device").getString("uuid"))
        val media = manifest.getJSONArray("media")
        val names = (0 until media.length()).map { media.getJSONObject(it).getString("filename") }
        assertEquals("no missing, extra, or duplicate media", expected.size, names.size)
        assertEquals("exact source artifact inventory", expected.keys, names.toSet())
        for (i in 0 until media.length()) {
            val item = media.getJSONObject(i)
            val name = item.getString("filename")
            val bytes = get("/v1/submissions/$id/media/$name")
            assertEquals("unchanged source bytes: $name", expected.getValue(name), sha256(bytes))
            assertEquals("manifest hash: $name", expected.getValue(name), item.getString("checksum_sha256"))
            assertEquals("manifest size: $name", bytes.size.toLong(), item.getLong("size_bytes"))
        }
    }
}

/** A same-process invocation cannot masquerade as a restart, even if its test instance is new. */
private object AcceptanceProcess {
    val identity: String = UUID.randomUUID().toString()
}

/**
 * Device-loopback TCP relay to the host collector. FORWARD pipes bytes both
 * ways; DOWN resets every connection attempt (still counted) without forwarding;
 * HOLD accepts and drains the client without ever forwarding, recording when
 * the client closes. It never interprets HTTP.
 */
class Relay(
    private val host: String,
    private val port: Int,
    bindPort: Int = 0,
    initialMode: Mode = Mode.FORWARD,
) {
    enum class Mode { FORWARD, DOWN, HOLD }

    class Held(val socket: Socket) {
        @Volatile var closed = false
    }

    val mode = AtomicReference(initialMode)
    val connections = AtomicInteger(0)
    val held = AtomicReference<Held?>(null)
    val localPort: Int
    @Volatile private var server: ServerSocket? = null
    @Volatile private var stopped = false
    private val loopback = InetAddress.getByName("127.0.0.1")

    init {
        val s = ServerSocket().apply {
            reuseAddress = true
            bind(InetSocketAddress(loopback, bindPort), 50)
        }
        localPort = s.localPort
        server = s
        Thread({ supervise() }, "relay-$localPort").apply { isDaemon = true }.start()
    }

    val url: String get() = "http://127.0.0.1:$localPort"

    private fun supervise() {
        while (!stopped) {
            val listener = server ?: break
            listener.soTimeout = 100
            val client = try { listener.accept() } catch (e: java.io.IOException) { continue }
            connections.incrementAndGet()
            when (mode.get()) {
                Mode.DOWN -> {
                    // Offline: every attempt is counted and reset before any byte
                    // is forwarded, so the client sees a transport failure.
                    client.setSoLinger(true, 0)
                    runCatching { client.close() }
                }
                Mode.HOLD -> {
                    val h = Held(client)
                    held.set(h)
                    Thread {
                        try {
                            val input = client.getInputStream()
                            val buf = ByteArray(8192)
                            while (input.read(buf) >= 0) { /* drain, never forward */ }
                        } catch (_: java.io.IOException) {
                        }
                        h.closed = true
                        runCatching { client.close() }
                    }.apply { isDaemon = true }.start()
                }
                Mode.FORWARD -> {
                    val upstream = try { Socket(host, port) } catch (e: java.io.IOException) {
                        client.close(); continue
                    }
                    pipe(client.getInputStream(), upstream.getOutputStream(), client, upstream)
                    pipe(upstream.getInputStream(), client.getOutputStream(), client, upstream)
                }
            }
        }
        server?.close()
    }

    private fun pipe(input: InputStream, output: OutputStream, a: Socket, b: Socket) {
        Thread {
            try {
                input.copyTo(output)
                output.flush()
            } catch (_: java.io.IOException) {
            } finally {
                runCatching { a.close() }
                runCatching { b.close() }
            }
        }.apply { isDaemon = true }.start()
    }

    fun close() {
        stopped = true
        runCatching { server?.close() }
    }
}
