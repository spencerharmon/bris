package io.github.spencerharmon.bris.upload

import android.content.Context
import android.os.Build
import io.github.spencerharmon.bris.BuildConfig
import uniffi.bris_ffi.FfiCollectorEndpoint
import uniffi.bris_ffi.FfiFlushOutcome
import uniffi.bris_ffi.FfiSubmissionReview
import uniffi.bris_ffi.FfiSubmissionSource
import uniffi.bris_ffi.Submitter
import java.io.File
import uniffi.bris_ffi.version as brisFfiVersion

/** Production [SubmitterPort] over the bris-ffi `Submitter`. */
class FfiSubmitterPort(
    queueRoot: File,
    private val deviceUuid: String,
) : SubmitterPort {
    private val submitter = Submitter.open(queueRoot.absolutePath)

    private class Wrapped(val ffi: FfiSubmissionReview) : PendingReview {
        override val kind: String = ffi.kind()
        override val lines: List<ReviewLine> = ffi.lines().map { ReviewLine(it.label, it.value) }
    }

    override fun review(bundleDir: String, note: String?): PendingReview {
        val source = FfiSubmissionSource(
            deviceUuid = deviceUuid,
            deviceModel = Build.MODEL ?: "unknown",
            deviceOs = "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})",
            appVersion = BuildConfig.BRIS_APP_VERSION,
            brisCoreVersion = runCatching { brisFfiVersion().brisFfi }.getOrDefault("unknown"),
            brisDataVersion = null,
            note = note,
        )
        return Wrapped(submitter.review(bundleDir, source, null))
    }

    override fun approveAndEnqueue(review: PendingReview): String =
        submitter.approveAndEnqueue((review as Wrapped).ffi)

    override fun flush(endpoint: CollectorEndpointConfig): List<FlushOutcome> =
        submitter.flush(
            FfiCollectorEndpoint(endpoint.baseUrl, endpoint.bearerToken, endpoint.deviceUuid),
        ).map {
            when (it) {
                is FfiFlushOutcome.Sent -> FlushOutcome.Sent(it.queueId, it.collectorId)
                is FfiFlushOutcome.Retrying -> FlushOutcome.Retrying(it.queueId, it.attempts.toInt())
                is FfiFlushOutcome.DeadLettered -> FlushOutcome.DeadLettered(it.queueId, it.reason)
                is FfiFlushOutcome.Deferred -> FlushOutcome.Deferred(it.queueId)
            }
        }

    override fun pendingIds(): List<String> = submitter.pendingIds()
    override fun deadIds(): List<String> = submitter.deadIds()

    companion object {
        fun queueRoot(context: Context): File = File(context.filesDir, "submission-queue")
    }
}
