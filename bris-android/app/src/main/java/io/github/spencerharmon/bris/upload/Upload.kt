package io.github.spencerharmon.bris.upload

import android.content.Context
import androidx.datastore.preferences.preferencesDataStore
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import io.github.spencerharmon.bris.Prefs
import java.util.concurrent.TimeUnit

private val Context.collectorStore by preferencesDataStore(name = "bris_collector")

/** App-wide wiring for the collector upload path. */
object Upload {
    fun settings(context: Context) = CollectorSettingsStore(context.applicationContext.collectorStore)

    suspend fun coordinator(context: Context): UploadCoordinator {
        val app = context.applicationContext
        val prefs = Prefs(app)
        val uuid = prefs.deviceUuid()
        return UploadCoordinator(
            FfiSubmitterPort(FfiSubmitterPort.queueRoot(app), uuid),
            settings(app),
        ) { uuid }
    }

    /** Schedule a network-constrained, retrying background flush. */
    fun scheduleFlush(context: Context) {
        val req = OneTimeWorkRequestBuilder<FlushWorker>()
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30, TimeUnit.SECONDS)
            .build()
        WorkManager.getInstance(context.applicationContext)
            .enqueueUniqueWork(FlushWorker.NAME, ExistingWorkPolicy.REPLACE, req)
    }
}

/** Background flush of the durable submission queue. */
class FlushWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result = try {
        val summary = Upload.coordinator(applicationContext).flush()
        if (summary.needsRetry) Result.retry() else Result.success()
    } catch (e: NotConfiguredException) {
        Result.failure()
    } catch (t: Throwable) {
        Upload.settings(applicationContext).setLastError("${t.javaClass.simpleName}: ${t.message}")
        Result.retry()
    }

    companion object {
        const val NAME = "bris-collector-flush"
    }
}
