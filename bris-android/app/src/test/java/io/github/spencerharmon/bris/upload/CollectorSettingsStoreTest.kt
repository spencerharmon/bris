package io.github.spencerharmon.bris.upload

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class CollectorSettingsStoreTest {
    @get:Rule val tmp = TemporaryFolder()

    private fun <T> withStore(file: File, block: suspend (CollectorSettingsStore) -> T): T {
        // DataStore allows one active instance per file: join the
        // scope so the next instance in the same test may open it.
        val job = SupervisorJob()
        val scope = CoroutineScope(Dispatchers.IO + job)
        try {
            val ds = PreferenceDataStoreFactory.create(scope = scope) { file }
            return runBlocking { block(CollectorSettingsStore(ds)) }
        } finally {
            runBlocking { job.cancelAndJoin() }
        }
    }

    @Test
    fun unconfiguredByDefault_noCompiledDefaults() {
        val f = File(tmp.root, "a.preferences_pb")
        withStore(f) { s ->
            val c = s.current()
            assertEquals("", c.baseUrl)
            assertEquals("", c.enteredToken)
            assertFalse(c.configured)
            assertNull(c.endpoint("dev"))
        }
    }

    @Test
    fun persistsAcrossStoreInstances() {
        val f = File(tmp.root, "b.preferences_pb")
        withStore(f) { it.save("https://collector.example.com/", "enroll-tok"); it.setDeviceToken("dev-tok") }
        withStore(f) { s ->
            val c = s.current()
            assertEquals("https://collector.example.com", c.baseUrl)
            assertEquals("enroll-tok", c.enteredToken)
            assertTrue(c.registered)
            assertEquals(CollectorEndpointConfig("https://collector.example.com", "dev-tok", "uuid-1"), c.endpoint("uuid-1"))
        }
    }

    @Test
    fun changingTokenInvalidatesDeviceToken() {
        val f = File(tmp.root, "c.preferences_pb")
        withStore(f) { s ->
            s.save("https://collector.example.com", "t1")
            s.setDeviceToken("dev")
            s.save("https://collector.example.com", "t1")
            assertTrue(s.current().registered)
            s.save("https://collector.example.com", "t2")
            val c = s.current()
            assertFalse(c.registered)
            assertEquals(CollectorEndpointConfig("https://collector.example.com", "t2", null), c.endpoint("u"))
        }
    }

    @Test(expected = IllegalArgumentException::class)
    fun rejectsInvalidUrl() {
        withStore(File(tmp.root, "d.preferences_pb")) { it.save("ftp://x", "t") }
    }

    @Test
    fun tokensMaskedInToString() {
        val s = CollectorSettings("https://c.example.com", "secret-enroll-1234", "secret-device-5678").toString()
        assertFalse(s.contains("secret"))
        assertTrue(s.contains("5678"))
    }

    @Test
    fun normalizeBaseUrl() {
        assertEquals("http://192.0.2.1:8080", normalizeBaseUrl(" http://192.0.2.1:8080// "))
        assertNull(normalizeBaseUrl("collector.example.com"))
        assertNull(normalizeBaseUrl(""))
    }
}
