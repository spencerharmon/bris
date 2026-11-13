package io.github.spencerharmon.bris.upload

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/** Models the scoped native contract, not HTTP. Real transport is tested natively. */
internal class FakePort : SubmitterPort {
    data class Entry(val id: String, val scope: String, val automatic: Boolean)
    private val lock = Any()
    private val queue = mutableListOf<Entry>()
    private val dead = mutableListOf<String>()
    private var manual: String? = null
    private var automatic: String? = null
    private var active: Entry? = null
    private var serial = 0
    val cancellations = mutableListOf<Boolean>()
    val flushedWith = mutableListOf<CollectorEndpointConfig>()
    val automaticBundles = mutableListOf<String>()
    val reviews = AtomicInteger()
    val activeFlushes = AtomicInteger()
    val maxFlushes = AtomicInteger()
    var started: CountDownLatch? = null
    var release: CountDownLatch? = null
    var enqueueFailure = false
    var onConfigure: (() -> Unit)? = null
    var nextOutcome: (String) -> FlushOutcome = { FlushOutcome.Sent(it, "c-$it") }

    private class R : PendingReview {
        override val kind = "fix"
        override val lines = listOf(ReviewLine("bundle", "canonical capture"))
    }
    override fun review(bundleDir: String, note: String?): PendingReview {
        reviews.incrementAndGet()
        return R()
    }
    override fun configureUploads(manualScope: String?, automaticScope: String?) = synchronized(lock) {
        manual = manualScope
        automatic = automaticScope
        queue.removeAll { it.scope != if (it.automatic) automatic else manual }
        active?.let {
            if (it.scope != if (it.automatic) automatic else manual) {
                cancellations += it.automatic
                release?.countDown()
            }
        }
        onConfigure?.invoke()
        Unit
    }
    private fun enqueue(scope: String, auto: Boolean): String = synchronized(lock) {
        check(scope == if (auto) automatic else manual)
        check(!enqueueFailure)
        "q${++serial}".also { queue += Entry(it, scope, auto) }
    }
    override fun approveScoped(review: PendingReview, scope: String) = enqueue(scope, false)
    override fun enqueueAutomatic(bundleDir: String, scope: String): String {
        val id = enqueue(scope, true)
        automaticBundles += bundleDir
        return id
    }
    override fun cancelFlush() = synchronized(lock) {
        active?.let { cancellations += it.automatic }
        release?.countDown()
        Unit
    }
    override fun flushScoped(endpoint: CollectorEndpointConfig, scope: String, automatic: Boolean): List<FlushOutcome> {
        val entries = synchronized(lock) {
            if (scope != if (automatic) this.automatic else manual) return emptyList()
            queue.filter { it.scope == scope && it.automatic == automatic }.also {
                if (it.isNotEmpty()) { active = it.first(); flushedWith += endpoint }
            }
        }
        if (entries.isEmpty()) return emptyList()
        val activeCount = activeFlushes.incrementAndGet()
        maxFlushes.updateAndGet { maxOf(it, activeCount) }
        try {
            started?.countDown()
            check(release?.await(5, TimeUnit.SECONDS) != false) { "fake native was not cancelled" }
            return synchronized(lock) {
                entries.filter { it in queue }.map { entry ->
                    nextOutcome(entry.id).also {
                        when (it) {
                            is FlushOutcome.Sent -> queue.remove(entry)
                            is FlushOutcome.DeadLettered -> { queue.remove(entry); dead += entry.id }
                            else -> Unit
                        }
                    }
                }
            }
        } finally {
            synchronized(lock) { active = null }
            activeFlushes.decrementAndGet()
        }
    }
    override fun pendingIds() = synchronized(lock) { queue.map { it.id } }
    override fun deadIds() = synchronized(lock) { dead.toList() }
}

class UploadCoordinatorTest {
    @get:Rule val tmp = TemporaryFolder()
    private var job = SupervisorJob()
    private lateinit var store: CollectorSettingsStore
    private lateinit var file: File
    private val port = FakePort()
    private lateinit var coord: UploadCoordinator
    private val scheduled = AtomicInteger()

    private fun coordinator(registrar: (String, String, String) -> RegistrationResult = { _, _, _ ->
        RegistrationResult.Registered("device-token")
    }) = UploadCoordinator(port, store, { scheduled.incrementAndGet(); Unit }, registrar) { "uuid-7" }

