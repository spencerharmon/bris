package io.github.spencerharmon.bris.ui

import android.Manifest
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.rule.GrantPermissionRule
import androidx.work.WorkManager
import io.github.spencerharmon.bris.MainActivity
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.SessionHolder
import io.github.spencerharmon.bris.engine.SessionStore
import io.github.spencerharmon.bris.upload.FfiSubmitterPort
import io.github.spencerharmon.bris.upload.FlushWorker
import io.github.spencerharmon.bris.upload.Upload
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.rules.RuleChain
import java.io.File
import java.util.concurrent.TimeUnit

/** Real MainActivity -> CameraX -> recorder -> native queue -> Rust collector. */
class CaptureUploadTest {
    private val compose = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: RuleChain = RuleChain.outerRule(GrantPermissionRule.grant(Manifest.permission.CAMERA))
        .around(compose)

    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext
    private val arguments get() = InstrumentationRegistry.getArguments()
    private val baseUrl get() = requireNotNull(arguments.getString("collectorBaseUrl")) {
        "Run through bris-android/scripts/connected_tests.py (real collector required)"
    }
    private val token get() = requireNotNull(arguments.getString("collectorToken"))
    private val http = OkHttpClient.Builder().callTimeout(10, TimeUnit.SECONDS).build()

    @Test
    fun firstStartWithoutSessionsUploadsOnce() = capture(explicitSession = false)

    @Test
    fun explicitSessionUploadsOnce() = capture(explicitSession = true)

