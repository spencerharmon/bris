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
    private val submissionSource: ((String?) -> FfiSubmissionSource)? = null,
) : SubmitterPort {
    private val submitter = Submitter.open(queueRoot.absolutePath)

    private class Wrapped(val ffi: FfiSubmissionReview) : PendingReview {
        override val kind: String = ffi.kind()
        override val lines: List<ReviewLine> = ffi.lines().map { ReviewLine(it.label, it.value) }
    }

    private fun source(note: String?) = submissionSource?.invoke(note) ?: FfiSubmissionSource(
            deviceUuid = deviceUuid,
            deviceModel = checkNotNull(Build.MODEL),
            deviceOs = "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})",
            appVersion = BuildConfig.BRIS_APP_VERSION,
            brisCoreVersion = brisFfiVersion().brisFfi,
            brisDataVersion = null,
            note = note,
        )
    override fun review(bundleDir: String, note: String?): PendingReview =
        Wrapped(submitter.review(bundleDir, source(note), null))

    override fun configureUploads(manualScope: String?, automaticScope: String?) =
        submitter.configureUploads(manualScope, automaticScope)

    override fun approveScoped(review: PendingReview, scope: String): String =
        submitter.approveScoped((review as Wrapped).ffi, scope)

    override fun enqueueAutomatic(bundleDir: String, scope: String): String =
        submitter.enqueueAutomatic(bundleDir, source(null), scope)

    override fun cancelFlush() = submitter.cancelFlush()

    override fun flushScoped(endpoint: CollectorEndpointConfig, scope: String, automatic: Boolean): List<FlushOutcome> =
        submitter.flushScoped(
            FfiCollectorEndpoint(endpoint.baseUrl, endpoint.bearerToken, endpoint.deviceUuid),
            scope, automatic,
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
