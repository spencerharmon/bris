package io.github.spencerharmon.bris.upload

import android.content.Context
import androidx.compose.ui.test.assertIsOff
import androidx.compose.ui.test.assertIsOn
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performTextReplacement
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.ui.AUTOMATIC_UPLOAD_SWITCH_TAG
import io.github.spencerharmon.bris.ui.SettingsScreen
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
 *   "Automatic upload" switch) -> [Upload.captureCompleted] (the exact
 *   `onBundleCompleted` session callback LiveScreen wires into CaptureRecorder)
 *   -> UploadCoordinator -> generated UniFFI `Submitter` (real native queue)
 *   -> WorkManager [FlushWorker] -> HTTP -> collector.
 *
 * The app talks to the collector through [Relay], a byte-level TCP relay on the
 * device loopback the test can take offline or make hold an in-flight
 * transfer; it never parses or answers HTTP itself. Verification reads the
 * collector's own admin API (manifest + stored media bytes). The harness then
 * replays every stored submission in place with `bris replay`.
 */
@RunWith(AndroidJUnit4::class)
class AutoUploadAcceptanceTest {

    @get:Rule
    val compose = createComposeRule()

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
    private val gate = Relay(collector.host, collector.port)
    private val admin = Relay(collector.host, collector.port)

    @After
    fun tearDown() {
        gate.close()
        admin.close()
    }

    @Test
    fun automaticUploadIsSilentDurableVisibleAndRevocable() = runBlocking {
        val store = Upload.settings(context)
        val uuid = Prefs(context).deviceUuid()
        compose.setContent { SettingsScreen(Prefs(context), onBack = {}) }

        // ---- Default: toggle OFF, debug OFF. ----
        assertFalse("automatic upload must default OFF", store.current().automaticUpload)
        switch().performScrollTo().assertIsOff()

        // ---- Settings UI: configure + register a per-device token. ----
        compose.onNodeWithText("Base URL").performScrollTo().performTextReplacement(gate.url)
        compose.onNodeWithText("Token (enrollment or device)").performScrollTo()
            .performTextReplacement(adminToken)
        compose.onNodeWithText("Save & register").performScrollTo().performClick()
        waitFor("per-device registration") { store.current().registered }
        assertTrue(store.current().deviceToken != adminToken)
        val baseline = collectorIds().toSet()

        // ---- OFF: a completed session sends nothing. ----
        gate.connections.set(0)
        Upload.captureCompleted(context, capture("off"))
        Thread.sleep(5_000)
        assertEquals("toggle OFF must not contact the collector", 0, gate.connections.get())
        assertEquals(0 to 0, Upload.coordinator(context).counts())

        // ---- ON via the Settings switch: silent, no Debug, no review. ----
        switch().performScrollTo().performClick()
        waitFor("toggle ON persisted") { store.current().automaticUpload }
        switch().assertIsOn()
        val on = store.current()
        assertFalse(on.debugMode)
        assertNull("no manual/review scope exists on the automatic path", on.manualScope)
        val first = capture("online")
        Upload.captureCompleted(context, first)
        val firstId = waitForNewSubmission(baseline)
        assertStored(firstId, first, uuid)
        compose.onAllNodesWithText("Pre-upload review", substring = true).assertCountEquals(0)
        waitFor("queue drained") { Upload.coordinator(context).counts() == (0 to 0) }

        // ---- Offline: visible pending, WorkManager retry delivers after recovery. ----
        gate.mode.set(Relay.Mode.DOWN)
        val offline = capture("offline")
        Upload.captureCompleted(context, offline)
        waitFor("pending visible in Settings", 30_000) {
            compose.onAllNodesWithText("Queue: 1 pending, 0 rejected").fetchSemanticsNodes().isNotEmpty()
        }
        waitFor("worker observed the transient failure", 60_000) {
            store.current().lastError?.contains("will retry") == true
        }
        // Reopen: the entry is durable on disk, independent of in-memory state.
        assertEquals(1, File(FfiSubmitterPort.queueRoot(context), "pending").listFiles()!!.count { it.isDirectory })
        gate.mode.set(Relay.Mode.FORWARD)
        val known = collectorIds().toSet()
        val retriedId = waitForNewSubmission(known, timeoutMs = 240_000)
        assertStored(retriedId, offline, uuid)
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
    }

    // ------------------------------------------------------------ helpers

    private fun switch() = compose.onNodeWithTag(AUTOMATIC_UPLOAD_SWITCH_TAG)

    private fun androidx.compose.ui.test.SemanticsNodeInteractionCollection.assertCountEquals(n: Int) =
        assertEquals(n, fetchSemanticsNodes().size)

    private fun waitFor(what: String, timeoutMs: Long = 20_000, cond: suspend () -> Boolean) = runBlocking {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (true) {
            compose.waitForIdle()
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

    /** Schema, manifest identity, unchanged bundle.json, every checksum. */
    private fun assertStored(id: String, capture: File, uuid: String) {
        val manifest = JSONObject(String(get("/v1/submissions/$id")))
        assertEquals(1, manifest.getInt("schema_version"))
        assertEquals("manifest identity", uuid, manifest.getJSONObject("device").getString("uuid"))
        val media = manifest.getJSONArray("media")
        val names = (0 until media.length()).map { media.getJSONObject(it).getString("filename") }
        assertTrue(names.toString(), "bundle.json" in names)
        assertTrue(names.toString(), names.count { it.endsWith(".pgm") } == 2)
        for (i in 0 until media.length()) {
            val item = media.getJSONObject(i)
            val bytes = get("/v1/submissions/$id/media/${item.getString("filename")}")
            assertEquals(item.getString("checksum_sha256"), sha256(bytes))
        }
        val stored = get("/v1/submissions/$id/media/bundle.json")
        assertTrue("engine bundle.json must arrive unchanged",
            stored.contentEquals(File(capture, "bundle.json").readBytes()))
    }
}

/**
 * Device-loopback TCP relay to the host collector. FORWARD pipes bytes both
 * ways; DOWN refuses connections (listener closed, same port reopened later);
 * HOLD accepts and drains the client without ever forwarding, recording when
 * the client closes. It never interprets HTTP.
 */
class Relay(private val host: String, private val port: Int) {
    enum class Mode { FORWARD, DOWN, HOLD }

    class Held(val socket: Socket) {
        @Volatile var closed = false
    }

    val mode = AtomicReference(Mode.FORWARD)
    val connections = AtomicInteger(0)
    val held = AtomicReference<Held?>(null)
    private val localPort: Int
    @Volatile private var server: ServerSocket? = null
    @Volatile private var stopped = false
    private val loopback = InetAddress.getByName("127.0.0.1")

    init {
        val s = ServerSocket(0, 50, loopback)
        localPort = s.localPort
        server = s
        Thread({ supervise() }, "relay-$localPort").apply { isDaemon = true }.start()
    }

    val url: String get() = "http://127.0.0.1:$localPort"

    private fun supervise() {
        while (!stopped) {
            val s = server
            if (mode.get() == Mode.DOWN) {
                s?.close()
                server = null
                Thread.sleep(50)
                continue
            }
            val listener = s ?: try {
                ServerSocket().apply {
                    reuseAddress = true
                    bind(InetSocketAddress(loopback, localPort))
                }.also { server = it }
            } catch (e: java.io.IOException) {
                Thread.sleep(50); continue
            }
            listener.soTimeout = 100
            val client = try { listener.accept() } catch (e: java.io.IOException) { continue }
            connections.incrementAndGet()
            when (mode.get()) {
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
                else -> {
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
