package io.github.spencerharmon.bris.upload

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/** Fake Submitter: a queue + scripted flush outcomes. */
private class FakePort : SubmitterPort {
    val queue = mutableListOf<String>()
    val dead = mutableListOf<String>()
    val flushedWith = mutableListOf<CollectorEndpointConfig>()
    var nextOutcome: (String) -> FlushOutcome = { FlushOutcome.Sent(it, "c-$it") }

    private class R(override val kind: String, override val lines: List<ReviewLine>) : PendingReview
    override fun review(bundleDir: String, note: String?): PendingReview =
        R("fix", listOf(ReviewLine("bundle", bundleDir)))
    override fun approveAndEnqueue(review: PendingReview): String =
        "q${queue.size + 1}".also { queue += it }
    override fun flush(endpoint: CollectorEndpointConfig): List<FlushOutcome> {
        flushedWith += endpoint
        return queue.toList().map { id ->
            nextOutcome(id).also {
                when (it) {
                    is FlushOutcome.Sent -> queue.remove(id)
                    is FlushOutcome.DeadLettered -> { queue.remove(id); dead += id }
                    else -> {}
                }
            }
        }
    }
    override fun pendingIds() = queue.toList()
    override fun deadIds() = dead.toList()
}

class UploadCoordinatorTest {
    @get:Rule val tmp = TemporaryFolder()
    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private lateinit var store: CollectorSettingsStore
    private val port = FakePort()
    private lateinit var coord: UploadCoordinator

    @Before fun setUp() {
        store = CollectorSettingsStore(PreferenceDataStoreFactory.create(scope = scope) { File(tmp.root, "s.preferences_pb") })
        coord = UploadCoordinator(port, store) { "uuid-7" }
    }
    @After fun tearDown() { scope.cancel() }

    @Test
    fun reviewDoesNotEnqueue_approveDoes_flushSends() = runBlocking {
        store.save("https://collector.example.com", "enroll")
        store.setDeviceToken("devtok")
        val r = coord.review("/b/1")
        assertEquals("fix", r.kind)
        assertTrue(port.queue.isEmpty())
        assertEquals("q1", coord.approve(r))
        assertEquals(1 to 0, coord.counts())
        val s = coord.flush()
        assertEquals(1, s.sent)
        assertFalse(s.needsRetry)
        assertEquals(CollectorEndpointConfig("https://collector.example.com", "devtok", "uuid-7"), port.flushedWith.single())
        assertEquals(0 to 0, coord.counts())
    }

    @Test
    fun transientFailureNeedsRetryAndRecordsError() = runBlocking {
        store.save("https://collector.example.com", "tok")
        coord.approve(coord.review("/b"))
        port.nextOutcome = { FlushOutcome.Retrying(it, 1) }
        val s = coord.flush()
        assertTrue(s.needsRetry)
        assertEquals(CollectorEndpointConfig("https://collector.example.com", "tok", null), port.flushedWith.single())
        assertTrue(store.current().lastError!!.contains("retry"))
        port.nextOutcome = { FlushOutcome.DeadLettered(it, "HTTP 400") }
        val s2 = coord.flush()
        assertEquals(1, s2.deadAfter)
        assertFalse(s2.needsRetry)
        assertTrue(store.current().lastError!!.contains("HTTP 400"))
    }

    @Test
    fun flushWithoutConfigFails() = runBlocking {
        coord.approve(coord.review("/b"))
        try { coord.flush(); fail("expected NotConfiguredException") } catch (_: NotConfiguredException) {}
        assertTrue(port.flushedWith.isEmpty())
    }
}