    @Before fun setUp() {
        file = File(tmp.root, "s.preferences_pb")
        openStore()
        coord = coordinator()
    }
    private fun openStore() {
        store = CollectorSettingsStore(PreferenceDataStoreFactory.create(
            scope = CoroutineScope(Dispatchers.IO + job),
        ) { file })
    }
    @After fun tearDown() = runBlocking { job.cancelAndJoin() }

    @Test fun automaticDefaultsOffAndCompletionDoesNothing() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        coord.captureCompleted("/canonical/capture")
        assertFalse(store.current().automaticUpload)
        assertEquals(0, scheduled.get())
        assertEquals(0, port.reviews.get())
        assertTrue(port.pendingIds().isEmpty())
    }

    @Test fun automaticIsSilentAndIndependentOfDebug() = runBlocking {
        coord.saveSettings("https://collector.example.com", "entered-device-token")
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/canonical/capture")
        assertFalse(store.current().debugMode)
        assertEquals(listOf("/canonical/capture"), port.automaticBundles)
        assertEquals(0, port.reviews.get())
        assertEquals(1, scheduled.get())
        assertEquals(1, coord.flush().sent)
        assertEquals(CollectorEndpointConfig("https://collector.example.com", "entered-device-token", "uuid-7"),
            port.flushedWith.single())
    }

    @Test fun automaticErrorsAreSettingsOnly() = runBlocking {
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/canonical/capture")
        assertNotNull(store.current().lastError)
        assertEquals(1, scheduled.get())
        assertEquals(1, port.pendingIds().size)
        coord.saveSettings("https://collector.example.com", "secret-token")
        assertEquals(1, port.pendingIds().size)
        port.enqueueFailure = true
        coord.captureCompleted("/canonical/capture")
        assertNotNull(store.current().lastError)
        assertFalse(store.current().lastError!!.contains("secret-token"))
        assertEquals(0, port.reviews.get())
    }

    @Test fun manualRequiresDebugAndCurrentReview() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        try { coord.review("/capture"); fail("Debug required") } catch (_: IllegalStateException) {}
        coord.setDebugMode(true)
        val review = coord.review("/capture")
        assertTrue(port.pendingIds().isEmpty())
        coord.setDebugMode(false)
        coord.setDebugMode(true)
        try { coord.approve(review); fail("stale review") } catch (_: IllegalStateException) {}
        coord.approve(coord.review("/capture"))
        assertEquals(1, coord.flush().sent)
    }

    @Test fun togglesRevokeOnlyTheirOwnQueue() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        coord.setAutomaticUpload(true)
        coord.setDebugMode(true)
        val manual = coord.approve(coord.review("/manual"))
        coord.captureCompleted("/automatic")
        val autoScope = store.current().automaticScope
        coord.setDebugMode(false)
        assertFalse(port.pendingIds().contains(manual))
        assertEquals(autoScope, store.current().automaticScope)
        assertEquals(1, port.pendingIds().size)
        coord.setDebugMode(true)
        val secondManual = coord.approve(coord.review("/manual"))
        coord.setAutomaticUpload(false)
        assertEquals(listOf(secondManual), port.pendingIds())
    }

    @Test fun autoOffCancelsNativeBeforePersistWithoutWaitingForDrain() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/automatic")
        port.started = CountDownLatch(1)
        port.release = CountDownLatch(1)
        val flushing = async(Dispatchers.Default) { coord.flush() }
        assertTrue(port.started!!.await(5, TimeUnit.SECONDS))
        // The data flow still exposes the old snapshot inside beforePersist.
        port.onConfigure = {
            if (port.cancellations.isNotEmpty()) {
                assertTrue(runBlocking { store.current() }.automaticUpload)
                port.onConfigure = null
            }
        }
        coord.setAutomaticUpload(false)
        port.onConfigure = null
        flushing.await()
        assertEquals(listOf(true), port.cancellations)
        assertFalse(store.current().automaticUpload)
        assertTrue(port.pendingIds().isEmpty())
    }

    @Test fun debugOffDoesNotCancelAutomaticButCredentialsDo() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        coord.setDebugMode(true)
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/automatic")
        port.started = CountDownLatch(1)
        port.release = CountDownLatch(1)
        val flushing = async(Dispatchers.Default) { coord.flush() }
        assertTrue(port.started!!.await(5, TimeUnit.SECONDS))
        coord.setDebugMode(false)
        assertTrue(port.cancellations.isEmpty())
        coord.saveSettings("https://other.example.com", "other-token")
        flushing.await()
        assertEquals(listOf(true), port.cancellations)
        assertTrue(port.pendingIds().isEmpty())
        assertTrue(port.flushedWith.all { it.baseUrl == "https://collector.example.com" })
    }

    @Test fun autoOffDoesNotCancelManualButDebugOffDoes() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        coord.setDebugMode(true)
        coord.setAutomaticUpload(true)
        coord.approve(coord.review("/manual"))
        port.started = CountDownLatch(1)
        port.release = CountDownLatch(1)
        val flushing = async(Dispatchers.Default) { coord.flush() }
        assertTrue(port.started!!.await(5, TimeUnit.SECONDS))
        coord.setAutomaticUpload(false)
        assertTrue(port.cancellations.isEmpty())
        coord.setDebugMode(false)
        flushing.await()
        assertEquals(listOf(false), port.cancellations)
    }

    @Test fun concurrentDrainsSerializeAndWorkerCancellationReachesNative() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/automatic")
        port.started = CountDownLatch(1)
        port.release = CountDownLatch(1)
        val first = launch(Dispatchers.Default) { coord.flush() }
        assertTrue(port.started!!.await(5, TimeUnit.SECONDS))
        val second = async(Dispatchers.Default) { coord.flush() }
        first.cancelAndJoin()
        second.await()
        assertTrue(port.cancellations.contains(true))
        assertEquals(1, port.maxFlushes.get())
    }

    @Test fun retrySurvivesRestartButRevokedGenerationDoesNot() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/automatic")
        port.nextOutcome = { FlushOutcome.Retrying(it, 1) }
        assertTrue(coord.flush().needsRetry)
        val previous = store.current().automaticScope
        job.cancelAndJoin()
        job = SupervisorJob()
        openStore()
        coord = coordinator()
        assertEquals(previous, store.current().automaticScope)
        assertTrue(coord.flush().needsRetry)
        coord.setAutomaticUpload(false)
        coord.setAutomaticUpload(true)
        assertNotEquals(previous, store.current().automaticScope)
        assertFalse(coord.flush().needsRetry)
    }

    @Test fun registrationCannotStaleWriteAcrossAtoBtoA() = runBlocking {
        val started = CountDownLatch(1)
        val release = CountDownLatch(1)
        coord = coordinator { url, token, uuid ->
            assertEquals("https://a.example.com", url)
            assertEquals("token-a", token)
            assertEquals("uuid-7", uuid)
            started.countDown()
            check(release.await(5, TimeUnit.SECONDS))
            RegistrationResult.Registered("old-device-a")
        }
        coord.saveSettings("https://a.example.com", "token-a")
        val initial = store.current().configGeneration
        val registering = async(Dispatchers.Default) { coord.register() }
        assertTrue(started.await(5, TimeUnit.SECONDS))
        coord.saveSettings("https://b.example.com", "token-b")
        assertNull(store.current().deviceToken)
        coord.saveSettings("https://a.example.com", "token-a")
        assertNotEquals(initial, store.current().configGeneration)
        release.countDown()
        assertEquals(RegistrationResult.Superseded, registering.await())
        assertNull(store.current().deviceToken)
    }

    @Test fun registrationPersistsCurrentTokenAndRevokesOldCredentialScope() = runBlocking {
        coord.saveSettings("https://collector.example.com", "enrollment")
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/automatic")
        val oldScope = store.current().automaticScope
        assertEquals(RegistrationResult.Registered("device-token"), coord.register())
        assertEquals("device-token", store.current().deviceToken)
        assertNotEquals(oldScope, store.current().automaticScope)
        assertTrue(port.pendingIds().isEmpty())
        coord.captureCompleted("/new-capture")
        coord.flush()
        assertEquals("device-token", port.flushedWith.single().bearerToken)
    }

    @Test fun registrationsSerializeAndClearingCredentialsRejectsInFlightResult() = runBlocking {
        val started = CountDownLatch(1)
        val release = CountDownLatch(1)
        val calls = AtomicInteger()
        coord = coordinator { _, _, _ ->
            calls.incrementAndGet()
            started.countDown()
            check(release.await(5, TimeUnit.SECONDS))
            RegistrationResult.Registered("old-token")
        }
        coord.saveSettings("https://a.example.com", "enrollment")
        val first = async(Dispatchers.Default) { coord.register() }
        assertTrue(started.await(5, TimeUnit.SECONDS))
        val second = async(Dispatchers.Default) { runCatching { coord.register() } }
        coord.saveSettings("", "")
        release.countDown()
        assertEquals(RegistrationResult.Superseded, first.await())
        assertTrue(second.await().isFailure)
        assertEquals(1, calls.get())
        assertFalse(store.current().configured)
        assertNull(store.current().deviceToken)
    }

    @Test fun deadLetterStopsRetryAndReportsError() = runBlocking {
        coord.saveSettings("https://collector.example.com", "token")
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/automatic")
        port.nextOutcome = { FlushOutcome.DeadLettered(it, "HTTP 400") }
        assertFalse(coord.flush().needsRetry)
        assertTrue(store.current().lastError!!.contains("HTTP 400"))
    }

    @Test fun initialEnrollmentPreservesHeldAutomaticCapturesWithoutEnrollmentCredential() = runBlocking {
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/b/pre-config")
        assertEquals(1, port.pendingIds().size)
        coord.saveSettings("https://collector.example.com", "enroll-admin")
        assertEquals(1, port.pendingIds().size)
        assertTrue(store.current().automaticAwaitingEnrollment)
        // Held: the enrollment/admin credential never submits device data.
        assertEquals(0, coord.flush().sent)
        assertTrue(port.flushedWith.isEmpty())
        coord.captureCompleted("/b/configured-unregistered")
        val before = scheduled.get()
        assertEquals(RegistrationResult.Registered("device-token"), coord.register())
        assertTrue(scheduled.get() > before)
        assertFalse(store.current().automaticAwaitingEnrollment)
        assertEquals(2, port.pendingIds().size)
        assertEquals(2, coord.flush().sent)
        assertEquals(listOf("device-token"), port.flushedWith.map { it.bearerToken })
    }

    @Test fun failedInitialEnrollmentKeepsHoldUntilRetrySucceeds() = runBlocking {
        var result: RegistrationResult = RegistrationResult.Failed("down")
        coord = coordinator { _, _, _ -> result }
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/b/pre")
        coord.saveSettings("https://collector.example.com", "enroll")
        coord.register()
        assertTrue(store.current().automaticAwaitingEnrollment)
        assertEquals(0, coord.flush().sent)
        result = RegistrationResult.Registered("dev")
        coord.register()
        assertEquals(1, coord.flush().sent)
        assertEquals("dev", port.flushedWith.single().bearerToken)
    }

    @Test fun notEnrollmentTokenReleasesHeldCapturesToEnteredDeviceToken() = runBlocking {
        coord = coordinator { _, _, _ -> RegistrationResult.NotEnrollmentToken(401) }
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/b/pre")
        coord.saveSettings("https://collector.example.com", "already-device")
        coord.register()
        assertEquals(1, coord.flush().sent)
        assertEquals("already-device", port.flushedWith.single().bearerToken)
    }

    @Test fun endpointChangeOrToggleOffDuringHoldStillRevokes() = runBlocking {
        coord.setAutomaticUpload(true)
        coord.captureCompleted("/b/pre")
        coord.saveSettings("https://collector.example.com", "enroll")
        coord.saveSettings("https://other.example.com", "enroll")
        assertEquals(0, port.pendingIds().size)
        assertFalse(store.current().automaticAwaitingEnrollment)
        coord.captureCompleted("/b/a")
        coord.saveSettings("", "")
        coord.saveSettings("https://collector.example.com", "enroll")
        coord.setAutomaticUpload(false)
        assertFalse(store.current().automaticAwaitingEnrollment)
        assertEquals(0, port.pendingIds().size)
    }

    @Test fun reRegistrationAfterEnrollmentAndTokenChangeStillRevoke() = runBlocking {
        coord.setAutomaticUpload(true)
        coord.saveSettings("https://collector.example.com", "enroll")
        coord.register()
        coord.captureCompleted("/b/enrolled")
        assertEquals(1, port.pendingIds().size)
        coord.register() // re-registration while enrolled is not initial enrollment
        assertEquals(0, port.pendingIds().size)
        coord.captureCompleted("/b/again")
        coord.saveSettings("https://collector.example.com", "new-token")
        assertEquals(0, port.pendingIds().size)
        assertFalse(store.current().automaticAwaitingEnrollment)
    }
}
