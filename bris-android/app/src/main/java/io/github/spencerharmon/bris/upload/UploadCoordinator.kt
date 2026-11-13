package io.github.spencerharmon.bris.upload

import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

/** One line of the pre-upload review. */
data class ReviewLine(val label: String, val value: String)

/** A prepared (not yet approved) submission. */
interface PendingReview {
    val kind: String
    val lines: List<ReviewLine>
}

/** Per-entry flush outcome (mirrors `FfiFlushOutcome`). */
sealed interface FlushOutcome {
    val queueId: String
    data class Sent(override val queueId: String, val collectorId: String) : FlushOutcome
    data class Retrying(override val queueId: String, val attempts: Int) : FlushOutcome
    data class DeadLettered(override val queueId: String, val reason: String) : FlushOutcome
    data class Deferred(override val queueId: String) : FlushOutcome
}

/**
 * Narrow port over the bris-ffi `Submitter` so the
 * review -> enqueue -> flush flow is unit-testable on the JVM.
 * Production: [FfiSubmitterPort].
 */
interface SubmitterPort {
    fun review(bundleDir: String, note: String?): PendingReview
    fun configureUploads(manualScope: String?, automaticScope: String?)
    fun approveScoped(review: PendingReview, scope: String): String
    fun enqueueAutomatic(bundleDir: String, scope: String): String
    fun flushScoped(endpoint: CollectorEndpointConfig, scope: String, automatic: Boolean): List<FlushOutcome>
    fun cancelFlush()
    fun pendingIds(): List<String>
    fun deadIds(): List<String>
}

/** Aggregate of one flush pass, for UI + worker retry policy. */
data class FlushSummary(
    val sent: Int,
    val retrying: Int,
    val dead: Int,
    val deferred: Int,
    val pendingAfter: Int,
    val deadAfter: Int,
    val lastError: String?,
) {
    /** The background worker should run again. */
    val needsRetry: Boolean get() = pendingAfter > 0
}

/** Outcome of a flush request when the app is not configured. */
class NotConfiguredException : IllegalStateException(
    "collector URL/token not configured (Settings)",
)

/**
 * The operator-gated upload flow:
 *
 *  1. [review] builds the one-screen pre-upload review (no
 *     network, no permission granted).
 *  2. [approve] — ONLY in direct response to the operator
 *     confirming the review — persists it to the durable queue.
 *  3. [flush] drives one pass over the queue against the
 *     runtime-configured endpoint; the background worker calls
 *     this and retries while entries remain pending.
 */
