package io.github.spencerharmon.bris.ui

import android.Manifest
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.rule.GrantPermissionRule
import androidx.work.WorkInfo
import androidx.work.WorkManager
import io.github.spencerharmon.bris.MainActivity
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.SessionStore
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
import java.util.UUID
import java.util.concurrent.TimeUnit

/** Each concrete class must run in a separate instrumentation process after pm clear. */
abstract class CaptureUploadTest {
    protected val ui = createAndroidComposeRule<MainActivity>()
    @get:Rule val rules: RuleChain = RuleChain.outerRule(
        GrantPermissionRule.grant(Manifest.permission.CAMERA),
    ).around(ui)

    protected fun captureTwice(explicitSession: Boolean) {
        val context = ui.activity.applicationContext
        val prefs = Prefs(context)
        val root = requireNotNull(context.getExternalFilesDir(null))
        val store = SessionStore.forApp(context)
        val args = InstrumentationRegistry.getArguments()
        val url = requireNotNull(args.getString("collectorUrl")) { "Run connected_tests.py" }
        val token = requireNotNull(args.getString("collectorToken"))
        val client = OkHttpClient.Builder().callTimeout(10, TimeUnit.SECONDS).build()
        fun submissions(): JSONArray = client.newCall(Request.Builder()
            .url("$url/v1/submissions").header("Authorization", "Bearer $token").build())
            .execute().use { response ->
                check(response.isSuccessful) { "Collector list HTTP ${response.code}" }
                JSONArray(requireNotNull(response.body).string())
            }
        fun workers() = WorkManager.getInstance(context)
            .getWorkInfosForUniqueWork(FlushWorker.NAME).get(10, TimeUnit.SECONDS)

        assertTrue("Requires fresh app data", store.list().isEmpty())
        assertNull(runBlocking { prefs.activeSessionIdFlow.first() })
        assertFalse(runBlocking { prefs.debugModeFlow.first() })
        assertFalse(runBlocking { Upload.settings(context).current().automaticUpload })
        assertEquals(0, submissions().length())

        ui.onNodeWithText("Settings").performClick()
        ui.onNodeWithTag("debug-mode-switch").performScrollTo().assertIsOff()
        ui.onNodeWithText("Base URL").performScrollTo().performTextReplacement(url)
        ui.onNodeWithText("Token (enrollment or device)").performScrollTo()
            .performTextReplacement(token)
        ui.onNodeWithText("Save & register").performScrollTo().performClick()
        ui.waitUntil(30_000) { runBlocking { Upload.settings(context).current().registered } }
        ui.onNodeWithTag("automatic-upload-switch").performScrollTo().assertIsOff().performClick()
        ui.waitUntil(10_000) { runBlocking { Upload.settings(context).current().automaticUpload } }
        ui.onNodeWithTag("automatic-upload-switch").assertIsOn()
        ui.onNodeWithText("Back").performScrollTo().performClick()
        ui.waitUntil(30_000) { workers().isNotEmpty() && workers().all { it.state.isFinished } }
        assertTrue(workers().all { it.state == WorkInfo.State.SUCCEEDED })
        val initialWorkers = workers().map { it.id }.toSet()
        assertTrue("Settings must not create a session", store.list().isEmpty())
        assertNull(runBlocking { prefs.activeSessionIdFlow.first() })

        var expectedSession: String? = null
        if (explicitSession) {
            ui.onNodeWithText("Sessions").performClick()
            ui.onNodeWithText("+ New session").performClick()
            ui.waitUntil(10_000) {
                ui.onAllNodesWithText("Title").fetchSemanticsNodes().isNotEmpty()
            }
            ui.onNodeWithText("Title").performTextReplacement("Connected explicit session")
            ui.onNodeWithText("Save").performScrollTo().performClick()
            ui.waitUntil(10_000) { store.list().singleOrNull()?.title == "Connected explicit session" }
            expectedSession = store.list().single().sessionId.toString()
            ui.onNodeWithText("Use").performClick()
            ui.waitUntil(10_000) { runBlocking { prefs.activeSessionIdFlow.first() } == expectedSession }
            ui.onNodeWithText("Back").performClick()
        }

        val captureIds = mutableListOf<String>()
        val immutableArtifacts = mutableMapOf<File, ByteArray>()
        repeat(2) { index ->
            ui.onNodeWithText("Start capture").performClick()
            ui.waitUntil(10_000) {
                ui.onAllNodesWithText("Stop capture").fetchSemanticsNodes().isNotEmpty()
            }
            ui.waitUntil(10_000) { runBlocking { prefs.activeSessionIdFlow.first() } != null }
            val boundSession = requireNotNull(runBlocking { prefs.activeSessionIdFlow.first() })
            assertEquals(boundSession, UUID.fromString(boundSession).toString())
            if (expectedSession == null) expectedSession = boundSession
            assertEquals("Session must be bound before Stop", expectedSession, boundSession)
            // Let preference-driven recomposition happen before Stop. The original bug
            // replaces the active recorder here, losing Stop and orphaning the capture.
            Thread.sleep(1_500)
            ui.onNodeWithText("Stop capture").assertIsDisplayed().performClick()
            ui.waitUntil(30_000) {
                store.list().singleOrNull()?.orderedCaptureIds?.size == index + 1
            }
            val session = store.list().single()
            if (expectedSession == null) expectedSession = session.sessionId.toString()
            assertEquals(expectedSession, session.sessionId.toString())
            assertEquals(expectedSession, runBlocking { prefs.activeSessionIdFlow.first() })
            captureIds += session.orderedCaptureIds.last()
            assertEquals(captureIds, session.orderedCaptureIds)
            assertEquals(captureIds.size, captureIds.toSet().size)
            val captures = File(root, "sessions/$expectedSession/captures")
            assertEquals(captureIds.toSet(), captures.listFiles()!!.map { it.name }.toSet())
            val capture = File(captures, captureIds.last())
            for (name in listOf("manifest.json", "bundle.json", "pbris.log")) {
                assertTrue("Missing canonical $name", File(capture, name).isFile)
            }
            val bundle = JSONObject(File(capture, "bundle.json").readText())
            assertEquals(expectedSession, bundle.getString("session_id"))
            assertEquals(captureIds.last(), bundle.getString("bundle_id"))
            assertFalse("Debug OFF must not attach GPS truth", bundle.has("gps_truth"))
            File(capture, "frames").listFiles()?.filter { it.extension == "json" }?.forEach {
                assertEquals("Debug OFF retains only fix frames", "fix_frame",
                    JSONObject(it.readText()).getString("retention"))
            }
            assertFalse("No legacy capture artifacts", File(root, "sights").walkTopDown().any { it.isFile })
            assertFalse("No orphan capture artifacts", File(root, "sessions/orphan/captures").exists())
            immutableArtifacts.forEach { (file, bytes) -> assertArrayEquals(file.path, bytes, file.readBytes()) }
            capture.walkTopDown().filter { it.isFile }.forEach { immutableArtifacts[it] = it.readBytes() }

            ui.waitUntil(60_000) { submissions().length() >= index + 1 }
            ui.waitUntil(30_000) { workers().all { it.state.isFinished } }
            val completed = workers().filter { it.id !in initialWorkers }
            assertEquals("Exactly one scheduled completion effect per capture", index + 1, completed.size)
            assertTrue(completed.all { it.state == WorkInfo.State.SUCCEEDED })
            assertEquals(index + 1, submissions().length())
            assertEquals(0 to 0, runBlocking { Upload.coordinator(context).counts() })
        }
        // Detect delayed duplicate completion work, not just the first successful send.
        Thread.sleep(2_000)
        assertEquals(2, submissions().length())
        assertEquals(2, workers().count { it.id !in initialWorkers })
        immutableArtifacts.forEach { (file, bytes) -> assertArrayEquals(file.path, bytes, file.readBytes()) }
        assertFalse(runBlocking { prefs.debugModeFlow.first() })
        assertTrue(runBlocking { Upload.settings(context).current().automaticUpload })
        assertNull(runBlocking { Upload.settings(context).current().lastError })
        assertEquals(1, store.list().size)
        if (explicitSession) assertEquals("Connected explicit session", store.list().single().title)
        // AGP uninstalls the app after instrumentation, including external files.
        // Export while it is installed; the host validates these exact bytes.
        val output = InstrumentationRegistry.getInstrumentation().uiAutomation
            .executeShellCommand("cp -R ${root.absolutePath} /data/local/tmp/${javaClass.simpleName}")
        android.os.ParcelFileDescriptor.AutoCloseInputStream(output).use {
            assertEquals("Evidence export failed", "", it.bufferedReader().readText())
        }
    }
}

class FirstCaptureUploadTest : CaptureUploadTest() {
    @Test fun freshInstallWithoutVisitingSessionsUploadsBothCaptures() = captureTwice(false)
}

class ExplicitSessionUploadTest : CaptureUploadTest() {
    @Test fun selectedSessionUploadsBothCapturesWithoutCreatingDefault() = captureTwice(true)
}
