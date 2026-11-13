package io.github.spencerharmon.bris.upload

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
    fun approveAndEnqueue(review: PendingReview): String
    fun flush(endpoint: CollectorEndpointConfig): List<FlushOutcome>
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
    "collector URL/token not configured (Settings \u2192 Debug mode)",
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
    private val deviceUuid: suspend () -> String,
) {
    fun review(bundleDir: String, note: String? = null): PendingReview = port.review(bundleDir, note)

    fun approve(review: PendingReview): String = port.approveAndEnqueue(review)

    suspend fun flush(): FlushSummary {
        val endpoint = settings.current().endpoint(deviceUuid()) ?: run {
            val err = NotConfiguredException()
            settings.setLastError(err.message)
            throw err
        }
        val outcomes = port.flush(endpoint)
        val summary = summarize(outcomes, port.pendingIds().size, port.deadIds().size)
        settings.setLastError(summary.lastError)
        return summary
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