class UploadCoordinator(
    private val port: SubmitterPort,
    private val settings: CollectorSettingsStore,
    private val scheduleFlush: () -> Unit = {},
    private val registerDevice: (String, String, String) -> RegistrationResult = DeviceRegistrar()::register,
    private val deviceUuid: suspend () -> String,
) {
    private val mutation = Mutex()
    private val drain = Mutex()
    private val registration = Mutex()

    private class ScopedReview(val review: PendingReview, val scope: String) : PendingReview by review

    private fun configure(config: CollectorSettings) =
        port.configureUploads(config.manualScope, config.automaticScope)

    private suspend fun mutate(transform: (CollectorSettings) -> CollectorSettings) =
        mutation.withLock {
            // Native revocation is synchronous and precedes the durable settings write.
            withContext(NonCancellable) { settings.update(::configure, transform) }
        }

    suspend fun saveSettings(baseUrl: String, token: String) {
        val url = if (baseUrl.isBlank()) "" else requireNotNull(normalizeBaseUrl(baseUrl)) {
            "invalid collector URL"
        }
        val entered = token.trim()
        mutate {
            if (it.baseUrl == url && it.enteredToken == entered) it
            else {
                // Initial configuration keeps already-authorized automatic captures, but holds
                // them until enrollment; a genuine endpoint/token change revokes them.
                val initial = !it.configured
                val next = it.copy(baseUrl = url, enteredToken = entered, deviceToken = null,
                    automaticEpoch = if (initial) it.automaticEpoch else UUID.randomUUID().toString(),
                    configGeneration = UUID.randomUUID().toString(), lastError = null)
                next.copy(automaticAwaitingEnrollment = initial && next.configured)
            }
        }
    }

    suspend fun setAutomaticUpload(enabled: Boolean) {
        mutate {
            if (it.automaticUpload == enabled) it else it.copy(automaticUpload = enabled,
                automaticEpoch = UUID.randomUUID().toString(), automaticAwaitingEnrollment = false)
        }
    }

    suspend fun setDebugMode(enabled: Boolean, persist: suspend (Boolean) -> Unit = {}) {
        mutation.withLock {
            withContext(NonCancellable) {
                settings.update(::configure) {
                    if (it.debugMode == enabled) it else it.copy(debugMode = enabled,
                        manualEpoch = UUID.randomUUID().toString())
                }
                persist(enabled)
            }
        }
    }

    /** One request at a time; the result belongs to this generation, not merely this URL/token. */
    suspend fun register(): RegistrationResult = registration.withLock {
        val snapshot = mutation.withLock { settings.current() }
        if (snapshot.baseUrl.isBlank() || snapshot.enteredToken.isBlank()) throw NotConfiguredException()
        val uuid = deviceUuid()
        val result = withContext(Dispatchers.IO) {
            registerDevice(snapshot.baseUrl, snapshot.enteredToken, uuid)
        }
        var applied = false
        mutate { current ->
            if (current.configGeneration != snapshot.configGeneration) current else {
                applied = true
                when (result) {
                    // Initial enrollment releases the held pre-configuration captures to the
                    // issued device token; any other re-registration still revokes.
                    is RegistrationResult.Registered -> current.copy(deviceToken = result.deviceToken,
                        automaticEpoch = if (current.automaticAwaitingEnrollment) current.automaticEpoch
                            else UUID.randomUUID().toString(),
                        automaticAwaitingEnrollment = false,
                        configGeneration = UUID.randomUUID().toString(), lastError = null)
                    // The entered token is itself a device token: submitting with it is intended.
                    is RegistrationResult.NotEnrollmentToken ->
                        current.copy(automaticAwaitingEnrollment = false, lastError = null)
                    is RegistrationResult.Failed -> current.copy(lastError = result.message)
                    RegistrationResult.Superseded -> current
                }
            }
        }
        if (!applied) return@withLock RegistrationResult.Superseded
        if (result is RegistrationResult.Registered || result is RegistrationResult.NotEnrollmentToken) scheduleFlush()
        result
    }

    suspend fun review(bundleDir: String, note: String? = null): PendingReview {
        val scope = mutation.withLock {
            val config = settings.current()
            configure(config)
            checkNotNull(config.manualScope) { "manual upload requires Debug mode and collector configuration" }
        }
        return ScopedReview(withContext(Dispatchers.IO) { port.review(bundleDir, note) }, scope)
    }

    suspend fun approve(review: PendingReview): String {
        val scoped = mutation.withLock {
            val config = settings.current()
            configure(config)
            val scoped = review as? ScopedReview ?: error("review was not prepared by the coordinator")
            check(scoped.scope == config.manualScope) { "upload authorization changed; review again" }
            scoped
        }
        val id = withContext(Dispatchers.IO) { port.approveScoped(scoped.review, scoped.scope) }
        scheduleFlush()
        return id
    }

    /** No review or Debug requirement; failures are visible only through Settings. */
    suspend fun captureCompleted(bundleDir: String) {
        val config = mutation.withLock {
            val config = settings.current()
            if (!config.automaticUpload) return
            configure(config)
            config
        }
        try {
            val scope = config.automaticScope ?: throw NotConfiguredException()
            withContext(Dispatchers.IO) { port.enqueueAutomatic(bundleDir, scope) }
            if (!config.configured) settings.update {
                it.copy(lastError = "Automatic upload queued; configure collector URL and token to send.")
            }
            scheduleFlush()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            mutation.withLock {
                settings.update {
                    if (it.configGeneration == config.configGeneration && it.automaticScope == config.automaticScope)
                        it.copy(lastError = "automatic upload could not be queued (${e.javaClass.simpleName})") else it
                }
            }
        }
    }

    suspend fun flush(): FlushSummary = drain.withLock {
        val outcomes = mutableListOf<FlushOutcome>()
        val attemptedScopes = mutableSetOf<String>()
        for (automatic in listOf(false, true)) {
            val config = mutation.withLock {
                settings.current().also(::configure)
            }
            val scope = (if (automatic) config.automaticScope else config.manualScope) ?: continue
            // Never submit held device data with an unverified enrollment/admin credential.
            if (automatic && config.automaticAwaitingEnrollment) continue
            val endpoint = config.endpoint(deviceUuid()) ?: continue
            attemptedScopes += scope
            try {
                outcomes += coroutineScope {
                    val caller = coroutineContext
                    val finished = AtomicBoolean(false)
                    // Cancellation must cross the blocking FFI boundary. Keep the drain
                    // lock until native returns, even if WorkManager stops this worker.
                    val cancellation = launch(Dispatchers.Default, start = CoroutineStart.UNDISPATCHED) {
                        try { awaitCancellation() } finally {
                            // Cancellation can race native's registration of its call.
                            // Keep signalling until the blocking call has really returned.
                            withContext(NonCancellable) {
                                while (!finished.get()) {
                                    port.cancelFlush()
                                    delay(10)
                                }
                            }
                        }
                    }
                    try {
                        withContext(NonCancellable + Dispatchers.IO) {
                            caller.ensureActive()
                            port.flushScoped(endpoint, scope, automatic)
                        }
                    } finally {
                        finished.set(true)
                        withContext(NonCancellable) { cancellation.cancelAndJoin() }
                    }
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                mutation.withLock {
                    settings.update {
                        if (it.configGeneration == config.configGeneration &&
                            (if (automatic) it.automaticScope else it.manualScope) == scope)
                            it.copy(lastError = "upload failed (${e.javaClass.simpleName}); will retry") else it
                    }
                }
                throw e
            }
        }
        mutation.withLock {
            val current = settings.current()
            configure(current)
            val summary = summarize(outcomes, port.pendingIds().size, port.deadIds().size)
            if (attemptedScopes.isNotEmpty() && attemptedScopes.all {
                    it == current.manualScope || it == current.automaticScope
                }) settings.update { it.copy(lastError = summary.lastError) }
            summary
        }
    }

    fun counts(): Pair<Int, Int> = port.pendingIds().size to port.deadIds().size

    companion object {
        fun summarize(outcomes: List<FlushOutcome>, pendingAfter: Int, deadAfter: Int): FlushSummary {
            val lastError = outcomes.filterIsInstance<FlushOutcome.DeadLettered>().lastOrNull()
                ?.let { "rejected ${it.queueId}: ${it.reason}" }
                ?: outcomes.filterIsInstance<FlushOutcome.Retrying>().lastOrNull()
                    ?.let { "transient failure on ${it.queueId} (attempt ${it.attempts}); will retry" }
            return FlushSummary(
                sent = outcomes.count { it is FlushOutcome.Sent },
                retrying = outcomes.count { it is FlushOutcome.Retrying },
                dead = outcomes.count { it is FlushOutcome.DeadLettered },
                deferred = outcomes.count { it is FlushOutcome.Deferred },
                pendingAfter = pendingAfter,
                deadAfter = deadAfter,
                lastError = lastError,
            )
        }
    }
}