    private fun capture(explicitSession: Boolean) {
        val prefs = Prefs(context)
        val store = SessionStore.forApp(context)
        val root = context.getExternalFilesDir(null) ?: context.filesDir
        // Orchestrator clears package data between tests, not just session preferences.
        assertTrue("requires fresh app data", store.list().isEmpty())
        assertNull(runBlocking { prefs.activeSessionIdFlow.first() })
        assertFalse(runBlocking { prefs.debugModeFlow.first() })
        assertFalse(runBlocking { Upload.settings(context).current().automaticUpload })
        assertTrue(FfiSubmitterPort.queueRoot(context).walkTopDown().none { it.name == "state.json" })

        compose.onNodeWithText("Settings").performClick()
        compose.onNodeWithText("Base URL").performScrollTo().performTextReplacement(baseUrl)
        compose.onNodeWithText("Token (enrollment or device)").performScrollTo()
            .performTextReplacement(token)
        compose.onNodeWithText("Save & register").performScrollTo().performClick()
        compose.waitUntil(30_000) { runBlocking { Upload.settings(context).current().registered } }
        val automatic = compose.onNodeWithContentDescription("Automatic upload")
        automatic.performScrollTo().assertIsOff().performClick()
        compose.waitUntil(10_000) { runBlocking { Upload.settings(context).current().automaticUpload } }
        automatic.assertIsOn()
        compose.onNodeWithText("Back").performScrollTo().performClick()

        val explicitId = if (explicitSession) {
            compose.onNodeWithText("Sessions").performClick()
            compose.onNodeWithText("+ New session").performClick()
            compose.waitUntil(10_000) {
                compose.onAllNodesWithText("Title").fetchSemanticsNodes().isNotEmpty()
            }
            compose.onNodeWithText("Title").performTextReplacement("Connected explicit session")
            compose.onNodeWithText("Save").performScrollTo().performClick()
            compose.onNodeWithText("Use").performClick()
            compose.onNodeWithText("Back").performClick()
            store.list().single().also { assertEquals("Connected explicit session", it.title) }.sessionId
        } else {
            assertTrue("Settings must not create a session", store.list().isEmpty())
            assertNull(runBlocking { prefs.activeSessionIdFlow.first() })
            null
        }

        compose.onNodeWithText("Start capture").performClick()
        // Wait for real camera frames AND the preference-driven recomposition. The old
        // first-Start recorder disappears here, rather than surviving until Stop.
        compose.waitUntil(30_000) {
            runBlocking { prefs.activeSessionIdFlow.first() } != null &&
                (SessionHolder.peek()?.snapshot?.value?.framesPushed ?: 0uL) > 0uL
        }
        compose.onNodeWithText("Stop capture").assertIsDisplayed().performClick()
        compose.waitUntil(30_000) {
            store.list().singleOrNull()?.orderedCaptureIds?.size == 1
        }
        compose.onNodeWithText("Start capture").assertIsDisplayed()
        compose.onNodeWithText("Stop capture").assertDoesNotExist()
        compose.onNode(
            hasText("No fix recorded (operator stopped).") or hasText("Captured ", substring = true),
        ).assertIsDisplayed()

        val session = store.list().single()
        if (explicitId != null) assertEquals(explicitId, session.sessionId)
        assertEquals(session.sessionId.toString(), runBlocking { prefs.activeSessionIdFlow.first() })
        val captureId = session.orderedCaptureIds.single()
        val captureDir = File(root, "sessions/${session.sessionId}/captures/$captureId")
        assertEquals(listOf(captureId), captureDir.parentFile!!.listFiles()!!.map { it.name })
        // An empty orphan engine-store may exist before Start, but no capture may land there.
        assertTrue(File(root, "sights").walkTopDown().none { it.isFile })
        assertTrue(File(root, "sessions/orphan").walkTopDown().none {
            it.name in setOf("bundle.json", "manifest.json", "pbris.log") || it.name == "captures"
        })
        assertEquals(1, root.walkTopDown().count { it.name == "bundle.json" })
        assertEquals(1, root.walkTopDown().count { it.name == "session.json" })
        val original = captureDir.walkTopDown().filter { it.isFile }
            .associate { it.relativeTo(captureDir).path to it.readBytes() }
        assertTrue(original.keys.containsAll(listOf("bundle.json", "manifest.json", "pbris.log")))
        val bundle = JSONObject(String(original.getValue("bundle.json"), Charsets.UTF_8))
        assertEquals(session.sessionId.toString(), bundle.getString("session_id"))
        assertEquals(captureId, bundle.getString("bundle_id"))
        val timing = bundle.getJSONObject("capture")
        assertTrue(timing.getLong("ended_unix_ms") >= timing.getLong("started_unix_ms"))

        val queue = FfiSubmitterPort.queueRoot(context)
        // Do not call captureCompleted, enqueue, flush, or replace any production callback.
        compose.waitUntil(60_000) {
            File(queue, "sent").listFiles()?.size == 1 &&
                WorkManager.getInstance(context).getWorkInfosForUniqueWork(FlushWorker.NAME)
                    .get(5, TimeUnit.SECONDS).let { work ->
                        work.isNotEmpty() && work.all { it.state.isFinished }
                    }
        }
        val sent = File(queue, "sent").listFiles()!!.single()
        val state = JSONObject(File(sent, "state.json").readText())
        assertTrue(state.getJSONObject("upload_scope").getBoolean("automatic"))
        val collectorId = state.getString("collector_id")
        val deviceUuid = runBlocking { prefs.deviceUuid() }
        val submissions = JSONArray(get("/v1/submissions").toString(Charsets.UTF_8))
        val deviceSubmissions = (0 until submissions.length()).map { submissions.getJSONObject(it) }
            .filter { it.getString("device_uuid") == deviceUuid }
        assertEquals(listOf(collectorId), deviceSubmissions.map { it.getString("id") })
        val manifest = JSONObject(get("/v1/submissions/$collectorId").toString(Charsets.UTF_8))
        val media = manifest.getJSONArray("media")
        // The native payload uses canonical frames and root companions, not the
        // sight-log's local manifest or its legacy media mirror.
        val expected = original.filterKeys {
            it in setOf("bundle.json", "pbris.log", "index.jsonl") || it.startsWith("frames/")
        }.mapKeys { File(it.key).name }
        assertEquals(expected.size, media.length())
        assertEquals(expected.keys, (0 until media.length()).map {
            media.getJSONObject(it).getString("filename")
        }.toSet())
        for ((filename, bytes) in expected) {
            assertArrayEquals("collector changed $filename", bytes,
                get("/v1/submissions/$collectorId/media/$filename"))
            assertArrayEquals("queue changed $filename", bytes, File(sent, "parts/$filename").readBytes())
        }
        assertNull(runBlocking { Upload.settings(context).current().lastError })
        // Leaving/re-entering Live must not finalize or enqueue the completed capture again.
        compose.onNodeWithText("Settings").performClick()
        compose.onNodeWithText("Back").performScrollTo().performClick()
        compose.waitForIdle()
        assertEquals(1, queue.walkTopDown().count { it.name == "state.json" })
        for (directory in listOf("pending", "dead", ".staging")) {
            assertTrue("unexpected $directory entry", File(queue, directory).listFiles().orEmpty().isEmpty())
        }
        assertEquals(listOf(captureId), store.list().single().orderedCaptureIds)
        assertEquals(original.keys, captureDir.walkTopDown().filter { it.isFile }
            .map { it.relativeTo(captureDir).path }.toSet())
        original.forEach { (path, bytes) ->
            assertArrayEquals("capture changed after completion: $path", bytes, File(captureDir, path).readBytes())
        }
    }

    private fun get(path: String): ByteArray = http.newCall(Request.Builder()
        .url(baseUrl + path).header("Authorization", "Bearer $token").build()).execute().use {
        assertEquals("collector GET $path", 200, it.code)
        requireNotNull(it.body).bytes()
    }
}
